//! What may cross the tunnel: allowlisted routes, headers and error shapes.

use std::time::Duration;

use http::{HeaderMap, HeaderName, Method, Response, StatusCode, header};
use hyper::body::Incoming;
use serde_json::json;

use crate::body::{ProxyBody, full, guard};

/// Correlation id forwarded end to end when the caller sets one.
pub const X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

/// Request headers forwarded to the engine; credentials are deliberately absent.
pub const REQUEST_HEADERS: [HeaderName; 4] = [
    header::CONTENT_TYPE,
    header::CONTENT_LENGTH,
    header::ACCEPT,
    X_REQUEST_ID,
];

/// Response headers forwarded back to the caller.
pub const RESPONSE_HEADERS: [HeaderName; 3] =
    [header::CONTENT_TYPE, header::CACHE_CONTROL, X_REQUEST_ID];

/// Whether an OpenAI-compatible route may be proxied. Queries are never forwarded.
///
/// Video follows the OpenAI Videos surface as LiteLLM proxies it (`/v1/videos`, its
/// `{id}`, `{id}/content`, `{id}/remix`, `edits`, `extensions` and `characters`), which is
/// also the surface the Alzeo video lane serves. Realtime websockets are not forwarded.
#[must_use]
pub fn is_allowed_route(method: &Method, path: &str) -> bool {
    match *method {
        Method::GET => path == "/v1/models" || is_video_read(path),
        Method::POST => {
            matches!(
                path,
                "/v1/chat/completions"
                    | "/v1/completions"
                    | "/v1/embeddings"
                    | "/v1/embedding"
                    | "/v1/responses"
                    | "/v1/messages"
                    | "/v1/images/generations"
                    | "/v1/audio/transcriptions"
                    | "/v1/audio/speech"
                    | "/v1/videos"
                    | "/v1/videos/edits"
                    | "/v1/videos/extensions"
                    | "/v1/videos/characters"
            ) || is_video_remix(path)
        }
        _ => false,
    }
}

/// A video or character identifier: one path segment, never a reserved word.
fn is_object_id(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= 128
        && !matches!(segment, "characters" | "edits" | "extensions")
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b':'))
}

/// `GET /v1/videos`, `/v1/videos/{id}`, `/v1/videos/{id}/content`, `/v1/videos/characters/{id}`.
fn is_video_read(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/v1/videos") else {
        return false;
    };
    if rest.is_empty() {
        return true;
    }
    let Some(tail) = rest.strip_prefix('/') else {
        return false;
    };
    match tail.split_once('/') {
        None => is_object_id(tail),
        Some((id, "content")) => is_object_id(id),
        Some(("characters", id)) => is_object_id(id),
        Some(_) => false,
    }
}

/// `POST /v1/videos/{id}/remix`.
fn is_video_remix(path: &str) -> bool {
    path.strip_prefix("/v1/videos/")
        .and_then(|tail| tail.strip_suffix("/remix"))
        .is_some_and(is_object_id)
}

/// Copies only the allowlisted headers.
#[must_use]
pub fn filter_headers(src: &HeaderMap, allowed: &[HeaderName]) -> HeaderMap {
    let mut out = HeaderMap::new();
    for name in allowed {
        for value in src.get_all(name) {
            out.append(name.clone(), value.clone());
        }
    }
    out
}

/// An OpenAI-style JSON error; `message` must not carry internal detail.
#[must_use]
pub fn error_response(status: StatusCode, code: &str, message: &str) -> Response<ProxyBody> {
    let body = json!({
        "error": {
            "message": message,
            "type": "reverse_error",
            "param": null,
            "code": code,
        }
    });
    let mut response = Response::new(full(body.to_string()));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    response
}

/// Re-streams a peer response with allowlisted headers and an idle guard.
#[must_use]
pub fn passthrough_response(response: Response<Incoming>, idle: Duration) -> Response<ProxyBody> {
    let (parts, body) = response.into_parts();
    let mut out = Response::new(guard(body, idle, None));
    *out.status_mut() = parts.status;
    *out.headers_mut() = filter_headers(&parts.headers, &RESPONSE_HEADERS);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_openai_routes_are_allowed() {
        assert!(is_allowed_route(&Method::GET, "/v1/models"));
        assert!(is_allowed_route(&Method::POST, "/v1/chat/completions"));
        assert!(is_allowed_route(&Method::POST, "/v1/embeddings"));
        assert!(is_allowed_route(&Method::POST, "/v1/embedding"));
        assert!(!is_allowed_route(&Method::POST, "/v1/models"));
        assert!(!is_allowed_route(&Method::GET, "/v1/chat/completions"));
        assert!(!is_allowed_route(&Method::POST, "/flush_cache"));
        assert!(!is_allowed_route(&Method::POST, "/v1/chat/completions/"));
        assert!(!is_allowed_route(&Method::POST, "/v1/embedding/"));
        assert!(!is_allowed_route(&Method::DELETE, "/v1/models"));
    }

    #[test]
    fn media_routes_follow_the_openai_videos_surface() {
        assert!(is_allowed_route(&Method::POST, "/v1/images/generations"));
        assert!(is_allowed_route(&Method::POST, "/v1/audio/transcriptions"));
        assert!(is_allowed_route(&Method::POST, "/v1/audio/speech"));
        assert!(is_allowed_route(&Method::POST, "/v1/videos"));
        assert!(is_allowed_route(&Method::GET, "/v1/videos"));
        assert!(is_allowed_route(&Method::GET, "/v1/videos/video_123"));
        assert!(is_allowed_route(
            &Method::GET,
            "/v1/videos/video_123/content"
        ));
        assert!(is_allowed_route(
            &Method::POST,
            "/v1/videos/video_123/remix"
        ));
        assert!(is_allowed_route(&Method::POST, "/v1/videos/edits"));
        assert!(is_allowed_route(&Method::POST, "/v1/videos/extensions"));
        assert!(is_allowed_route(&Method::POST, "/v1/videos/characters"));
        assert!(is_allowed_route(
            &Method::GET,
            "/v1/videos/characters/character_1"
        ));
    }

    #[test]
    fn media_routes_refuse_unknown_shapes_and_traversal() {
        assert!(!is_allowed_route(&Method::GET, "/v1/videos/characters"));
        assert!(!is_allowed_route(&Method::GET, "/v1/videos/edits"));
        assert!(!is_allowed_route(&Method::GET, "/v1/videos/video_1/remix"));
        assert!(!is_allowed_route(
            &Method::POST,
            "/v1/videos/video_1/content"
        ));
        assert!(!is_allowed_route(
            &Method::GET,
            "/v1/videos/video_1/content/extra"
        ));
        assert!(!is_allowed_route(&Method::GET, "/v1/videos/../models"));
        assert!(!is_allowed_route(&Method::GET, "/v1/videos/video.1"));
        assert!(!is_allowed_route(&Method::GET, "/v1/videos/video_1/"));
        assert!(!is_allowed_route(&Method::POST, "/v1/videos/video_1"));
        assert!(!is_allowed_route(&Method::DELETE, "/v1/videos/video_1"));
        assert!(!is_allowed_route(
            &Method::POST,
            "/v1/videos/video_1/remix/x"
        ));
        assert!(!is_allowed_route(
            &Method::GET,
            &format!("/v1/videos/{}", "x".repeat(129))
        ));
        assert!(!is_allowed_route(&Method::GET, "/v1/realtime"));
        assert!(!is_allowed_route(&Method::GET, "/v1/audio/transcriptions"));
    }

    #[test]
    fn credentials_are_never_forwarded() {
        let mut src = HeaderMap::new();
        src.insert(header::AUTHORIZATION, "Bearer secret".parse().unwrap());
        src.insert(header::COOKIE, "a=b".parse().unwrap());
        src.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
        let out = filter_headers(&src, &REQUEST_HEADERS);
        assert!(out.get(header::AUTHORIZATION).is_none());
        assert!(out.get(header::COOKIE).is_none());
        assert!(out.get(header::CONTENT_TYPE).is_some());
    }
}
