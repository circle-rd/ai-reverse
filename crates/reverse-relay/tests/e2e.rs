//! End to end: a real agent and relay between a fake engine and a fake LiteLLM.

mod common;

use std::time::Duration;

use common::{AUDIENCE, ENGINE_KEY, Options, Stack, eventually, send, send_streaming};
use http::{Method, StatusCode, header};
use http_body_util::BodyExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const CHAT: &str = "/tester/v1/chat/completions";

#[tokio::test]
async fn registers_the_served_model_and_streams_a_completion() {
    let stack = Stack::start(Options::default()).await;

    let row = eventually("the deployment to be registered", || async {
        stack.litellm.row("tester/qwen3.8-27b")
    })
    .await;
    assert_eq!(row["litellm_params"]["model"], "hosted_vllm/qwen3.8-27b");
    assert_eq!(
        row["litellm_params"]["api_base"],
        "http://relay.test/tester/v1"
    );
    assert_eq!(row["model_info"]["managed_by"], "reverse-relay");
    assert_eq!(row["model_info"]["created_by"], "tester");

    let reply = send(
        stack.relay_api,
        Method::POST,
        CHAT,
        r#"{"model":"qwen3.8-27b","stream":true,"messages":[]}"#,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.headers[header::CONTENT_TYPE], "text/event-stream");
    let text = String::from_utf8(reply.body.to_vec()).unwrap();
    assert!(
        text.contains("Hello ") && text.ends_with("data: [DONE]\n\n"),
        "{text}"
    );

    let hits = stack.engine.hits_on("/v1/chat/completions");
    let hit = hits.last().expect("the engine saw the completion");
    assert_eq!(hit.model.as_deref(), Some("qwen3.8-27b"));
    assert_eq!(
        hit.authorization.as_deref(),
        Some(format!("Bearer {ENGINE_KEY}").as_str()),
        "the engine key replaces whatever credential LiteLLM sent"
    );
    stack.shutdown().await;
}

#[tokio::test]
async fn an_embedding_model_outside_the_engine_list_is_announced_and_tagged() {
    let stack = Stack::start(Options {
        extra_models: Some("perplexity-ai/pplx-embed-v1-0.6b"),
        model_tags: Some("qwen3.8-27b=text,tools;perplexity-ai/pplx-embed-v1-0.6b=embedding"),
        ..Options::default()
    })
    .await;

    let row = eventually("the embedding deployment to be registered", || async {
        stack.litellm.row("tester/perplexity-ai/pplx-embed-v1-0.6b")
    })
    .await;
    assert_eq!(row["model_info"]["mode"], "embedding");
    assert_eq!(row["model_info"]["tags"][0], "embedding");
    assert_eq!(row["model_info"]["hub_owner"], "perplexity-ai");
    assert_eq!(
        row["model_info"]["hub_avatar_url"],
        "https://cdn-avatars.huggingface.co/avatar.png"
    );
    assert_eq!(row["model_info"]["author_safety_claim"], "abliterated");

    let llm = eventually("the tagged LLM deployment to be registered", || async {
        stack.litellm.row("tester/qwen3.8-27b")
    })
    .await;
    assert_eq!(llm["model_info"]["tags"][0], "text");
    assert_eq!(llm["model_info"]["tags"][1], "tools");
    assert_eq!(llm["model_info"]["supports_function_calling"], true);
    assert_eq!(llm["model_info"]["created_by"], "tester");
    stack.shutdown().await;
}

#[tokio::test]
async fn a_model_change_on_the_engine_replaces_the_deployment() {
    let stack = Stack::start(Options::default()).await;
    eventually("the first model", || async {
        stack.litellm.row("tester/qwen3.8-27b")
    })
    .await;

    stack.engine.set_models(&["alpha"]);
    eventually("alpha to replace the first model", || async {
        (stack.litellm.names() == ["tester/alpha"]).then_some(())
    })
    .await;

    stack.engine.set_models(&["Org/beta-7b"]);
    eventually("beta to replace alpha", || async {
        (stack.litellm.names() == ["tester/Org/beta-7b"]).then_some(())
    })
    .await;
    stack.shutdown().await;
}

#[tokio::test]
async fn an_engine_outage_deregisters_after_the_grace_period_and_recovers() {
    let stack = Stack::start(Options::default()).await;
    eventually("the model", || async {
        stack.litellm.row("tester/qwen3.8-27b")
    })
    .await;

    stack.engine.set_models(&[]);
    eventually("the model to be removed", || async {
        stack.litellm.names().is_empty().then_some(())
    })
    .await;

    stack.engine.set_models(&["qwen3.8-27b"]);
    eventually("the model to return", || async {
        stack.litellm.row("tester/qwen3.8-27b")
    })
    .await;
    stack.shutdown().await;
}

#[tokio::test]
async fn a_dropped_agent_is_deregistered_and_requests_get_a_503() {
    let mut stack = Stack::start(Options::default()).await;
    eventually("the model", || async {
        stack.litellm.row("tester/qwen3.8-27b")
    })
    .await;

    stack.stop_agent().await;
    eventually("the deployment to be removed", || async {
        stack.litellm.names().is_empty().then_some(())
    })
    .await;

    let reply = send(stack.relay_api, Method::POST, CHAT, "{}").await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.json()["error"]["code"], "agent_unavailable");
    assert_eq!(reply.headers[header::RETRY_AFTER], "5");
    stack.shutdown().await;
}

#[tokio::test]
async fn a_client_disconnect_cancels_the_stream_on_the_engine() {
    let stack = Stack::start(Options::default()).await;
    eventually("the model", || async {
        stack.litellm.row("tester/qwen3.8-27b")
    })
    .await;

    let mut response = send_streaming(
        stack.relay_api,
        CHAT,
        r#"{"model":"qwen3.8-27b","stream":true,"slow":true}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let first = response.body_mut().frame().await;
    assert!(first.is_some(), "the stream started");
    drop(response);

    eventually("the engine to notice the disconnect", || async {
        stack.engine.stream_was_cancelled().then_some(())
    })
    .await;
    stack.shutdown().await;
}

#[tokio::test]
async fn only_allowlisted_routes_and_connected_clients_are_served() {
    let stack = Stack::start(Options::default()).await;
    eventually("the model", || async {
        stack.litellm.row("tester/qwen3.8-27b")
    })
    .await;

    let health = send(stack.relay_api, Method::GET, "/healthz", "").await;
    assert_eq!(health.status, StatusCode::OK);

    let files = send(stack.relay_api, Method::POST, "/tester/v1/files", "{}").await;
    assert_eq!(files.status, StatusCode::NOT_FOUND);
    assert_eq!(files.json()["error"]["code"], "route_not_allowed");

    let admin = send(stack.relay_api, Method::POST, "/tester/flush_cache", "{}").await;
    assert_eq!(admin.status, StatusCode::NOT_FOUND);

    let nobody = send(stack.relay_api, Method::GET, "/nobody1/v1/models", "").await;
    assert_eq!(nobody.status, StatusCode::SERVICE_UNAVAILABLE);

    let reserved = send(stack.relay_api, Method::GET, "/openai/v1/models", "").await;
    assert_eq!(reserved.status, StatusCode::NOT_FOUND);

    let models = send(stack.relay_api, Method::GET, "/tester/v1/models", "").await;
    assert_eq!(models.status, StatusCode::OK);
    assert_eq!(models.json()["data"][0]["id"], "qwen3.8-27b");
    stack.shutdown().await;
}

async fn raw_exchange(addr: std::net::SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut buf = vec![0_u8; 512];
    let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
        .await
        .unwrap()
        .unwrap();
    String::from_utf8_lossy(buf.get(..n).unwrap_or_default()).into_owned()
}

#[tokio::test]
async fn the_tunnel_port_refuses_bad_credentials_and_plain_requests() {
    let stack = Stack::start(Options::default()).await;
    let upgrade = |authorization: &str| {
        let auth = if authorization.is_empty() {
            String::new()
        } else {
            format!("authorization: {authorization}\r\n")
        };
        format!(
            "GET /tunnel HTTP/1.1\r\nhost: relay\r\nconnection: upgrade\r\nupgrade: reverse-h2/1\r\n{auth}\r\n"
        )
    };

    let stolen_shape = "Bearer stale-token".to_owned();
    for bad in [
        "",
        "Bearer nope",
        "Basic dGVzdGVyOg==",
        stolen_shape.as_str(),
    ] {
        let reply = raw_exchange(stack.relay_tunnel, &upgrade(bad)).await;
        assert!(reply.starts_with("HTTP/1.1 401"), "{bad:?}: {reply}");
    }

    let plain = raw_exchange(
        stack.relay_tunnel,
        "GET /tunnel HTTP/1.1\r\nhost: relay\r\nauthorization: Bearer stale-token\r\n\r\n",
    )
    .await;
    assert!(plain.starts_with("HTTP/1.1 400"), "{plain}");

    let elsewhere = raw_exchange(
        stack.relay_tunnel,
        "GET /v1/models HTTP/1.1\r\nhost: relay\r\n\r\n",
    )
    .await;
    assert!(elsewhere.starts_with("HTTP/1.1 404"), "{elsewhere}");
    stack.shutdown().await;
}

#[tokio::test]
async fn the_tunnel_requires_the_right_audience_scope_and_agent_id() {
    let stack = Stack::start(Options::default()).await;
    let upgrade = |token: &str, agent: Option<&str>| {
        let agent = agent
            .map(|id| format!("x-agent-id: {id}\r\n"))
            .unwrap_or_default();
        format!(
            "GET /tunnel HTTP/1.1\r\nhost: relay\r\nconnection: upgrade\r\nupgrade: reverse-h2/1\r\nauthorization: Bearer {token}\r\n{agent}\r\n"
        )
    };
    let scope = "openid profile offline_access tunnel:serve";
    for token in [
        stack.idp.token("http://wrong.test", scope),
        stack.idp.token(AUDIENCE, "openid profile"),
    ] {
        let reply = raw_exchange(
            stack.relay_tunnel,
            &upgrade(&token, Some("0123456789abcdef")),
        )
        .await;
        assert!(reply.starts_with("HTTP/1.1 401"), "{reply}");
    }
    let valid = stack.idp.token(AUDIENCE, scope);
    for agent in [None, Some("not-hex")] {
        let reply = raw_exchange(stack.relay_tunnel, &upgrade(&valid, agent)).await;
        assert!(reply.starts_with("HTTP/1.1 401"), "{agent:?}: {reply}");
    }
    let accepted = raw_exchange(
        stack.relay_tunnel,
        &upgrade(&valid, Some("0123456789abcdef")),
    )
    .await;
    assert!(accepted.starts_with("HTTP/1.1 101"), "{accepted}");
    stack.shutdown().await;
}

#[tokio::test]
async fn the_api_port_requires_the_shared_key() {
    let stack = Stack::start(Options::default()).await;
    for auth in [
        "",
        "authorization: Bearer wrong\r\n",
        "authorization: Basic abc\r\n",
    ] {
        let reply = raw_exchange(
            stack.relay_api,
            &format!("GET /tester/v1/models HTTP/1.1\r\nhost: relay\r\n{auth}\r\n"),
        )
        .await;
        assert!(reply.starts_with("HTTP/1.1 401"), "{auth:?}: {reply}");
    }
    let health = raw_exchange(
        stack.relay_api,
        "GET /healthz HTTP/1.1\r\nhost: relay\r\n\r\n",
    )
    .await;
    assert!(health.starts_with("HTTP/1.1 200"), "{health}");
    stack.shutdown().await;
}

#[tokio::test]
async fn orphaned_deployments_are_cleaned_up_but_foreign_ones_are_kept() {
    let stack = Stack::start_with(Options::default(), |litellm| {
        litellm.seed("ghost/old-model", true);
        litellm.seed("operator/own-model", false);
    })
    .await;

    eventually("the orphan to be removed", || async {
        let names = stack.litellm.names();
        (!names.contains(&"ghost/old-model".to_owned())
            && names.contains(&"operator/own-model".to_owned())
            && names.contains(&"tester/qwen3.8-27b".to_owned()))
        .then_some(())
    })
    .await;
    stack.shutdown().await;
}

#[tokio::test]
async fn shutting_down_removes_what_the_relay_registered() {
    let stack = Stack::start_with(Options::default(), |litellm| {
        litellm.seed("operator/own-model", false);
    })
    .await;
    eventually("the model", || async {
        stack.litellm.row("tester/qwen3.8-27b")
    })
    .await;

    let litellm = stack.shutdown().await;
    assert_eq!(litellm.names(), ["operator/own-model"]);
}

#[tokio::test]
async fn a_rejected_engine_key_surfaces_as_a_bad_gateway_not_a_401() {
    let stack = Stack::start(Options {
        agent_key: "wrong-key",
        ..Options::default()
    })
    .await;

    let reply = eventually("the tunnel to carry a request", || async {
        let reply = send(stack.relay_api, Method::POST, CHAT, "{}").await;
        (reply.status != StatusCode::SERVICE_UNAVAILABLE).then_some(reply)
    })
    .await;
    assert_eq!(reply.status, StatusCode::BAD_GATEWAY);
    assert_eq!(
        reply.json()["error"]["code"],
        "upstream_credentials_rejected"
    );
    assert!(
        stack.litellm.names().is_empty(),
        "no model is served without a working key"
    );
    stack.shutdown().await;
}

#[tokio::test]
async fn repeated_bad_credentials_lock_the_address_out() {
    let stack = Stack::start(Options::default()).await;
    let upgrade = |authorization: &str| {
        format!(
            "GET /tunnel HTTP/1.1\r\nhost: relay\r\nconnection: upgrade\r\nupgrade: reverse-h2/1\r\nauthorization: {authorization}\r\nx-agent-id: 0123456789abcdef\r\n\r\n"
        )
    };
    for _ in 0..5 {
        let reply = raw_exchange(stack.relay_tunnel, &upgrade("Bearer nope")).await;
        assert!(reply.starts_with("HTTP/1.1 401"), "{reply}");
    }
    let blocked = raw_exchange(stack.relay_tunnel, &upgrade("Bearer nope")).await;
    assert!(blocked.starts_with("HTTP/1.1 429"), "{blocked}");
    assert!(
        blocked.to_ascii_lowercase().contains("retry-after"),
        "{blocked}"
    );
    stack.shutdown().await;
}

#[tokio::test]
async fn an_account_cannot_exceed_its_tunnel_cap() {
    let stack = Stack::start(Options {
        max_tunnels_per_user: Some("1"),
        ..Options::default()
    })
    .await;
    eventually("the first tunnel to register", || async {
        (stack.litellm.names() == ["tester/qwen3.8-27b"]).then_some(())
    })
    .await;
    let token = stack
        .idp
        .token(AUDIENCE, "openid profile offline_access tunnel:serve");
    let upgrade = format!(
        "GET /tunnel HTTP/1.1\r\nhost: relay\r\nconnection: upgrade\r\nupgrade: reverse-h2/1\r\nauthorization: Bearer {token}\r\nx-agent-id: 0123456789abcdef\r\n\r\n"
    );
    let reply = raw_exchange(stack.relay_tunnel, &upgrade).await;
    assert!(reply.starts_with("HTTP/1.1 429"), "{reply}");
    stack.shutdown().await;
}
