//! Accepts agent tunnels: HTTP/1.1 upgrade, verified token, then HTTP/2 to the agent.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use http::{HeaderMap, Method, Request, Response, StatusCode, header};
use hyper::body::Incoming;
use hyper::client::conn::http2;
use hyper::upgrade::Upgraded;
use hyper_util::rt::{TokioExecutor, TokioTimer};
use reverse_core::body::{ProxyBody, empty};
use reverse_core::forward::error_response;
use reverse_core::shutdown::triggered;
use reverse_core::wire::{TUNNEL_PATH, TUNNEL_PROTOCOL};
use tokio::sync::{Semaphore, watch};

use crate::auth::{AuthState, Identity};
use crate::error::TunnelError;
use crate::registry::Registry;
use crate::watcher;

const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(15);
const KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(10);

/// Shared state of the tunnel listener.
pub struct TunnelState {
    /// Where accepted tunnels register.
    pub registry: Arc<Registry>,
    /// Verifies the access token presented at the upgrade.
    pub auth: Arc<AuthState>,
    /// Interval between model polls of each agent.
    pub models_poll: Duration,
    /// Bound on one model poll.
    pub poll_timeout: Duration,
    /// Cap on concurrently upgraded tunnels.
    pub limit: Arc<Semaphore>,
    /// Stops every tunnel on shutdown.
    pub shutdown: watch::Receiver<bool>,
}

fn wants_tunnel_upgrade(headers: &HeaderMap) -> bool {
    let protocol = headers.get(header::UPGRADE).is_some_and(|v| {
        v.as_bytes()
            .eq_ignore_ascii_case(TUNNEL_PROTOCOL.as_bytes())
    });
    let connection = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|token| token.trim().eq_ignore_ascii_case("upgrade"));
    protocol && connection
}

/// Handles the upgrade request of one agent.
pub async fn handle(
    mut request: Request<Incoming>,
    state: Arc<TunnelState>,
    peer: SocketAddr,
) -> Result<Response<ProxyBody>, Infallible> {
    if request.method() != Method::GET || request.uri().path() != TUNNEL_PATH {
        return Ok(error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            "not found",
        ));
    }
    if !wants_tunnel_upgrade(request.headers()) {
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            "upgrade_required",
            "this endpoint only speaks the reverse tunnel protocol",
        ));
    }
    let identity = match state.auth.authenticate(request.headers()).await {
        Ok(identity) => identity,
        Err(error) => {
            tracing::warn!(%peer, reason = %error, "tunnel.rejected_credentials");
            let mut response = error_response(
                StatusCode::UNAUTHORIZED,
                "invalid_token",
                "the access token is missing, invalid, expired or lacks the tunnel scope",
            );
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                header::HeaderValue::from_static("Bearer"),
            );
            return Ok(response);
        }
    };
    let Ok(permit) = state.limit.clone().try_acquire_owned() else {
        tracing::warn!(%peer, "tunnel.rejected_capacity");
        return Ok(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "capacity",
            "too many tunnels",
        ));
    };

    let on_upgrade = hyper::upgrade::on(&mut request);
    // The task owns the permit and ends with the tunnel or on shutdown.
    tokio::spawn(async move {
        let _permit = permit;
        tracing::info!(
            user = %identity.username,
            node_id = %identity.node_id(),
            %peer,
            "tunnel.accepted"
        );
        match on_upgrade.await {
            Ok(upgraded) => match run_tunnel(upgraded, &identity, &state).await {
                Ok(()) => tracing::info!(user = %identity.username, %peer, "tunnel.closed"),
                Err(error) => {
                    tracing::warn!(user = %identity.username, %peer, %error, "tunnel.failed");
                }
            },
            Err(error) => tracing::warn!(%peer, %error, "tunnel.upgrade_failed"),
        }
    });

    let mut response = Response::new(empty());
    *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    response.headers_mut().insert(
        header::CONNECTION,
        header::HeaderValue::from_static("upgrade"),
    );
    response.headers_mut().insert(
        header::UPGRADE,
        header::HeaderValue::from_static(TUNNEL_PROTOCOL),
    );
    Ok(response)
}

async fn run_tunnel(
    upgraded: Upgraded,
    identity: &Identity,
    state: &TunnelState,
) -> Result<(), TunnelError> {
    let (sender, connection) = http2::Builder::new(TokioExecutor::new())
        .timer(TokioTimer::new())
        .keep_alive_interval(KEEP_ALIVE_INTERVAL)
        .keep_alive_timeout(KEEP_ALIVE_TIMEOUT)
        .keep_alive_while_idle(true)
        .adaptive_window(true)
        .handshake::<_, ProxyBody>(upgraded)
        .await
        .map_err(TunnelError::Handshake)?;

    let node_id = identity.node_id();
    let lease = state
        .registry
        .attach(identity.username.clone(), node_id.clone(), sender.clone());
    tracing::info!(user = %identity.username, node_id = %node_id, "tunnel.registered");
    let watcher = watcher::run(
        sender,
        identity.username.clone(),
        node_id,
        state.registry.clone(),
        state.models_poll,
        state.poll_timeout,
    );
    let mut shutdown = state.shutdown.clone();
    tokio::select! {
        result = connection => result.map_err(TunnelError::Connection),
        () = watcher => Ok(()),
        () = lease.replaced() => Ok(()),
        () = triggered(&mut shutdown) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                http::HeaderName::from_static(name),
                header::HeaderValue::from_static(value),
            );
        }
        map
    }

    #[test]
    fn upgrade_headers_must_name_the_protocol_and_the_connection_token() {
        assert!(wants_tunnel_upgrade(&headers(&[
            ("upgrade", "reverse-h2/1"),
            ("connection", "keep-alive, Upgrade"),
        ])));
        assert!(!wants_tunnel_upgrade(&headers(&[(
            "upgrade",
            "reverse-h2/1"
        )])));
        assert!(!wants_tunnel_upgrade(&headers(&[
            ("upgrade", "websocket"),
            ("connection", "upgrade"),
        ])));
    }
}
