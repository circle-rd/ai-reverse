//! Dials the relay and serves HTTP/2 on the upgraded connection.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{Method, Request, StatusCode, header};
use http_body_util::Empty;
use hyper::body::Incoming;
use hyper::server::conn::http2;
use hyper::service::service_fn;
use hyper::upgrade::Upgraded;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use reverse_core::secret::SecretString;
use reverse_core::shutdown::triggered;
use reverse_core::wire::{TUNNEL_PATH, TUNNEL_PROTOCOL};
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio_rustls::TlsConnector;

use crate::config::Config;
use crate::error::TunnelError;
use crate::proxy::{AgentState, handle};
use crate::{credential, enroll, tls};

const STABLE_AFTER: Duration = Duration::from_secs(10);
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(15);
const KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CONCURRENT_STREAMS: u32 = 256;

/// Keeps a tunnel open until shutdown, reconnecting with exponential backoff.
///
/// Without a configured token the agent pairs with the relay first, and pairs again whenever
/// the relay stops accepting its saved credential.
pub async fn run(config: Arc<Config>, state: Arc<AgentState>, mut shutdown: watch::Receiver<bool>) {
    let tls = if config.relay.scheme() == &http::uri::Scheme::HTTPS {
        match tls::connector() {
            Ok(connector) => Some(connector),
            Err(error) => {
                tracing::error!(%error, "tls.setup_failed");
                return;
            }
        }
    } else {
        None
    };
    let mut token = match &config.token {
        Some(token) => Some(token.clone()),
        None => credential::load(&config.credential_file).unwrap_or_else(|error| {
            tracing::warn!(%error, "credential.unreadable");
            None
        }),
    };

    let mut delay = config.reconnect_min;
    loop {
        let current = match token.clone() {
            Some(current) => current,
            None => match enroll::acquire(&config, &mut shutdown).await {
                Some(fresh) => {
                    token = Some(fresh.clone());
                    fresh
                }
                None => return,
            },
        };

        let started = Instant::now();
        let dialed = tokio::select! {
            result = dial(&config, tls.as_ref(), &current) => result,
            () = triggered(&mut shutdown) => return,
        };
        match dialed {
            Ok(upgraded) => {
                tracing::info!(relay = %config.relay.authority(), "tunnel.connected");
                match serve(upgraded, state.clone(), shutdown.clone()).await {
                    Ok(()) => tracing::info!("tunnel.closed"),
                    Err(error) => tracing::warn!(%error, "tunnel.failed"),
                }
            }
            Err(TunnelError::Refused(StatusCode::UNAUTHORIZED)) if config.token.is_none() => {
                tracing::warn!("credential.rejected");
                credential::forget(&config.credential_file);
                token = None;
                continue;
            }
            Err(error) => tracing::warn!(%error, "tunnel.dial_failed"),
        }
        if *shutdown.borrow() {
            return;
        }
        if started.elapsed() >= STABLE_AFTER {
            delay = config.reconnect_min;
        }
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            () = triggered(&mut shutdown) => return,
        }
        delay = (delay * 2).min(config.reconnect_max);
    }
}

async fn bounded<T, E>(
    limit: Duration,
    work: impl Future<Output = Result<T, E>>,
    map: impl FnOnce(E) -> TunnelError,
) -> Result<T, TunnelError> {
    match tokio::time::timeout(limit, work).await {
        Err(_) => Err(TunnelError::Timeout(limit)),
        Ok(result) => result.map_err(map),
    }
}

async fn dial(
    config: &Config,
    tls: Option<&TlsConnector>,
    token: &SecretString,
) -> Result<Upgraded, TunnelError> {
    let limit = config.connect_timeout;
    let stream = bounded(
        limit,
        TcpStream::connect((config.relay.host(), config.relay.port())),
        TunnelError::Connect,
    )
    .await?;
    stream.set_nodelay(true).map_err(TunnelError::Connect)?;

    match tls {
        None => upgrade(stream, config, token).await,
        Some(connector) => {
            let name = ServerName::try_from(config.relay.host().to_owned())
                .map_err(|_| TunnelError::ServerName)?;
            let secured = bounded(limit, connector.connect(name, stream), TunnelError::Tls).await?;
            upgrade(secured, config, token).await
        }
    }
}

async fn upgrade<S>(
    stream: S,
    config: &Config,
    token: &SecretString,
) -> Result<Upgraded, TunnelError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let limit = config.connect_timeout;
    let (mut sender, connection) = bounded(
        limit,
        hyper::client::conn::http1::handshake::<_, Empty<Bytes>>(TokioIo::new(stream)),
        TunnelError::Handshake,
    )
    .await?;
    // The connection task ends by itself once the upgrade completes or the socket closes.
    tokio::spawn(connection.with_upgrades());

    let request = Request::builder()
        .method(Method::GET)
        .uri(TUNNEL_PATH)
        .header(header::HOST, config.relay.authority().as_str())
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, TUNNEL_PROTOCOL)
        .header(
            header::AUTHORIZATION,
            token.bearer_header().map_err(TunnelError::Credential)?,
        )
        .body(Empty::<Bytes>::new())?;
    let response = bounded(limit, sender.send_request(request), TunnelError::Handshake).await?;

    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        return Err(TunnelError::Refused(response.status()));
    }
    let agreed = response
        .headers()
        .get(header::UPGRADE)
        .is_some_and(|value| {
            value
                .as_bytes()
                .eq_ignore_ascii_case(TUNNEL_PROTOCOL.as_bytes())
        });
    if !agreed {
        return Err(TunnelError::Protocol);
    }
    hyper::upgrade::on(response)
        .await
        .map_err(TunnelError::Upgrade)
}

async fn serve(
    upgraded: Upgraded,
    state: Arc<AgentState>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), TunnelError> {
    let service = service_fn(move |request: Request<Incoming>| {
        let state = state.clone();
        async move { handle(request, state).await }
    });
    let connection = http2::Builder::new(TokioExecutor::new())
        .timer(TokioTimer::new())
        .keep_alive_interval(KEEP_ALIVE_INTERVAL)
        .keep_alive_timeout(KEEP_ALIVE_TIMEOUT)
        .max_concurrent_streams(MAX_CONCURRENT_STREAMS)
        .adaptive_window(true)
        .serve_connection(upgraded, service);
    tokio::pin!(connection);

    tokio::select! {
        result = connection.as_mut() => result.map_err(TunnelError::Serve),
        () = triggered(&mut shutdown) => {
            connection.as_mut().graceful_shutdown();
            connection.await.map_err(TunnelError::Serve)
        }
    }
}
