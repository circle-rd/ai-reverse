//! Relay entrypoint: configuration, logging, signal handling and the container healthcheck.

use std::net::{SocketAddr, TcpStream};
use std::process::ExitCode;
use std::time::Duration;

use reverse_core::{config::process_env, logging, shutdown};
use reverse_relay::{Config, Relay};

const HEALTHCHECK_TIMEOUT: Duration = Duration::from_secs(2);

fn main() -> ExitCode {
    let config = match Config::from_env(&process_env()) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("reverse-relay: configuration error: {error}");
            return ExitCode::from(2);
        }
    };

    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        return healthcheck(config.api_listen);
    }

    logging::init();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("reverse-relay: cannot start the async runtime: {error}");
            return ExitCode::from(1);
        }
    };

    runtime.block_on(async move {
        let relay = match Relay::bind(config).await {
            Ok(relay) => relay,
            Err(error) => {
                eprintln!("reverse-relay: {error}");
                return ExitCode::from(1);
            }
        };
        let (stop, stopped) = shutdown::channel();
        let serving = tokio::spawn(relay.serve(stopped));
        shutdown::wait_for_signal().await;
        tracing::info!("relay.stopping");
        // Receivers also resolve when the sender drops, so a failed send is harmless.
        let _ = stop.send(true);
        if let Err(error) = serving.await {
            tracing::error!(%error, "relay.crashed");
            return ExitCode::from(1);
        }
        ExitCode::SUCCESS
    })
}

/// TCP probe of the local API listener, usable from a `scratch` image.
fn healthcheck(api_listen: SocketAddr) -> ExitCode {
    let target = if api_listen.ip().is_unspecified() {
        SocketAddr::from(([127, 0, 0, 1], api_listen.port()))
    } else {
        api_listen
    };
    match TcpStream::connect_timeout(&target, HEALTHCHECK_TIMEOUT) {
        Ok(_) => ExitCode::SUCCESS,
        Err(_) => ExitCode::from(1),
    }
}
