//! Serves the allowlisted OpenAI routes through the tunnel.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};

use http::{Method, Request, Response, StatusCode, header};
use hyper::body::{Body, Incoming};
use reverse_core::body::{ProxyBody, empty, full, guard};
use reverse_core::forward::{
    REQUEST_HEADERS, error_response, filter_headers, is_allowed_route, passthrough_response,
};
use reverse_core::wire::{AGENT_INFO_PATH, AGENT_MODELS_PATH, AgentInfo, ModelsSnapshot};
use tokio::sync::watch;

use crate::error::UpstreamError;
use crate::upstream::Upstream;

/// Shared state of every request handler.
pub struct AgentState {
    /// The local engine.
    pub upstream: Arc<Upstream>,
    /// Latest model snapshot.
    pub models: watch::Receiver<ModelsSnapshot>,
    /// Facts about this process and its host.
    pub info: AgentInfo,
    /// Largest request body accepted.
    pub max_request_bytes: usize,
    /// Body stall limit in both directions.
    pub idle_timeout: Duration,
}

/// Routes one request received through the tunnel; failures become responses, never errors.
pub async fn handle(
    request: Request<Incoming>,
    state: Arc<AgentState>,
) -> Result<Response<ProxyBody>, Infallible> {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();

    if method == Method::GET && path == AGENT_MODELS_PATH {
        return Ok(json_response(&state.models.borrow().clone()));
    }
    if method == Method::GET && path == AGENT_INFO_PATH {
        return Ok(json_response(&state.info));
    }
    if !is_allowed_route(&method, &path) {
        return Ok(error_response(
            StatusCode::NOT_FOUND,
            "route_not_allowed",
            "route is not served by this agent",
        ));
    }

    let started = Instant::now();
    let response = proxy(&state, request, method.clone(), &path).await;
    tracing::info!(
        method = %method,
        path = %path,
        status = response.status().as_u16(),
        elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "request.proxied"
    );
    Ok(response)
}

fn json_response<T: serde::Serialize>(value: &T) -> Response<ProxyBody> {
    match serde_json::to_vec(value) {
        Ok(json) => {
            let mut response = Response::new(full(json));
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/json"),
            );
            response
        }
        Err(error) => {
            tracing::error!(%error, "agent.serialize_failed");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "internal error",
            )
        }
    }
}

async fn proxy(
    state: &AgentState,
    request: Request<Incoming>,
    method: Method,
    path: &str,
) -> Response<ProxyBody> {
    let (parts, body) = request.into_parts();
    let headers = filter_headers(&parts.headers, &REQUEST_HEADERS);
    let body = if body.is_end_stream() {
        empty()
    } else {
        guard(body, state.idle_timeout, Some(state.max_request_bytes))
    };

    match state.upstream.send(method, path, headers, body).await {
        Ok(response) => {
            // A rejected engine key is the agent owner's problem, not the caller's.
            if matches!(
                response.status(),
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
            ) {
                tracing::error!(
                    status = response.status().as_u16(),
                    "upstream.credentials_rejected"
                );
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    "upstream_credentials_rejected",
                    "the inference engine rejected the agent credentials",
                );
            }
            passthrough_response(response, state.idle_timeout)
        }
        Err(error) => {
            tracing::warn!(%error, "upstream.failed");
            match error {
                UpstreamError::Timeout(_) => error_response(
                    StatusCode::GATEWAY_TIMEOUT,
                    "upstream_timeout",
                    "the inference engine did not answer in time",
                ),
                _ => error_response(
                    StatusCode::BAD_GATEWAY,
                    "upstream_unavailable",
                    "the inference engine is unavailable",
                ),
            }
        }
    }
}
