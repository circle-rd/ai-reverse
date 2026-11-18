//! Load smoke test: hundreds of agents pooling into one alias.
//!
//! Ignored by default because it opens 500 tunnels; run it with
//! `cargo test -p reverse-relay --test load -- --ignored --nocapture`.

mod common;

use std::time::{Duration, Instant};

use common::{Options, Stack, send};
use http::{Method, StatusCode};

const HOLD: Duration = Duration::from_secs(15);

fn agents() -> usize {
    std::env::var("LOAD_AGENTS")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(500)
}

async fn wait_until(what: &str, mut probe: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while !probe() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
#[ignore = "load test: 500 tunnels"]
async fn many_agents_pool_into_one_alias() {
    let mut stack = Stack::start(Options {
        max_tunnels: Some("1024"),
        max_tunnels_per_user: Some("1024"),
        ..Options::default()
    })
    .await;
    let count = agents();
    let mut sims = Vec::with_capacity(count);
    for index in 0..count {
        sims.push(stack.spawn_agent(index).await);
    }

    wait_until("the pooled alias to be registered", || {
        stack.litellm.names() == ["tester/qwen3.8-27b"]
    })
    .await;
    for _ in 0..20 {
        let reply = send(
            stack.relay_api,
            Method::POST,
            "/tester/v1/chat/completions",
            r#"{"model":"qwen3.8-27b","messages":[]}"#,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
    }

    tokio::time::sleep(HOLD).await;
    for agent in sims {
        agent.stop().await;
    }
    stack.stop_agent().await;
    wait_until("the alias to be removed", || {
        stack.litellm.names().is_empty()
    })
    .await;
    stack.shutdown().await;
}
