//! Accept loop shared by the tunnel and API listeners.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::Duration;

use hyper::Request;
use hyper::Response;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use reverse_core::body::ProxyBody;
use reverse_core::shutdown::triggered;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinSet;

const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// Serves HTTP/1 connections until shutdown; `make` builds one handler per connection.
pub async fn serve<M, H, Fut>(
    listener: TcpListener,
    label: &'static str,
    mut shutdown: watch::Receiver<bool>,
    make: M,
) where
    M: Fn(SocketAddr) -> H,
    H: Fn(Request<Incoming>) -> Fut + Send + 'static,
    Fut: Future<Output = Result<Response<ProxyBody>, Infallible>> + Send + 'static,
{
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    connections.spawn(connection(stream, make(peer), shutdown.clone()));
                }
                Err(error) => {
                    tracing::warn!(%error, listener = label, "accept.failed");
                    tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
                }
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
            () = triggered(&mut shutdown) => break,
        }
    }
    let drained = tokio::time::timeout(DRAIN_TIMEOUT, async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        connections.abort_all();
    }
}

async fn connection<H, Fut>(stream: TcpStream, handler: H, mut shutdown: watch::Receiver<bool>)
where
    H: Fn(Request<Incoming>) -> Fut + Send + 'static,
    Fut: Future<Output = Result<Response<ProxyBody>, Infallible>> + Send + 'static,
{
    if let Err(error) = stream.set_nodelay(true) {
        tracing::debug!(%error, "socket.nodelay_failed");
    }
    // Upgraded tunnels outlive this future: they own their own shutdown handling.
    let served = http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT)
        .serve_connection(TokioIo::new(stream), service_fn(handler))
        .with_upgrades();
    tokio::pin!(served);
    tokio::select! {
        result = served.as_mut() => {
            if let Err(error) = result {
                tracing::debug!(%error, "connection.ended");
            }
        }
        () = triggered(&mut shutdown) => {
            served.as_mut().graceful_shutdown();
            if tokio::time::timeout(DRAIN_TIMEOUT, served).await.is_err() {
                tracing::debug!("connection.drain_timeout");
            }
        }
    }
}
