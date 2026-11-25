//! Pushes the relay's inventory to the application that owns the settings, and applies the
//! settings it answers with.
//!
//! The relay keeps no durable state. Each push is the complete picture of what is connected,
//! signed with a shared secret; the answer is the complete set of owner settings for it.
//! A failed push is not queued: the next one carries the latest picture.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use http::{StatusCode, header};
use reqwest::{Client, redirect};
use reverse_core::inventory::{InventoryAnswer, InventorySnapshot, ModelSettings};
use reverse_core::shutdown::triggered;
use ring::hmac;
use tokio::sync::watch;
use tokio::time::{MissedTickBehavior, interval};

use crate::config::InventoryConfig;
use crate::error::InventoryError;
use crate::registry::Registry;

const SIGNATURE_HEADER: &str = "x-relay-signature";
const TIMEOUT: Duration = Duration::from_secs(10);
const FINAL_PUSH_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_ANSWER_BYTES: usize = 1024 * 1024;
const MAX_SETTINGS: usize = 4096;

/// The signed channel to the receiver.
pub struct Inventory {
    http: Client,
    url: String,
    key: hmac::Key,
    relay_id: String,
}

impl Inventory {
    /// Builds the client; nothing is sent yet.
    pub fn new(config: &InventoryConfig) -> Result<Self, reqwest::Error> {
        let http = Client::builder()
            .timeout(TIMEOUT)
            .connect_timeout(TIMEOUT)
            .redirect(redirect::Policy::none())
            .build()?;
        Ok(Self {
            http,
            url: config.url.clone(),
            key: hmac::Key::new(hmac::HMAC_SHA256, config.secret.expose().as_bytes()),
            relay_id: config.relay_id.clone(),
        })
    }

    /// Sends one snapshot and returns the settings the receiver answered with.
    async fn push(
        &self,
        nodes: Vec<reverse_core::inventory::InventoryNode>,
        generated_at: u64,
    ) -> Result<BTreeMap<String, ModelSettings>, InventoryError> {
        let body = serde_json::to_vec(&InventorySnapshot {
            relay_id: self.relay_id.clone(),
            generated_at,
            nodes,
        })?;
        let timestamp = unix_seconds()?;
        let mut response = self
            .http
            .post(&self.url)
            .header(header::CONTENT_TYPE, "application/json")
            .header(SIGNATURE_HEADER, signature(&self.key, timestamp, &body))
            .body(body)
            .send()
            .await?;
        if response.status() != StatusCode::OK {
            return Err(InventoryError::Status(response.status()));
        }
        let mut answer = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if answer.len() + chunk.len() > MAX_ANSWER_BYTES {
                return Err(InventoryError::TooLarge(MAX_ANSWER_BYTES));
            }
            answer.extend_from_slice(&chunk);
        }
        settings_of(&answer)
    }
}

/// `t=<unix seconds>,v1=<hex HMAC-SHA256 of "<t>.<body>">`.
fn signature(key: &hmac::Key, timestamp: u64, body: &[u8]) -> String {
    let mut signed = format!("{timestamp}.").into_bytes();
    signed.extend_from_slice(body);
    let mac = hmac::sign(key, &signed);
    let hex: String = mac.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    format!("t={timestamp},v1={hex}")
}

/// Validates the answer and keys the settings by published name.
///
/// One invalid entry rejects the whole answer: the caller keeps what it had, because dropping
/// a bad entry would silently serve that model unpriced.
fn settings_of(answer: &[u8]) -> Result<BTreeMap<String, ModelSettings>, InventoryError> {
    let answer: InventoryAnswer =
        serde_json::from_slice(answer).map_err(|e| InventoryError::Answer(e.to_string()))?;
    if answer.settings.len() > MAX_SETTINGS {
        return Err(InventoryError::Answer("too many settings".to_owned()));
    }
    let mut settings = BTreeMap::new();
    for entry in answer.settings {
        let validated = entry
            .settings
            .validated()
            .map_err(|e| InventoryError::Answer(e.to_string()))?;
        // An entry that decides nothing is the same as no entry.
        if validated != ModelSettings::default()
            && settings.insert(entry.model, validated).is_some()
        {
            return Err(InventoryError::Answer("a model appears twice".to_owned()));
        }
    }
    Ok(settings)
}

fn unix_seconds() -> Result<u64, InventoryError> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| InventoryError::Clock)?
        .as_secs())
}

fn unix_millis() -> Result<u64, InventoryError> {
    Ok(u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| InventoryError::Clock)?
            .as_millis(),
    )
    .unwrap_or(u64::MAX))
}

/// Pushes every `every` until shutdown, then once more with no node so the receiver sees the
/// relay go.
pub async fn run(
    inventory: Inventory,
    registry: Arc<Registry>,
    every: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut ticker = interval(every);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last = 0_u64;
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            () = triggered(&mut shutdown) => break,
        }
        // The receiver refuses an older snapshot, so the stamp never goes backwards.
        let stamp = match unix_millis() {
            Ok(now) => now.max(last + 1),
            Err(error) => {
                tracing::warn!(%error, "inventory.push_failed");
                continue;
            }
        };
        last = stamp;
        match inventory.push(registry.inventory_nodes(), stamp).await {
            Ok(settings) => {
                let count = settings.len();
                if registry.set_settings(settings) {
                    tracing::info!(models = count, "inventory.settings_changed");
                }
            }
            Err(error) => tracing::warn!(%error, "inventory.push_failed"),
        }
    }
    let goodbye = inventory.push(Vec::new(), last.max(unix_millis().unwrap_or(0)) + 1);
    match tokio::time::timeout(FINAL_PUSH_TIMEOUT, goodbye).await {
        Ok(Ok(_)) => tracing::info!("inventory.cleared"),
        Ok(Err(error)) => tracing::warn!(%error, "inventory.clear_failed"),
        Err(_) => tracing::warn!("inventory.clear_timeout"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_signature_is_hmac_sha256_over_the_timestamp_a_dot_and_the_body() {
        // RFC 4231, test case 2: pins the primitive.
        let key = hmac::Key::new(hmac::HMAC_SHA256, b"Jefe");
        let mac = hmac::sign(&key, b"what do ya want for nothing?");
        let hex: String = mac.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        let header = signature(&key, 1_700_000_000, b"{}");
        assert!(header.starts_with("t=1700000000,v1="));
        assert_eq!(header.len(), "t=1700000000,v1=".len() + 64);
        // The receiver recomputes it from the same text.
        let tag = hmac::sign(&key, b"1700000000.{}");
        let expected: String = tag.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        assert!(header.ends_with(&expected));
    }

    #[test]
    fn a_valid_answer_is_keyed_by_model_and_drops_empty_entries() {
        let answer = br#"{"changed":true,"settings":[
            {"model":"u-a/m","maxSlots":2,"pricing":{"inputUsdPerMtok":"0.3","outputUsdPerMtok":"1.2"}},
            {"model":"u-a/n"}]}"#;
        let settings = settings_of(answer).unwrap();
        assert_eq!(settings.len(), 1);
        assert_eq!(settings["u-a/m"].max_slots, Some(2));
    }

    #[test]
    fn an_answer_with_one_bad_entry_is_rejected_whole() {
        let zero_slots = br#"{"settings":[{"model":"u-a/m","maxSlots":0}]}"#;
        assert!(settings_of(zero_slots).is_err());
        let bad_price = br#"{"settings":[{"model":"u-a/m","pricing":{"inputUsdPerMtok":"x","outputUsdPerMtok":"1"}}]}"#;
        assert!(settings_of(bad_price).is_err());
        let twice =
            br#"{"settings":[{"model":"u-a/m","maxSlots":1},{"model":"u-a/m","maxSlots":2}]}"#;
        assert!(settings_of(twice).is_err());
        assert!(settings_of(b"not json").is_err());
    }
}
