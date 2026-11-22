//! A fake Hugging Face hub answering model metadata, so tests never reach the network.

use std::convert::Infallible;
use std::net::SocketAddr;

use http::{Request, StatusCode};
use hyper::body::Incoming;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use super::{serve_http1, text};

pub struct FakeHub {
    pub addr: SocketAddr,
    task: JoinHandle<()>,
}

impl FakeHub {
    /// Answers `feature-extraction` for embedding ids and 401 for anything else, like the hub
    /// does for repositories it will not disclose.
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(serve_http1(
            listener,
            |request: Request<Incoming>| async move {
                let path = request.uri().path().to_owned();
                let reply = if path.ends_with("/users/perplexity-ai/overview") {
                    text(StatusCode::NOT_FOUND, "application/json", "{}")
                } else if path.ends_with("/organizations/perplexity-ai/overview") {
                    text(
                        StatusCode::OK,
                        "application/json",
                        r#"{"avatarUrl":"https://cdn-avatars.huggingface.co/avatar.png"}"#,
                    )
                } else if path.ends_with("pplx-embed-v1-0.6b") {
                    text(
                        StatusCode::OK,
                        "application/json",
                        r#"{"pipeline_tag":"feature-extraction","tags":["feature-extraction","abliterated"]}"#,
                    )
                } else {
                    text(StatusCode::UNAUTHORIZED, "application/json", "{}")
                };
                Ok::<_, Infallible>(reply)
            },
        ));
        Self { addr, task }
    }
}

impl Drop for FakeHub {
    fn drop(&mut self) {
        self.task.abort();
    }
}
