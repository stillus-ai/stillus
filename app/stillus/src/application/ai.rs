// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

use crate::settings::GlobalSettingsStore;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use stillus_ai::{AiConnection, AiError, AiProfile, AiSettings, ApiKey, CatalogTransport};
use stillus_platform::credentials::CredentialStore;
use zeroize::Zeroizing;

pub(crate) enum Action {
    Connect(Zeroizing<String>),
    Refresh,
    Disconnect,
    Save {
        old: Option<String>,
        name: String,
        profile: AiProfile,
    },
    Remove(String),
    Cleanup,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Failure {
    Api(AiError),
    Credentials,
    Settings,
    Cancelled,
    Cleanup,
}

pub(crate) fn start(
    home: std::path::PathBuf,
    expected: AiSettings,
    action: Action,
    cancellation: std::sync::Arc<AtomicBool>,
) -> std::sync::mpsc::Receiver<Result<AiSettings, Failure>> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        #[cfg(feature = "test-utils")]
        if std::env::var("STILLUS_TEST_AI").as_deref() == Ok("1") {
            let result = execute(
                &home,
                expected,
                action,
                &cancellation,
                &fixtures::Catalog,
                &fixtures::Vault,
            );
            let _ = sender.send(result);
            return;
        }
        let result = execute(
            &home,
            expected,
            action,
            &cancellation,
            &stillus_ai::provider::RegistryCatalogTransport,
            &stillus_platform::credentials::SystemCredentials,
        );
        let _ = sender.send(result);
    });
    receiver
}

pub(crate) fn execute(
    home: &Path,
    expected: AiSettings,
    action: Action,
    cancelled: &AtomicBool,
    transport: &dyn CatalogTransport,
    credentials: &dyn CredentialStore,
) -> Result<AiSettings, Failure> {
    let current = GlobalSettingsStore::load(Some(home));
    if current.diagnostic.is_some() || current.settings.ai != expected {
        return Err(Failure::Settings);
    }
    if cancelled.load(Ordering::Acquire) {
        return Err(Failure::Cancelled);
    }
    let journal = crate::ai_journal::FileJournal::for_home(home);
    static NEXT_OPERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let operation = format!(
        "ai/catalog/{:x}/{:x}/{:x}",
        std::process::id(),
        stillus_ai::journal::now_ms(),
        NEXT_OPERATION.fetch_add(1, Ordering::Relaxed)
    );
    let mut wanted = expected.clone();
    let mut inserted = None;
    match action {
        Action::Connect(value) => {
            let (provider, key) = ApiKey::parse(value).map_err(Failure::Api)?;
            let models = transport
                .list_recorded(provider.clone(), &key, journal.as_ref(), &operation)
                .map_err(Failure::Api)?;
            if cancelled.load(Ordering::Acquire) {
                return Err(Failure::Cancelled);
            }
            let reference = credentials
                .insert(key.expose())
                .map_err(|_| Failure::Credentials)?;
            inserted = Some(reference.clone());
            if let Some(old) = &expected.connection {
                wanted.pending_deletions.push(old.credential.clone());
            }
            wanted.connect(AiConnection {
                provider,
                credential: reference,
                models,
                checked_at: now(),
            });
        }
        Action::Refresh => {
            let connection = wanted
                .connection
                .as_mut()
                .ok_or(Failure::Api(AiError::Incomplete))?;
            let value = credentials
                .read(&connection.credential)
                .map_err(|_| Failure::Credentials)?;
            let (provider, key) = ApiKey::parse(value).map_err(Failure::Api)?;
            if provider != connection.provider {
                return Err(Failure::Api(AiError::KeyFormat));
            }
            connection.models = transport
                .list_recorded(provider.clone(), &key, journal.as_ref(), &operation)
                .map_err(Failure::Api)?;
            connection.checked_at = now();
        }
        Action::Disconnect => {
            if let Some(old) = wanted.connection.take() {
                wanted.pending_deletions.push(old.credential);
            }
            wanted.aliases.clear();
        }
        Action::Save { old, name, profile } => {
            wanted
                .save_alias(old.as_deref(), name, profile)
                .map_err(Failure::Api)?;
        }
        Action::Remove(name) => wanted.remove_alias(&name).map_err(Failure::Api)?,
        Action::Cleanup => {}
    }
    let mut store = GlobalSettingsStore::load(Some(home)).store;
    let failure = if cancelled.load(Ordering::Acquire) {
        Some(Failure::Cancelled)
    } else if store.set_ai(&expected, wanted.clone()).is_err() {
        Some(Failure::Settings)
    } else {
        None
    };
    if let Some(failure) = failure {
        if let Some(reference) = inserted {
            // A rename may succeed before a directory flush reports failure.
            // Never remove a credential that the durable config might reference.
            let current = GlobalSettingsStore::load(Some(home));
            if current.diagnostic.is_none()
                && current
                    .settings
                    .ai
                    .connection
                    .as_ref()
                    .is_none_or(|c| c.credential != reference)
                && credentials.delete(&reference).is_err()
            {
                return Err(Failure::Cleanup);
            }
        }
        return Err(failure);
    }
    // Keep failed removals in the config, so retry remains possible after restart.
    let mut cleaned = wanted.clone();
    cleaned.pending_deletions.retain(|reference| {
        wanted
            .connection
            .as_ref()
            .is_some_and(|c| c.credential == *reference)
            || credentials.delete(reference).is_err()
    });
    if cleaned != wanted && store.set_ai(&wanted, cleaned.clone()).is_ok() {
        wanted = cleaned;
    }
    Ok(wanted)
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// XTEST fixtures are compiled only into the explicit test-utils build.
#[cfg(feature = "test-utils")]
pub(crate) mod fixtures {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::{Mutex, atomic::AtomicU64};
    use stillus_ai::{AiEffort, AiModel, AiProvider};
    use stillus_platform::credentials::CredentialError;
    static KEYS: Mutex<BTreeMap<String, Zeroizing<String>>> = Mutex::new(BTreeMap::new());
    static NEXT: AtomicU64 = AtomicU64::new(0);
    pub(crate) struct Vault;
    impl CredentialStore for Vault {
        fn insert(&self, value: &str) -> Result<String, CredentialError> {
            let reference = format!("ai/key/{:032x}", NEXT.fetch_add(1, Ordering::Relaxed));
            KEYS.lock()
                .map_err(|_| CredentialError)?
                .insert(reference.clone(), Zeroizing::new(value.into()));
            Ok(reference)
        }
        fn read(&self, reference: &str) -> Result<Zeroizing<String>, CredentialError> {
            KEYS.lock()
                .map_err(|_| CredentialError)?
                .get(reference)
                .cloned()
                .ok_or(CredentialError)
        }
        fn delete(&self, reference: &str) -> Result<(), CredentialError> {
            KEYS.lock().map_err(|_| CredentialError)?.remove(reference);
            Ok(())
        }
    }
    pub(crate) struct Catalog;
    impl CatalogTransport for Catalog {
        fn list(&self, provider: AiProvider, key: &ApiKey) -> Result<Vec<AiModel>, AiError> {
            if key.expose().ends_with("denied") {
                return Err(AiError::Unauthorized);
            }
            if key.expose().ends_with("slow") {
                std::thread::sleep(std::time::Duration::from_millis(600));
            }
            Ok(match provider {
                AiProvider::Other(_) => return Err(AiError::Unsupported),
                AiProvider::OpenAi => vec![
                    AiModel {
                        id: "gpt-5.6-luna".into(),
                        name: "GPT-5.6 Luna".into(),
                        efforts: vec![AiEffort::Low, AiEffort::High],
                    },
                    AiModel {
                        id: "gpt-5.6-sol".into(),
                        name: "GPT-5.6 Sol".into(),
                        efforts: vec![AiEffort::High, AiEffort::Max],
                    },
                    AiModel {
                        id: "gpt-4.1".into(),
                        name: "GPT-4.1".into(),
                        efforts: vec![],
                    },
                ],
                AiProvider::Anthropic => vec![AiModel {
                    id: "claude-sonnet-5".into(),
                    name: "Claude Sonnet 5".into(),
                    efforts: vec![AiEffort::Low, AiEffort::High],
                }],
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use stillus_ai::{AiEffort, AiModel, AiProvider};
    use stillus_platform::credentials::CredentialError;
    static NEXT_REFERENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    struct Vault(Mutex<std::collections::BTreeMap<String, String>>);
    impl CredentialStore for Vault {
        fn insert(&self, value: &str) -> Result<String, CredentialError> {
            let mut entries = self.0.lock().unwrap();
            let reference = format!("ai/key/{}", NEXT_REFERENCE.fetch_add(1, Ordering::Relaxed));
            entries.insert(reference.clone(), value.to_owned());
            Ok(reference)
        }
        fn read(&self, reference: &str) -> Result<Zeroizing<String>, CredentialError> {
            self.0
                .lock()
                .unwrap()
                .get(reference)
                .cloned()
                .map(Zeroizing::new)
                .ok_or(CredentialError)
        }
        fn delete(&self, reference: &str) -> Result<(), CredentialError> {
            self.0.lock().unwrap().remove(reference);
            Ok(())
        }
    }
    struct Catalog;
    impl CatalogTransport for Catalog {
        fn list(&self, _: AiProvider, _: &ApiKey) -> Result<Vec<AiModel>, AiError> {
            Ok(vec![AiModel {
                id: "gpt-5.6-luna".into(),
                name: "Luna".into(),
                efforts: vec![AiEffort::High],
            }])
        }
    }
    fn home() -> std::path::PathBuf {
        home_at(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        )
    }
    fn home_at(timestamp: u128) -> std::path::PathBuf {
        static NEXT_HOME: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "stillus-ai-test-{}-{timestamp}-{}",
            std::process::id(),
            NEXT_HOME.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir(&path).unwrap();
        path
    }
    #[test]
    fn parallel_test_homes_are_isolated_when_the_clock_does_not_advance() {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let start = std::sync::Barrier::new(16);
        let paths = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..16)
                .map(|_| {
                    scope.spawn(|| {
                        start.wait();
                        home_at(timestamp)
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(
            paths
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            16
        );
        for path in paths {
            assert_eq!(std::fs::read_dir(&path).unwrap().count(), 0);
            std::fs::remove_dir(path).unwrap();
        }
    }
    #[test]
    fn connect_save_restart_conflict_and_disconnect_preserve_secret_boundary() {
        let home = home();
        let vault = Vault(Mutex::default());
        let cancelled = AtomicBool::new(false);
        let key = "sk-proj-abcdefghijklmnopqrstuv";
        let connected = execute(
            &home,
            AiSettings::default(),
            Action::Connect(Zeroizing::new(key.into())),
            &cancelled,
            &Catalog,
            &vault,
        )
        .unwrap();
        assert_eq!(connected.aliases.len(), 1);
        assert!(
            !std::fs::read_to_string(home.join(".stillus.cfg"))
                .unwrap()
                .contains(key)
        );
        let profile = AiProfile {
            model: "gpt-5.6-luna".into(),
            effort: Some(AiEffort::High),
        };
        let saved = execute(
            &home,
            connected.clone(),
            Action::Save {
                old: None,
                name: "mini".into(),
                profile,
            },
            &cancelled,
            &Catalog,
            &vault,
        )
        .unwrap();
        assert_eq!(GlobalSettingsStore::load(Some(&home)).store.ai(), saved);
        assert_eq!(saved.aliases.len(), 2);
        assert_eq!(saved.resolve("missing"), saved.resolve("default"));
        assert_eq!(
            execute(
                &home,
                connected,
                Action::Disconnect,
                &cancelled,
                &Catalog,
                &vault
            ),
            Err(Failure::Settings)
        );
        assert_eq!(vault.0.lock().unwrap().len(), 1);
        let disconnected = execute(
            &home,
            saved,
            Action::Disconnect,
            &cancelled,
            &Catalog,
            &vault,
        )
        .unwrap();
        assert!(disconnected.connection.is_none());
        assert!(vault.0.lock().unwrap().is_empty());
        std::fs::remove_dir_all(home).unwrap();
    }
    #[test]
    fn alias_mutations_are_atomic_and_default_survives_deletion() {
        let home = home();
        let vault = Vault(Mutex::default());
        let flag = AtomicBool::new(false);
        let connected = execute(
            &home,
            AiSettings::default(),
            Action::Connect(Zeroizing::new("sk-proj-abcdefghijklmnopqrstuv".into())),
            &flag,
            &Catalog,
            &vault,
        )
        .unwrap();
        let profile = connected.resolve("default").unwrap().clone();
        let added = execute(
            &home,
            connected,
            Action::Save {
                old: None,
                name: "mini".into(),
                profile: profile.clone(),
            },
            &flag,
            &Catalog,
            &vault,
        )
        .unwrap();
        let bytes = std::fs::read(home.join(".stillus.cfg")).unwrap();
        assert_eq!(
            execute(
                &home,
                added.clone(),
                Action::Remove("default".into()),
                &flag,
                &Catalog,
                &vault
            ),
            Err(Failure::Api(AiError::DefaultAlias))
        );
        assert_eq!(std::fs::read(home.join(".stillus.cfg")).unwrap(), bytes);
        assert_eq!(
            execute(
                &home,
                added.clone(),
                Action::Save {
                    old: Some("mini".into()),
                    name: "default".into(),
                    profile
                },
                &flag,
                &Catalog,
                &vault
            ),
            Err(Failure::Api(AiError::AliasExists))
        );
        assert_eq!(std::fs::read(home.join(".stillus.cfg")).unwrap(), bytes);
        let removed = execute(
            &home,
            added,
            Action::Remove("mini".into()),
            &flag,
            &Catalog,
            &vault,
        )
        .unwrap();
        let loaded = GlobalSettingsStore::load(Some(&home)).store.ai();
        assert_eq!(loaded, removed);
        assert_eq!(loaded.resolve("mini"), loaded.resolve("default"));
        assert_eq!(vault.0.lock().unwrap().len(), 1);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn failed_authentication_cancellation_and_corrupt_config_do_not_replace_connection() {
        struct Denied;
        impl CatalogTransport for Denied {
            fn list(&self, _: AiProvider, _: &ApiKey) -> Result<Vec<AiModel>, AiError> {
                Err(AiError::Unauthorized)
            }
        }
        let home = home();
        let vault = Vault(Mutex::default());
        let key = || Action::Connect(Zeroizing::new("sk-proj-abcdefghijklmnopqrstuv".into()));
        assert_eq!(
            execute(
                &home,
                AiSettings::default(),
                key(),
                &AtomicBool::new(false),
                &Denied,
                &vault
            ),
            Err(Failure::Api(AiError::Unauthorized))
        );
        assert_eq!(
            execute(
                &home,
                AiSettings::default(),
                key(),
                &AtomicBool::new(true),
                &Catalog,
                &vault
            ),
            Err(Failure::Cancelled)
        );
        std::fs::write(home.join(".stillus.cfg"), b"corrupt").unwrap();
        assert_eq!(
            execute(
                &home,
                AiSettings::default(),
                key(),
                &AtomicBool::new(false),
                &Catalog,
                &vault
            ),
            Err(Failure::Settings)
        );
        assert_eq!(
            std::fs::read(home.join(".stillus.cfg")).unwrap(),
            b"corrupt"
        );
        assert!(vault.0.lock().unwrap().is_empty());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn failed_refresh_and_vault_access_preserve_the_last_verified_configuration() {
        struct Offline;
        impl CatalogTransport for Offline {
            fn list(&self, _: AiProvider, _: &ApiKey) -> Result<Vec<AiModel>, AiError> {
                Err(AiError::Network)
            }
        }
        let home = home();
        let vault = Vault(Mutex::default());
        let flag = AtomicBool::new(false);
        let original = execute(
            &home,
            AiSettings::default(),
            Action::Connect(Zeroizing::new("sk-proj-abcdefghijklmnopqrstuv".into())),
            &flag,
            &Catalog,
            &vault,
        )
        .unwrap();
        let bytes = std::fs::read(home.join(".stillus.cfg")).unwrap();
        assert_eq!(
            execute(
                &home,
                original.clone(),
                Action::Refresh,
                &flag,
                &Offline,
                &vault
            ),
            Err(Failure::Api(AiError::Network))
        );
        let empty_vault = Vault(Mutex::default());
        assert_eq!(
            execute(
                &home,
                original,
                Action::Refresh,
                &flag,
                &Catalog,
                &empty_vault
            ),
            Err(Failure::Credentials)
        );
        assert_eq!(std::fs::read(home.join(".stillus.cfg")).unwrap(), bytes);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn failed_deletion_is_retryable_after_restart_and_future_fields_survive() {
        struct Locked<'a>(&'a Vault);
        impl CredentialStore for Locked<'_> {
            fn insert(&self, value: &str) -> Result<String, CredentialError> {
                self.0.insert(value)
            }
            fn read(&self, reference: &str) -> Result<Zeroizing<String>, CredentialError> {
                self.0.read(reference)
            }
            fn delete(&self, _: &str) -> Result<(), CredentialError> {
                Err(CredentialError)
            }
        }
        let home = home();
        std::fs::write(
            home.join(".stillus.cfg"),
            br#"{"version":1,"locale":"ru","last_workspace":null,"future":{"preserve":true}}"#,
        )
        .unwrap();
        let vault = Vault(Mutex::default());
        let flag = AtomicBool::new(false);
        let connected = execute(
            &home,
            AiSettings::default(),
            Action::Connect(Zeroizing::new("sk-proj-abcdefghijklmnopqrstuv".into())),
            &flag,
            &Catalog,
            &vault,
        )
        .unwrap();
        let disconnected = execute(
            &home,
            connected,
            Action::Disconnect,
            &flag,
            &Catalog,
            &Locked(&vault),
        )
        .unwrap();
        assert!(disconnected.connection.is_none());
        assert_eq!(disconnected.pending_deletions.len(), 1);
        let loaded = GlobalSettingsStore::load(Some(&home)).store.ai();
        assert_eq!(loaded, disconnected);
        let cleaned = execute(&home, loaded, Action::Cleanup, &flag, &Catalog, &vault).unwrap();
        assert!(cleaned.pending_deletions.is_empty());
        assert!(vault.0.lock().unwrap().is_empty());
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(home.join(".stillus.cfg")).unwrap()).unwrap();
        assert_eq!(json["future"]["preserve"], true);
        assert_eq!(json["locale"], "ru");
        std::fs::remove_dir_all(home).unwrap();
    }
}
