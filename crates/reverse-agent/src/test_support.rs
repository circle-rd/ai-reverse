//! A loopback HTTP server with fixed answers, for tests that need a real [`Upstream`].

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::Config;
use crate::upstream::Upstream;

/// A `(path, status, body)` answer; any other path is a 404.
pub type Route = (&'static str, u16, &'static str);

/// Serves `routes` on a loopback port and returns an [`Upstream`] pointing at it.
pub async fn upstream_serving(routes: &'static [Route]) -> Upstream {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = [0_u8; 4096];
            let read = socket.read(&mut buffer).await.unwrap_or(0);
            let head = String::from_utf8_lossy(&buffer[..read]).into_owned();
            let path = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
            let (status, body) = routes
                .iter()
                .find(|(route, _, _)| *route == path)
                .map_or((404, ""), |(_, status, body)| (*status, *body));
            let answer = format!(
                "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(answer.as_bytes()).await;
        }
    });
    let pairs = [
        ("AGENT_RELAY_URL", "http://127.0.0.1:8443".to_owned()),
        ("AGENT_UPSTREAM_URL", format!("http://{addr}")),
        ("AGENT_AUTH_ISSUER", "http://127.0.0.1:9000".to_owned()),
        ("AGENT_AUTH_CLIENT_ID", "reverse-agent".to_owned()),
    ];
    let env = reverse_core::config::Env::new(move |key| {
        pairs
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.clone())
    });
    Upstream::new(&Config::from_env(&env).unwrap())
}
