//! Reverse tunnel relay: terminates agent tunnels and registers their models in LiteLLM.

pub mod api;
pub mod auth;
pub mod config;
pub mod crypto;
pub mod error;
pub mod limiter;
pub mod litellm;
pub mod reconcile;
pub mod registry;
pub mod server;
pub mod tunnel_server;
pub mod watcher;

use std::net::SocketAddr;
use std::sync::Arc;

use reverse_core::wire::TUNNEL_PATH;
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, watch};

pub use config::Config;
pub use error::RelayError;

use crate::api::ApiState;
use crate::auth::AuthState;
use crate::limiter::{FailureLimiter, LimiterConfig};
use crate::litellm::LiteLlm;
use crate::reconcile::Timing;
use crate::registry::Registry;
use crate::tunnel_server::TunnelState;

/// A relay with its listeners bound, ready to serve.
pub struct Relay {
    config: Config,
    auth: AuthState,
    litellm: LiteLlm,
    tunnel_listener: TcpListener,
    api_listener: TcpListener,
}

impl Relay {
    /// Binds both listeners and builds the outbound clients.
    pub async fn bind(config: Config) -> Result<Self, RelayError> {
        let tunnel_listener = bind(config.tunnel_listen, "tunnel").await?;
        let api_listener = bind(config.api_listen, "api").await?;
        let litellm = LiteLlm::new(&config).map_err(RelayError::LiteLlm)?;
        let auth = AuthState::new(&config).map_err(RelayError::Auth)?;
        Ok(Self {
            config,
            auth,
            litellm,
            tunnel_listener,
            api_listener,
        })
    }

    /// The address agents dial.
    pub fn tunnel_addr(&self) -> std::io::Result<SocketAddr> {
        self.tunnel_listener.local_addr()
    }

    /// The address LiteLLM calls.
    pub fn api_addr(&self) -> std::io::Result<SocketAddr> {
        self.api_listener.local_addr()
    }

    /// Serves until `shutdown` flips to `true`, then removes the deployments it registered.
    pub async fn serve(self, shutdown: watch::Receiver<bool>) {
        let config = Arc::new(self.config);
        let registry = Registry::new();

        let tunnel_state = Arc::new(TunnelState {
            registry: registry.clone(),
            auth: Arc::new(self.auth),
            limiter: Arc::new(FailureLimiter::new(LimiterConfig {
                threshold: config.auth_failure_threshold,
                window: config.auth_failure_window,
                lockout: config.auth_lockout,
            })),
            models_poll: config.models_poll,
            poll_timeout: config.request_timeout.min(config.models_poll * 2),
            limit: Arc::new(Semaphore::new(config.max_tunnels)),
            max_tunnels_per_user: config.max_tunnels_per_user,
            trust_forwarded_for: config.trust_forwarded_for,
            shutdown: shutdown.clone(),
        });
        let api_state = Arc::new(ApiState {
            registry: registry.clone(),
            api_key: config.api_key.clone(),
            request_timeout: config.request_timeout,
            idle_timeout: config.idle_timeout,
            max_request_bytes: config.max_request_bytes,
        });
        let timing = Timing {
            interval: config.reconcile_interval,
            resync: config.resync_interval,
            grace: config.deregister_grace,
        };

        tracing::info!(
            tunnel = ?self.tunnel_listener.local_addr().ok(),
            api = ?self.api_listener.local_addr().ok(),
            "relay.started"
        );
        tokio::join!(
            server::serve(
                self.tunnel_listener,
                "tunnel",
                shutdown.clone(),
                move |peer| {
                    let tunnel = tunnel_state.clone();
                    move |request: hyper::Request<hyper::body::Incoming>| {
                        let tunnel = tunnel.clone();
                        async move {
                            if request.uri().path() == TUNNEL_PATH {
                                tunnel_server::handle(request, tunnel, peer).await
                            } else {
                                Ok(reverse_core::forward::error_response(
                                    http::StatusCode::NOT_FOUND,
                                    "not_found",
                                    "not found",
                                ))
                            }
                        }
                    }
                }
            ),
            server::serve(self.api_listener, "api", shutdown.clone(), move |_peer| {
                let state = api_state.clone();
                move |request| api::handle(request, state.clone())
            }),
            reconcile::run(registry, self.litellm, timing, shutdown),
        );
        tracing::info!("relay.stopped");
    }
}

async fn bind(addr: SocketAddr, what: &'static str) -> Result<TcpListener, RelayError> {
    TcpListener::bind(addr)
        .await
        .map_err(|source| RelayError::Bind { what, addr, source })
}
