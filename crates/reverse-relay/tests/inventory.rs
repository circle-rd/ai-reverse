//! The inventory push to the settings owner, and the settings it brings back to LiteLLM.

mod common;

use common::{Options, Stack, eventually};
use http::StatusCode;
use serde_json::{Value, json};

const MODEL: &str = "u-tester/qwen3.8-27b";

fn with_inventory() -> Options {
    Options {
        inventory: true,
        ..Options::default()
    }
}

fn near(value: &Value, expected: f64) -> bool {
    value
        .as_f64()
        .is_some_and(|v| (v - expected).abs() < expected * 1e-9)
}

fn settings(output: &str) -> Value {
    json!([{
        "model": MODEL,
        "maxSlots": 4,
        "pricing": {
            "inputUsdPerMtok": "0.3",
            "outputUsdPerMtok": output,
            "cacheReadUsdPerMtok": "0.03",
        },
    }])
}

#[tokio::test]
async fn the_relay_pushes_its_nodes_signed_with_their_live_status() {
    let stack = Stack::start(with_inventory()).await;
    let log = stack.inventory.as_ref().unwrap().log.clone();

    let push = eventually("a push with the node serving and its status", || async {
        log.pushes()
            .into_iter()
            .find(|p| p["nodes"][0]["status"]["state"] == "idle")
    })
    .await;
    assert_eq!(push["relayId"], "relay-test");
    assert!(push["generatedAt"].as_u64().unwrap() > 0);
    let node = &push["nodes"][0];
    assert_eq!(node["ownerSub"], "user-tester");
    assert_eq!(node["ownerName"], "u-tester");
    assert!(!node["nodeId"].as_str().unwrap().contains("user-tester"));
    assert_eq!(node["models"][0]["name"], MODEL);
    assert_eq!(node["models"][0]["tags"], json!(["text"]));
    assert_eq!(node["hardware"]["cpuArch"], std::env::consts::ARCH);
    // The fake engine serves no metrics: unknown figures stay -1 and say what to do.
    assert_eq!(node["status"]["engine"]["maxSlots"], -1);
    assert_eq!(node["status"]["engine"]["hint"], "metrics-disabled");
    assert_eq!(node["status"]["benchmark"]["ttftMs"], -1);
    assert_eq!(log.refused(), 0, "every push carried a valid signature");

    // Nobody priced the model, so it is served unpriced and unlimited.
    let row = stack.litellm.row(MODEL).unwrap();
    assert!(row["litellm_params"].get("max_parallel_requests").is_none());
    assert!(row["litellm_params"].get("input_cost_per_token").is_none());
    assert!(row["model_info"].get("reverse_settings").is_none());

    stack.shutdown().await;
    let last = log.pushes().pop().unwrap();
    assert_eq!(last["nodes"], json!([]), "the relay says goodbye on exit");
}

#[tokio::test]
async fn owner_settings_reach_the_deployment_and_replace_it_when_they_change() {
    let stack = Stack::start(with_inventory()).await;
    let log = stack.inventory.as_ref().unwrap().log.clone();
    eventually("the deployment", || async { stack.litellm.row(MODEL) }).await;

    log.answer_settings(&settings("1.2"));
    let row = eventually("the priced deployment", || async {
        stack
            .litellm
            .row(MODEL)
            .filter(|row| row["litellm_params"]["max_parallel_requests"] == 4)
    })
    .await;
    for place in ["litellm_params", "model_info"] {
        assert!(near(&row[place]["input_cost_per_token"], 3e-7), "{place}");
        assert!(
            near(&row[place]["output_cost_per_token"], 1.2e-6),
            "{place}"
        );
        assert!(
            near(&row[place]["cache_read_input_token_cost"], 3e-8),
            "{place}"
        );
        assert!(row[place].get("cache_creation_input_token_cost").is_none());
    }
    assert_eq!(row["model_info"]["reverse_settings"]["maxSlots"], 4);
    let first = row["model_info"]["id"].clone();

    log.answer_settings(&settings("2.0"));
    let repriced = eventually("the repriced deployment", || async {
        stack
            .litellm
            .row(MODEL)
            .filter(|row| near(&row["model_info"]["output_cost_per_token"], 2e-6))
    })
    .await;
    assert_ne!(repriced["model_info"]["id"], first, "replaced, not patched");

    log.answer_settings(&json!([]));
    let cleared = eventually("the unpriced deployment", || async {
        stack
            .litellm
            .row(MODEL)
            .filter(|row| row["litellm_params"].get("max_parallel_requests").is_none())
    })
    .await;
    assert!(
        cleared["litellm_params"]
            .get("output_cost_per_token")
            .is_none()
    );
    assert_eq!(stack.litellm.names(), [MODEL], "never two deployments");
    stack.shutdown().await;
}

#[tokio::test]
async fn an_unusable_answer_keeps_the_settings_already_applied() {
    let stack = Stack::start(with_inventory()).await;
    let log = stack.inventory.as_ref().unwrap().log.clone();
    log.answer_settings(&settings("1.2"));
    let row = eventually("the priced deployment", || async {
        stack
            .litellm
            .row(MODEL)
            .filter(|row| row["litellm_params"]["max_parallel_requests"] == 4)
    })
    .await;

    let bad_slots = json!({"settings": [{"model": MODEL, "maxSlots": 0}]}).to_string();
    let bad_price = json!({"settings": [{"model": MODEL, "pricing":
        {"inputUsdPerMtok": "-1", "outputUsdPerMtok": "1"}}]})
    .to_string();
    for (status, body) in [
        (StatusCode::OK, bad_slots.as_str()),
        (StatusCode::OK, bad_price.as_str()),
        (StatusCode::OK, "not json"),
        (StatusCode::INTERNAL_SERVER_ERROR, "{}"),
    ] {
        log.answer_raw(status, body);
        let seen = log.pushes().len();
        eventually("two more pushes", || async {
            (log.pushes().len() >= seen + 2).then_some(())
        })
        .await;
        let now = stack.litellm.row(MODEL).unwrap();
        assert_eq!(now["model_info"]["id"], row["model_info"]["id"], "{body}");
        assert_eq!(now["litellm_params"]["max_parallel_requests"], 4, "{body}");
    }
    stack.shutdown().await;
}
