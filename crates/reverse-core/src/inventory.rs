//! The inventory the relay pushes to the application that owns the settings, and the settings
//! it receives in the answer.
//!
//! The relay holds no durable state: it describes what is connected, and the answer says what
//! the owners decided (price and parallel requests per model). Both directions are validated
//! here, at the boundary, so nothing downstream sees an unchecked value.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::status::AgentStatus;
use crate::wire::{HardwareInfo, ModelTag};

/// Most parallel requests an owner may allow on one model.
pub const MAX_SLOTS_LIMIT: u32 = 100_000;

/// A rejected price or setting.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SettingsError {
    /// The price is not a plain decimal with at most 6 decimals.
    #[error("price {0:?} is not a plain decimal with at most 6 decimals")]
    Price(String),
    /// The slot limit is zero or above [`MAX_SLOTS_LIMIT`].
    #[error("max slots must be between 1 and {MAX_SLOTS_LIMIT}")]
    Slots,
}

/// US dollars per million tokens, as the plain decimal string the owner typed.
///
/// Kept as a string so it round-trips exactly and is compared exactly; only the value sent to
/// LiteLLM is a float.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Price(String);

impl Price {
    /// Accepts `0`, `0.5`, `12.345678`: digits, an optional dot and up to 6 decimals.
    pub fn parse(raw: &str) -> Result<Self, SettingsError> {
        let (whole, fraction) = raw.split_once('.').unwrap_or((raw, ""));
        let digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
        let valid = (1..=9).contains(&whole.len())
            && digits(whole)
            && fraction.len() <= 6
            && digits(fraction)
            && (raw.contains('.') != fraction.is_empty());
        if valid {
            Ok(Self(raw.to_owned()))
        } else {
            Err(SettingsError::Price(raw.chars().take(16).collect()))
        }
    }

    /// The price as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Dollars per token, the unit LiteLLM costs use.
    #[must_use]
    pub fn usd_per_token(&self) -> f64 {
        // Validated at construction, so the parse cannot fail.
        self.0.parse::<f64>().unwrap_or(0.0) / 1_000_000.0
    }
}

impl TryFrom<String> for Price {
    type Error = SettingsError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::parse(&raw)
    }
}

impl From<Price> for String {
    fn from(price: Price) -> Self {
        price.0
    }
}

/// What an owner charges for a model. Cache prices exist only when the owner set them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pricing {
    /// Input tokens, per million.
    pub input_usd_per_mtok: Price,
    /// Output tokens, per million.
    pub output_usd_per_mtok: Price,
    /// Cached input tokens read, per million.
    #[serde(default)]
    pub cache_read_usd_per_mtok: Option<Price>,
    /// Input tokens written to the cache, per million.
    #[serde(default)]
    pub cache_write_usd_per_mtok: Option<Price>,
}

/// What an owner decided for one model. `None` fields leave LiteLLM's behaviour as it is: no
/// price means no cost (served free) and no slot limit means unlimited.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSettings {
    /// Parallel requests allowed on the model.
    #[serde(default)]
    pub max_slots: Option<u32>,
    /// Prices.
    #[serde(default)]
    pub pricing: Option<Pricing>,
}

impl ModelSettings {
    /// Rejects a slot limit outside its bounds; prices were checked when they were parsed.
    pub fn validated(self) -> Result<Self, SettingsError> {
        match self.max_slots {
            Some(slots) if slots == 0 || slots > MAX_SLOTS_LIMIT => Err(SettingsError::Slots),
            _ => Ok(self),
        }
    }
}

/// The settings of one model, by its published name (`{account}/{model}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSettingsEntry {
    /// `{account}/{model}`.
    pub model: String,
    /// The owner's decision.
    #[serde(flatten)]
    pub settings: ModelSettings,
}

/// The receiver's answer to a push: the settings of every model of the snapshot that has any.
///
/// Authoritative: a model absent from `settings` has none (unpriced, unlimited).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct InventoryAnswer {
    /// The settings to apply.
    pub settings: Vec<ModelSettingsEntry>,
}

/// A model one node serves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryModel {
    /// `{account}/{model}`.
    pub name: String,
    /// Capability tags.
    pub tags: Vec<ModelTag>,
}

/// One connected node. A node that is absent from a snapshot is offline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InventoryNode {
    /// The agent instance id.
    pub node_id: String,
    /// Subject of the token the agent enrolled with: the owner.
    pub owner_sub: String,
    /// Account name used in model aliases, for display.
    pub owner_name: String,
    /// What the node reported about its host, if it reports it.
    pub hardware: Option<HardwareInfo>,
    /// Its live status, if it reports it.
    pub status: Option<AgentStatus>,
    /// The models it serves.
    pub models: Vec<InventoryModel>,
}

/// The complete state of one relay: nodes and models absent from it are gone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InventorySnapshot {
    /// Which relay this is.
    pub relay_id: String,
    /// Unix time in milliseconds; the receiver refuses an older snapshot of the same relay.
    pub generated_at: u64,
    /// Connected nodes.
    pub nodes: Vec<InventoryNode>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices_are_plain_decimals_with_at_most_six_decimals() {
        for good in ["0", "0.5", "12.345678", "999999999"] {
            assert!(Price::parse(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "-1",
            "1e3",
            "1.",
            ".5",
            "0.1234567",
            "1,5",
            " 1",
            "1 ",
            "NaN",
            "1000000000",
            "$1",
        ] {
            assert!(Price::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_price_is_converted_to_dollars_per_token() {
        assert!((Price::parse("0.5").unwrap().usd_per_token() - 5e-7).abs() < 1e-15);
        assert_eq!(Price::parse("0").unwrap().usd_per_token(), 0.0);
    }

    #[test]
    fn an_invalid_price_in_an_answer_is_a_parse_error() {
        let bad = r#"{"settings":[{"model":"u-a/m","pricing":{"inputUsdPerMtok":"-1","outputUsdPerMtok":"1"}}]}"#;
        assert!(serde_json::from_str::<InventoryAnswer>(bad).is_err());
    }

    #[test]
    fn an_answer_carries_optional_settings_per_model() {
        let json = r#"{"changed":true,"settings":[
            {"model":"u-a/m","maxSlots":4,"pricing":{"inputUsdPerMtok":"0.3","outputUsdPerMtok":"1.2","cacheReadUsdPerMtok":"0.03"}},
            {"model":"u-a/n"}]}"#;
        let answer: InventoryAnswer = serde_json::from_str(json).unwrap();
        assert_eq!(answer.settings[0].settings.max_slots, Some(4));
        let pricing = answer.settings[0].settings.pricing.as_ref().unwrap();
        assert_eq!(
            pricing.cache_read_usd_per_mtok.as_ref().unwrap().as_str(),
            "0.03"
        );
        assert!(pricing.cache_write_usd_per_mtok.is_none());
        assert_eq!(answer.settings[1].settings, ModelSettings::default());
    }

    #[test]
    fn the_slot_limit_is_bounded() {
        let slots = |n| ModelSettings {
            max_slots: Some(n),
            pricing: None,
        };
        assert_eq!(slots(0).validated(), Err(SettingsError::Slots));
        assert_eq!(
            slots(MAX_SLOTS_LIMIT + 1).validated(),
            Err(SettingsError::Slots)
        );
        assert!(slots(1).validated().is_ok());
        assert!(ModelSettings::default().validated().is_ok());
    }
}
