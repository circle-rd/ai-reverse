//! Several nodes under one account: routing by model and by load, and independent lifetimes.

mod common;

use common::{Options, Stack, eventually, send, send_streaming};
use http::{Method, StatusCode};

const CHAT: &str = "/tester/v1/chat/completions";

fn body(model: &str, slow: bool) -> String {
    format!(r#"{{"model":"{model}","stream":true,"slow":{slow},"messages":[]}}"#)
}

fn chat_hits(engine: &common::FakeEngine) -> usize {
    engine.hits_on("/v1/chat/completions").len()
}

#[tokio::test]
async fn each_model_is_served_by_the_node_that_has_it() {
    let stack = Stack::start(Options {
        models: &["alpha"],
        ..Options::default()
    })
    .await;
    let second = stack.start_second_agent(&["beta"]).await;
    eventually("both models to be registered", || async {
        let names = stack.litellm.names();
        (names == ["tester/alpha", "tester/beta"]).then_some(())
    })
    .await;

    for model in ["beta", "alpha", "beta"] {
        let reply = send(stack.relay_api, Method::POST, CHAT, &body(model, false)).await;
        assert_eq!(reply.status, StatusCode::OK, "{model}");
    }
    assert_eq!(chat_hits(&stack.engine), 1, "alpha stays on its own node");
    assert_eq!(chat_hits(&second.engine), 2, "beta goes to the second node");
    second.stop().await;
    stack.shutdown().await;
}

#[tokio::test]
async fn a_model_served_twice_is_shared_by_load() {
    let stack = Stack::start(Options::default()).await;
    let second = stack.start_second_agent(&["qwen3.8-27b"]).await;
    eventually("both nodes to report the model", || async {
        let names = stack.litellm.names();
        (names == ["tester/qwen3.8-27b"]).then_some(())
    })
    .await;

    let first = send_streaming(stack.relay_api, CHAT, &body("qwen3.8-27b", true)).await;
    let other = send_streaming(stack.relay_api, CHAT, &body("qwen3.8-27b", true)).await;
    assert_eq!(chat_hits(&stack.engine), 1);
    assert_eq!(chat_hits(&second.engine), 1);
    drop((first, other));
    second.stop().await;
    stack.shutdown().await;
}

#[tokio::test]
async fn losing_one_node_keeps_the_models_of_the_others() {
    let stack = Stack::start(Options {
        models: &["alpha"],
        ..Options::default()
    })
    .await;
    let second = stack.start_second_agent(&["beta"]).await;
    eventually("both models", || async {
        (stack.litellm.names().len() == 2).then_some(())
    })
    .await;

    second.stop().await;
    eventually("beta to be deregistered", || async {
        (stack.litellm.names() == ["tester/alpha"]).then_some(())
    })
    .await;
    let reply = send(stack.relay_api, Method::POST, CHAT, &body("alpha", false)).await;
    assert_eq!(reply.status, StatusCode::OK);
    stack.shutdown().await;
}
