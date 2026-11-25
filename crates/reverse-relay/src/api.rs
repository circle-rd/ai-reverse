//! The OpenAI-compatible surface LiteLLM talks to, forwarded through agent tunnels.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Instant;

use http::{HeaderValue, Method, Request, Response, StatusCode, header};
use http_body_util::BodyExt;
use hyper::body::{Body, Incoming};
use reverse_core::body::{BodyError, ProxyBody, full, guard};
use reverse_core::forward::{
    REQUEST_HEADERS, error_response, filter_headers, is_allowed_route, passthrough_response,
};
use reverse_core::ids::ClientId;
use reverse_core::secret::SecretString;
use reverse_core::wire::TUNNEL_AUTHORITY;

use crate::crypto::secrets_match;
use crate::registry::{Registry, Route};

/// Shared state of the API listener.
pub struct ApiState {
    /// Connected tunnels.
    pub registry: Arc<Registry>,
    /// Bearer key LiteLLM must present.
    pub api_key: SecretString,
    /// How long an agent may take to start answering.
    pub request_timeout: std::time::Duration,
    /// Body stall limit in both directions.
    pub idle_timeout: std::time::Duration,
    /// Largest request body accepted.
    pub max_request_bytes: usize,
}

/// Routes `/{client_id}/v1/...` to a matching agent tunnel.
pub async fn handle(
    request: Request<Incoming>,
    state: Arc<ApiState>,
) -> Result<Response<ProxyBody>, Infallible> {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();

    if method == Method::GET && path == "/healthz" {
        return Ok(Response::new(full("ok")));
    }
    let authorized = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|token| secrets_match(token.trim(), state.api_key.expose()));
    if !authorized {
        return Ok(error_response(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "missing or invalid api key",
        ));
    }
    let Some((client, route)) = split_route(&path) else {
        return Ok(error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            "not found",
        ));
    };
    if !is_allowed_route(&method, &route) {
        return Ok(error_response(
            StatusCode::NOT_FOUND,
            "route_not_allowed",
            "route is not served by the relay",
        ));
    }

    let started = Instant::now();
    let response = forward(&state, request, method.clone(), &client, &route).await;
    tracing::info!(
        client_id = %client,
        method = %method,
        route = %route,
        status = response.status().as_u16(),
        elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "request.forwarded"
    );
    Ok(response)
}

fn split_route(path: &str) -> Option<(ClientId, String)> {
    let (client, rest) = path.strip_prefix('/')?.split_once('/')?;
    Some((ClientId::parse(client).ok()?, format!("/{rest}")))
}

async fn forward(
    state: &ApiState,
    request: Request<Incoming>,
    method: Method,
    client: &ClientId,
    route: &str,
) -> Response<ProxyBody> {
    if !state.registry.connected(client) {
        return unavailable();
    }
    let (parts, body) = request.into_parts();
    let headers = filter_headers(&parts.headers, &REQUEST_HEADERS);
    let (body, model) = if method == Method::POST && !body.is_end_stream() {
        match read_for_model(state, body).await {
            Ok(prepared) => prepared,
            Err(response) => return response,
        }
    } else {
        (
            guard(body, state.idle_timeout, Some(state.max_request_bytes)),
            None,
        )
    };
    let Some(Route {
        mut sender,
        node_id,
        inflight: _inflight,
    }) = state.registry.route(client, model.as_deref())
    else {
        return unavailable();
    };
    let built = Request::builder()
        .method(method)
        .uri(format!("http://{TUNNEL_AUTHORITY}{route}"))
        .body(body);
    let mut outgoing = match built {
        Ok(request) => request,
        Err(error) => {
            tracing::error!(%error, "request.build_failed");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "internal error",
            );
        }
    };
    *outgoing.headers_mut() = headers;

    match tokio::time::timeout(state.request_timeout, sender.send_request(outgoing)).await {
        Err(_) => error_response(
            StatusCode::GATEWAY_TIMEOUT,
            "agent_timeout",
            "the agent did not answer in time",
        ),
        Ok(Err(error)) => {
            tracing::warn!(client_id = %client, node_id = %node_id, %error, "tunnel.request_failed");
            error_response(
                StatusCode::BAD_GATEWAY,
                "agent_unavailable",
                "the agent connection failed",
            )
        }
        Ok(Ok(response)) => passthrough_response(response, state.idle_timeout),
    }
}

/// Reads a POST body whole, bounded, to name the model the caller wants.
async fn read_for_model(
    state: &ApiState,
    body: Incoming,
) -> Result<(ProxyBody, Option<String>), Response<ProxyBody>> {
    let limited = guard(body, state.idle_timeout, Some(state.max_request_bytes));
    match limited.collect().await {
        Ok(collected) => {
            let bytes = collected.to_bytes();
            let model = serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|value| value.get("model")?.as_str().map(str::to_owned));
            Ok((full(bytes), model))
        }
        Err(BodyError::TooLarge(_)) => Err(error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            "the request body is too large",
        )),
        Err(error) => {
            tracing::warn!(%error, "request.body_failed");
            Err(error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "the request body could not be read",
            ))
        }
    }
}

fn unavailable() -> Response<ProxyBody> {
    let mut response = error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "agent_unavailable",
        "no agent is connected for this client",
    );
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_client_and_route() {
        let (client, route) = split_route("/tester/v1/chat/completions").unwrap();
        assert_eq!(client.as_str(), "tester");
        assert_eq!(route, "/v1/chat/completions");
    }

    #[test]
    fn rejects_malformed_prefixes() {
        assert!(split_route("/tester").is_none());
        assert!(split_route("/").is_none());
        assert!(split_route("/Tester/v1/models").is_none());
        assert!(split_route(&format!("/{}/v1/models", "x".repeat(66))).is_none());
    }

    #[test]
    fn splits_account_prefixes() {
        let (client, route) = split_route("/u-b_mz/v1/models").unwrap();
        assert_eq!(client.as_str(), "u-b_mz");
        assert_eq!(route, "/v1/models");
    }
}
