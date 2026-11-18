//! Agent entrypoint: configuration, logging and signal handling only.

use std::process::ExitCode;

use reverse_agent::Config;
use reverse_core::{config::process_env, logging, shutdown};

fn main() -> ExitCode {
    logging::init();

    let config = match Config::from_env(&process_env()) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("reverse-agent: configuration error: {error}");
            return ExitCode::from(2);
        }
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("reverse-agent: cannot start the async runtime: {error}");
            return ExitCode::from(1);
        }
    };

    runtime.block_on(async move {
        let (stop, stopped) = shutdown::channel();
        let agent = tokio::spawn(reverse_agent::run(config, stopped));
        shutdown::wait_for_signal().await;
        tracing::info!("agent.stopping");
        // Receivers also resolve when the sender drops, so a failed send is harmless.
        let _ = stop.send(true);
        if let Err(error) = agent.await {
            tracing::error!(%error, "agent.crashed");
        }
    });
    ExitCode::SUCCESS
}
