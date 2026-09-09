// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

//! Shared AI configuration and model catalogs. This crate never reads notes.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use zeroize::Zeroizing;

pub mod journal;
mod models;
mod transport;
pub use transport::{CatalogTransport, HttpsCatalogTransport};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AiProvider {
    OpenAi,
    Anthropic,
}

impl AiProvider {
    pub fn name(self) -> &'static str {
        match self {
            Self::OpenAi => "OpenAI",
            Self::Anthropic => "Anthropic",
        }
    }
}

/// Not serializable, and deliberately has no Debug implementation.
pub struct ApiKey(Zeroizing<String>);

impl ApiKey {
    pub fn parse(value: Zeroizing<String>) -> Result<(AiProvider, Self), AiError> {
        let value = Zeroizing::new(value.trim().to_owned());
        let provider = detect_provider(&value).ok_or(AiError::KeyFormat)?;
        Ok((provider, Self(value)))
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

pub fn detect_provider(value: &str) -> Option<AiProvider> {
    let value = value.trim();
    if !(24..=4096).contains(&value.len())
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    if value.starts_with("sk-ant-api") {
        Some(AiProvider::Anthropic)
    } else if value.starts_with("sk-ant-")
        || value.starts_with("sk-or-")
        || value.starts_with("sk-admin-")
    {
        None
    } else if value.starts_with("sk-proj-")
        || value.starts_with("sk-svcacct-")
        || (value.starts_with("sk-") && !value[3..].contains('-'))
    {
        Some(AiProvider::OpenAi)
    } else {
        None
    }
}

pub const DEFAULT_ALIAS: &str = "default";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AiEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}
impl AiEffort {
    pub const ALL: [Self; 7] = [
        Self::None,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::Xhigh,
        Self::Max,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct AiModel {
    pub id: String,
    pub name: String,
    /// Empty means that the model manages reasoning itself.
    pub efforts: Vec<AiEffort>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AiProfile {
    pub model: String,
    pub effort: Option<AiEffort>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AiConnection {
    pub provider: AiProvider,
    pub credential: String,
    pub models: Vec<AiModel>,
    pub checked_at: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(from = "StoredSettings")]
pub struct AiSettings {
    pub connection: Option<AiConnection>,
    pub aliases: BTreeMap<String, AiProfile>,
    /// Retained until the system store confirms deletion; never contains keys.
    pub pending_deletions: Vec<String>,
    #[serde(flatten)]
    pub additional: BTreeMap<String, serde_json::Value>,
}

// Supply the built-in alias in memory; reading settings never rewrites the file.
#[derive(Default, Deserialize)]
#[serde(default)]
struct StoredSettings {
    connection: Option<AiConnection>,
    aliases: BTreeMap<String, AiProfile>,
    pending_deletions: Vec<String>,
    #[serde(flatten)]
    additional: BTreeMap<String, serde_json::Value>,
}

impl From<StoredSettings> for AiSettings {
    fn from(stored: StoredSettings) -> Self {
        let mut settings = Self {
            connection: stored.connection,
            aliases: stored.aliases,
            pending_deletions: stored.pending_deletions,
            additional: stored.additional,
        };
        // The unused fixed task profiles are intentionally retired, not migrated.
        settings.additional.remove("profiles");
        settings.ensure_default();
        settings
    }
}

impl AiSettings {
    pub fn is_empty(&self) -> bool {
        self.connection.is_none()
            && self.aliases.is_empty()
            && self.pending_deletions.is_empty()
            && self.additional.is_empty()
    }
    /// Resolve a product alias. Only a missing name falls back to default;
    /// an existing but unavailable selection is an error, never a silent substitution.
    pub fn resolve(&self, alias: &str) -> Result<&AiProfile, AiError> {
        let profile = self
            .aliases
            .get(alias)
            .or_else(|| self.aliases.get(DEFAULT_ALIAS))
            .ok_or(AiError::Incomplete)?;
        self.validate_profile(profile)?;
        Ok(profile)
    }

    pub fn validate_alias_name(&self, old: Option<&str>, name: &str) -> Result<(), AiError> {
        if name.is_empty() || name.trim() != name || name.chars().any(char::is_control) {
            return Err(AiError::AliasName);
        }
        if old == Some(DEFAULT_ALIAS) && name != DEFAULT_ALIAS {
            return Err(AiError::DefaultAlias);
        }
        if old != Some(name) && self.aliases.contains_key(name) {
            return Err(AiError::AliasExists);
        }
        if old.is_some_and(|old| !self.aliases.contains_key(old)) {
            return Err(AiError::Incomplete);
        }
        Ok(())
    }

    pub fn save_alias(
        &mut self,
        old: Option<&str>,
        name: String,
        profile: AiProfile,
    ) -> Result<(), AiError> {
        self.validate_alias_name(old, &name)?;
        self.validate_profile(&profile)?;
        if let Some(old) = old {
            self.aliases.remove(old);
        }
        self.aliases.insert(name, profile);
        Ok(())
    }

    pub fn remove_alias(&mut self, name: &str) -> Result<(), AiError> {
        if name == DEFAULT_ALIAS {
            return Err(AiError::DefaultAlias);
        }
        self.aliases.remove(name).ok_or(AiError::Incomplete)?;
        Ok(())
    }

    fn ensure_default(&mut self) {
        let Some(connection) = &self.connection else {
            return;
        };
        // Catalogs do not provide a portable "mini tier" field. Keep reviewed
        // defaults explicit, and prefer an available dated snapshot of that model.
        let preferred = match connection.provider {
            AiProvider::OpenAi => "gpt-5.6-luna",
            AiProvider::Anthropic => "claude-sonnet-5",
        };
        let available = connection
            .models
            .iter()
            .filter(|model| model.id == preferred || models::is_snapshot(&model.id, preferred))
            .max_by_key(|model| (model.id == preferred, &model.id));
        self.aliases
            .entry(DEFAULT_ALIAS.into())
            .or_insert_with(|| AiProfile {
                model: available
                    .map(|model| model.id.clone())
                    .unwrap_or_else(|| preferred.into()),
                effort: Some(AiEffort::High),
            });
    }

    pub fn validate_profile(&self, profile: &AiProfile) -> Result<(), AiError> {
        let model = self
            .connection
            .as_ref()
            .ok_or(AiError::Incomplete)?
            .models
            .iter()
            .find(|model| model.id == profile.model)
            .ok_or(AiError::ModelUnavailable)?;
        if if model.efforts.is_empty() {
            profile.effort.is_none()
        } else {
            profile
                .effort
                .is_some_and(|effort| model.efforts.contains(&effort))
        } {
            Ok(())
        } else {
            Err(AiError::EffortRequired)
        }
    }
    pub fn connect(&mut self, connection: AiConnection) {
        if self
            .connection
            .as_ref()
            .is_some_and(|old| old.provider != connection.provider)
        {
            self.aliases.clear();
        }
        self.connection = Some(connection);
        self.ensure_default();
        // Retain unavailable selections visibly; resolve() refuses to use them.
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AiError {
    AliasName,
    AliasExists,
    DefaultAlias,
    KeyFormat,
    Unauthorized,
    Forbidden,
    RateLimited,
    Network,
    Journal,
    Response,
    NoModels,
    ModelUnavailable,
    EffortRequired,
    Incomplete,
}

#[cfg(test)]
mod tests {
    use super::*;
    fn connected() -> AiSettings {
        let mut settings = AiSettings::default();
        settings.connect(AiConnection {
            provider: AiProvider::OpenAi,
            credential: "ai/key/test".into(),
            models: vec![AiModel {
                id: "gpt-5.6-luna".into(),
                name: "GPT-5.6 Luna".into(),
                efforts: vec![AiEffort::High],
            }],
            checked_at: 1,
        });
        settings
    }
    #[test]
    fn default_is_automatic_and_custom_aliases_are_unbounded() {
        let mut settings = connected();
        assert_eq!(settings.aliases.len(), 1);
        assert_eq!(
            settings.resolve(DEFAULT_ALIAS).unwrap().effort,
            Some(AiEffort::High)
        );
        let profile = settings.resolve(DEFAULT_ALIAS).unwrap().clone();
        for index in 0..1000 {
            settings
                .save_alias(None, format!("Моя задача {index}"), profile.clone())
                .unwrap();
        }
        assert_eq!(settings.aliases.len(), 1001);
        let mut incomplete = profile;
        incomplete.effort = None;
        assert_eq!(
            settings.save_alias(None, "mini".into(), incomplete),
            Err(AiError::EffortRequired)
        );
    }

    #[test]
    fn default_is_protected_and_deleted_or_renamed_aliases_fall_back() {
        let mut settings = connected();
        let profile = settings.resolve(DEFAULT_ALIAS).unwrap().clone();
        assert_eq!(
            settings.remove_alias(DEFAULT_ALIAS),
            Err(AiError::DefaultAlias)
        );
        assert_eq!(
            settings.save_alias(Some(DEFAULT_ALIAS), "new".into(), profile.clone()),
            Err(AiError::DefaultAlias)
        );
        settings
            .save_alias(None, "mini".into(), profile.clone())
            .unwrap();
        assert_eq!(
            settings.save_alias(None, "mini".into(), profile.clone()),
            Err(AiError::AliasExists)
        );
        settings
            .save_alias(Some("mini"), "simple".into(), profile.clone())
            .unwrap();
        assert!(!settings.aliases.contains_key("mini"));
        assert_eq!(settings.resolve("mini"), settings.resolve(DEFAULT_ALIAS));
        settings.remove_alias("simple").unwrap();
        assert_eq!(settings.resolve("simple"), settings.resolve(DEFAULT_ALIAS));
        assert_eq!(settings.resolve("unknown"), settings.resolve(DEFAULT_ALIAS));
        for name in ["", " ", " trailing ", "line\nbreak"] {
            assert_eq!(
                settings.save_alias(None, name.into(), profile.clone()),
                Err(AiError::AliasName)
            );
        }
    }

    #[test]
    fn retired_profiles_are_ignored_and_unknown_settings_survive() {
        let mut raw = serde_json::to_value(connected()).unwrap();
        let aliases = raw.as_object_mut().unwrap().remove("aliases").unwrap();
        raw["profiles"] = serde_json::json!({"small": aliases["default"]});
        raw["future"] = serde_json::json!({"keep": true});
        let loaded: AiSettings = serde_json::from_value(raw).unwrap();
        assert!(!loaded.aliases.contains_key("small"));
        assert!(loaded.aliases.contains_key(DEFAULT_ALIAS));
        let serialized = serde_json::to_value(&loaded).unwrap();
        assert!(serialized.get("profiles").is_none());
        assert_eq!(serialized["future"]["keep"], true);
        assert_eq!(
            serde_json::from_value::<AiSettings>(serialized).unwrap(),
            loaded
        );
    }

    #[test]
    fn provider_detection_never_probes_multiple_services() {
        assert_eq!(
            detect_provider("sk-ant-api03-abcdefghijklmnop"),
            Some(AiProvider::Anthropic)
        );
        assert_eq!(
            detect_provider("sk-proj-abcdefghijklmnopqrstuv"),
            Some(AiProvider::OpenAi)
        );
        for key in [
            "sk-ant-oat01-abcdefghijklmnopqrstuv",
            "sk-or-v1-abcdefghijklmnopqrstuv",
            "unknown",
            "sk-proj-abcdefghijklmn\nxyz",
        ] {
            assert_eq!(detect_provider(key), None);
        }
    }
    #[test]
    fn connection_changes_preserve_custom_defaults_and_unavailable_selections() {
        let mut settings = connected();
        let profile = settings.resolve(DEFAULT_ALIAS).unwrap().clone();
        settings.save_alias(None, "mini".into(), profile).unwrap();
        let mut connection = settings.connection.clone().unwrap();
        connection.models.clear();
        settings.connect(connection.clone());
        assert_eq!(settings.resolve("mini"), Err(AiError::ModelUnavailable));
        assert_eq!(settings.resolve("unknown"), Err(AiError::ModelUnavailable));
        assert_eq!(settings.aliases.len(), 2);
        connection.provider = AiProvider::Anthropic;
        connection.models.push(AiModel {
            id: "claude-sonnet-5".into(),
            name: "Sonnet 5".into(),
            efforts: vec![AiEffort::High],
        });
        settings.connect(connection);
        assert_eq!(settings.aliases.len(), 1);
        assert_eq!(
            settings.resolve(DEFAULT_ALIAS).unwrap().model,
            "claude-sonnet-5"
        );
    }

    #[test]
    fn default_uses_available_snapshot_but_does_not_invent_availability() {
        let mut connection = connected().connection.unwrap();
        connection.models[0].id = "gpt-5.6-luna-2026-09-01".into();
        let mut settings = AiSettings::default();
        settings.connect(connection);
        assert_eq!(
            settings.resolve(DEFAULT_ALIAS).unwrap().model,
            "gpt-5.6-luna-2026-09-01"
        );
        settings.aliases.get_mut(DEFAULT_ALIAS).unwrap().model = "unavailable".into();
        let connection = settings.connection.clone().unwrap();
        settings.connect(connection);
        assert_eq!(
            settings.resolve(DEFAULT_ALIAS),
            Err(AiError::ModelUnavailable)
        );
    }
}
