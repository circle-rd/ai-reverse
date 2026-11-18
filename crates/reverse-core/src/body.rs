//! Request/response bodies that are streamed, bounded and cancellable.

use std::time::Duration;

use bytes::Bytes;
use futures_util::stream;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Empty, Full, StreamBody};
use hyper::body::Incoming;
use thiserror::Error;

/// The single body type flowing through both services.
pub type ProxyBody = UnsyncBoxBody<Bytes, BodyError>;

/// Why a streamed body ended abnormally.
#[derive(Debug, Error)]
pub enum BodyError {
    /// The peer failed while sending the body.
    #[error("body stream failed: {0}")]
    Stream(#[from] hyper::Error),
    /// No frame arrived within the idle window.
    #[error("no body data for {0:?}")]
    Idle(Duration),
    /// The body exceeded the configured limit.
    #[error("body exceeded {0} bytes")]
    TooLarge(usize),
}

/// A body with no data.
#[must_use]
pub fn empty() -> ProxyBody {
    Empty::new().map_err(|never| match never {}).boxed_unsync()
}

/// A fully buffered body.
#[must_use]
pub fn full(data: impl Into<Bytes>) -> ProxyBody {
    Full::new(data.into())
        .map_err(|never| match never {})
        .boxed_unsync()
}

/// Streams `body` while failing it on a stalled peer or an oversized payload.
///
/// Dropping the result drops `body`, which resets the underlying HTTP/2 stream or closes the
/// HTTP/1 connection: that is how a client disconnect reaches the inference engine.
#[must_use]
pub fn guard(body: Incoming, idle: Duration, max_bytes: Option<usize>) -> ProxyBody {
    let frames = stream::unfold(Some((body, 0_usize)), move |state| async move {
        let (mut body, mut seen) = state?;
        match tokio::time::timeout(idle, body.frame()).await {
            Err(_) => Some((Err(BodyError::Idle(idle)), None)),
            Ok(None) => None,
            Ok(Some(Err(source))) => Some((Err(BodyError::Stream(source)), None)),
            Ok(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    seen = seen.saturating_add(data.len());
                    if let Some(max) = max_bytes
                        && seen > max
                    {
                        return Some((Err(BodyError::TooLarge(max)), None));
                    }
                }
                Some((Ok(frame), Some((body, seen))))
            }
        }
    });
    StreamBody::new(frames).boxed_unsync()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn empty_and_full_bodies_collect() {
        let empty = empty().collect().await.unwrap().to_bytes();
        assert!(empty.is_empty());
        let full = full("abc").collect().await.unwrap().to_bytes();
        assert_eq!(&full[..], b"abc");
    }
}
