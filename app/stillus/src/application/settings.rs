// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use stillus_platform::fs::{self, OpenOptions};

use crate::i18n::Locale;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct PublicSettings {
    pub locale: Locale,
    pub automatic: bool,
}
pub(crate) enum PublicEdit {
    Locale { expected: Locale, wanted: Locale },
    Automatic { expected: bool, wanted: bool },
}

pub(crate) const SETTINGS_VERSION: u32 = 1;
pub(crate) const DEFAULT_WINDOW_WIDTH: f64 = 1_240.0;
pub(crate) const DEFAULT_WINDOW_HEIGHT: f64 = 800.0;
pub(crate) const MIN_WINDOW_WIDTH: f64 = 960.0;
pub(crate) const MIN_WINDOW_HEIGHT: f64 = 600.0;
const MAX_WINDOW_DIMENSION: f64 = 16_384.0;
const SETTINGS_DIRECTORY: &str = ".stillus";
const SETTINGS_FILE: &str = "settings.json";
const GLOBAL_CONFIG_FILE: &str = ".stillus.cfg";
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// How the application looks for its own updates. Automatic checks are on by
/// default; `dismissed` remembers the version the user declined, so the same
/// release is offered once and not at every start.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct UpdateSettings {
    #[serde(default = "enabled")]
    pub(crate) automatic: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) dismissed: Option<String>,
}

fn enabled() -> bool {
    true
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            automatic: true,
            dismissed: None,
        }
    }
}

impl UpdateSettings {
    pub(crate) fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct GlobalSettings {
    pub(crate) version: u32,
    #[serde(default)]
    pub(crate) locale: Locale,
    pub(crate) last_workspace: Option<String>,
    #[serde(default, skip_serializing_if = "stillus_ai::AiSettings::is_empty")]
    pub(crate) ai: stillus_ai::AiSettings,
    #[serde(default, skip_serializing_if = "UpdateSettings::is_default")]
    pub(crate) updates: UpdateSettings,
    #[serde(flatten)]
    pub(crate) additional: BTreeMap<String, serde_json::Value>,
}

impl Default for GlobalSettings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            locale: Locale::default(),
            last_workspace: None,
            ai: stillus_ai::AiSettings::default(),
            updates: UpdateSettings::default(),
            additional: BTreeMap::new(),
        }
    }
}

impl GlobalSettings {
    pub(crate) fn workspace(&self) -> Option<PathBuf> {
        self.last_workspace.as_ref().and_then(|path| {
            let path = PathBuf::from(path);
            path.is_absolute().then_some(path)
        })
    }
}

pub(crate) struct GlobalSettingsLoad {
    pub(crate) store: GlobalSettingsStore,
    pub(crate) settings: GlobalSettings,
    pub(crate) diagnostic: Option<String>,
}

pub(crate) struct GlobalSettingsStore {
    home: Option<PathBuf>,
    settings: GlobalSettings,
}

impl GlobalSettingsStore {
    pub(crate) fn ai(&self) -> stillus_ai::AiSettings {
        self.settings.ai.clone()
    }

    pub(crate) fn home(&self) -> Option<PathBuf> {
        self.home.clone()
    }

    pub(crate) fn set_ai(
        &mut self,
        expected: &stillus_ai::AiSettings,
        wanted: stillus_ai::AiSettings,
    ) -> Result<(), SettingsError> {
        let home = self
            .home
            .as_deref()
            .ok_or_else(|| SettingsError::UnsafePath("global settings unavailable".into()))?;
        let _operation = stillus_platform::OperationLock::directory(home)?;
        let loaded = Self::load(Some(home));
        if loaded.diagnostic.is_some() || &loaded.settings.ai != expected {
            return Err(SettingsError::UnsafePath(
                "AI settings changed or could not be read; reopen settings".into(),
            ));
        }
        let mut settings = loaded.settings;
        settings.ai = wanted;
        atomic_write_global_settings(home, &settings)?;
        self.settings = settings;
        Ok(())
    }

    pub(crate) fn updates(&self) -> UpdateSettings {
        self.settings.updates.clone()
    }

    /// Update preferences are independent of everything else in the file, so
    /// a concurrent change elsewhere is merged instead of rejected.
    pub(crate) fn set_updates(&mut self, wanted: UpdateSettings) -> Result<(), SettingsError> {
        if self.settings.updates == wanted {
            return Ok(());
        }
        let home = self.home.as_deref().ok_or_else(|| {
            SettingsError::UnsafePath("HOME is unavailable; global config is disabled".to_owned())
        })?;
        let _operation = stillus_platform::OperationLock::directory(home)?;
        let mut settings = Self::load(Some(home)).settings;
        settings.updates = wanted;
        atomic_write_global_settings(home, &settings)?;
        self.settings = settings;
        Ok(())
    }

    pub(crate) fn public_settings(&self) -> PublicSettings {
        PublicSettings {
            locale: self.settings.locale,
            automatic: self.settings.updates.automatic,
        }
    }
    pub(crate) fn change_public(&mut self, edit: PublicEdit) -> Result<(), SettingsError> {
        let home = self
            .home
            .as_deref()
            .ok_or_else(|| SettingsError::UnsafePath("global settings unavailable".into()))?;
        let _operation = stillus_platform::OperationLock::directory(home)?;
        let loaded = Self::load(Some(home));
        if loaded.diagnostic.is_some() {
            return Err(SettingsError::Conflict);
        }
        let mut settings = loaded.settings;
        match edit {
            PublicEdit::Locale { expected, wanted } => {
                if settings.locale != expected {
                    return Err(SettingsError::Conflict);
                }
                settings.locale = wanted;
            }
            PublicEdit::Automatic { expected, wanted } => {
                if settings.updates.automatic != expected {
                    return Err(SettingsError::Conflict);
                }
                settings.updates.automatic = wanted;
            }
        }
        atomic_write_global_settings(home, &settings)?;
        self.settings = settings;
        Ok(())
    }
    pub(crate) fn locale(&self) -> Locale {
        self.settings.locale
    }

    pub(crate) fn set_locale(&mut self, locale: Locale) -> Result<(), SettingsError> {
        self.change_public(PublicEdit::Locale {
            expected: self.settings.locale,
            wanted: locale,
        })
    }

    pub(crate) fn load(home: Option<&Path>) -> GlobalSettingsLoad {
        let Some(home) = home else {
            return GlobalSettingsLoad {
                store: Self {
                    home: None,
                    settings: GlobalSettings::default(),
                },
                settings: GlobalSettings::default(),
                diagnostic: Some(
                    "HOME is unavailable; global workspace config is disabled".to_owned(),
                ),
            };
        };
        let path = home.join(GLOBAL_CONFIG_FILE);
        let (settings, diagnostic) = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<GlobalSettings>(&bytes) {
                Ok(mut settings) if settings.version == SETTINGS_VERSION => {
                    if settings.workspace().is_none() && settings.last_workspace.is_some() {
                        settings.last_workspace = None;
                    }
                    (settings, None)
                }
                Ok(settings) => (
                    GlobalSettings::default(),
                    Some(format!(
                        "global settings version {} is unsupported; workspace selection is required",
                        settings.version
                    )),
                ),
                Err(error) => (
                    GlobalSettings::default(),
                    Some(format!(
                        "global settings could not be read: {error}; workspace selection is required"
                    )),
                ),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                (GlobalSettings::default(), None)
            }
            Err(error) => (
                GlobalSettings::default(),
                Some(format!(
                    "global settings could not be opened: {error}; workspace selection is required"
                )),
            ),
        };
        GlobalSettingsLoad {
            store: Self {
                home: Some(home.to_path_buf()),
                settings: settings.clone(),
            },
            settings,
            diagnostic,
        }
    }

    pub(crate) fn remember_workspace(&mut self, workspace: &Path) -> Result<(), SettingsError> {
        if !workspace.is_absolute() {
            return Err(SettingsError::UnsafePath(format!(
                "global workspace path is not absolute: {}",
                workspace.display()
            )));
        }
        let Some(path) = workspace.to_str() else {
            return Err(SettingsError::UnsafePath(
                "global workspace path is not valid UTF-8".to_owned(),
            ));
        };
        let Some(home) = &self.home else {
            return Err(SettingsError::UnsafePath(
                "HOME is unavailable; global workspace config is disabled".to_owned(),
            ));
        };
        if self.settings.last_workspace.as_deref() == Some(path) {
            return Ok(());
        }
        let _operation = stillus_platform::OperationLock::directory(home)?;
        let mut settings = Self::load(Some(home)).settings;
        settings.last_workspace = Some(path.to_owned());
        atomic_write_global_settings(home, &settings)?;
        self.settings = settings;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct UiSettings {
    pub(crate) version: u32,
    pub(crate) window: WindowSettings,
    pub(crate) sidebar: SidebarSettings,
    pub(crate) selected_note: Option<String>,
    #[serde(default)]
    pub(crate) external_files: Vec<PersistedExternalFile>,
    #[serde(default)]
    pub(crate) selected_external: Option<String>,
    #[serde(default)]
    pub(crate) selected_rss: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) selected_chat: Option<String>,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            window: WindowSettings::default(),
            sidebar: SidebarSettings::default(),
            selected_note: None,
            external_files: Vec::new(),
            selected_external: None,
            selected_rss: None,
            selected_chat: None,
        }
    }
}

impl UiSettings {
    fn validated(mut self) -> Self {
        if !valid_dimension(self.window.width, MIN_WINDOW_WIDTH) {
            self.window.width = DEFAULT_WINDOW_WIDTH;
        }
        if !valid_dimension(self.window.height, MIN_WINDOW_HEIGHT) {
            self.window.height = DEFAULT_WINDOW_HEIGHT;
        }
        self.sidebar.width = self.sidebar.width.clamp(180.0, 480.0);
        if !self.sidebar.width.is_finite() {
            self.sidebar.width = 256.0;
        }
        self.sidebar.expanded.sort();
        self.sidebar.expanded.dedup();
        let mut seen_categories = std::collections::HashSet::new();
        self.sidebar
            .category_order
            .retain(|category| seen_categories.insert(category.clone()));
        let mut seen_note_sort = std::collections::HashSet::new();
        self.sidebar.note_sort.retain(|sort| {
            !sort.category.is_empty() && seen_note_sort.insert(sort.category.clone())
        });
        self.sidebar
            .note_sort
            .sort_by(|left, right| left.category.cmp(&right.category));
        self.selected_note = self.selected_note.and_then(|path| {
            let normalized = normalize_relative_note_path(&path)?;
            Some(normalized)
        });
        let mut seen_external = std::collections::HashSet::new();
        self.external_files.retain(|file| {
            let path = Path::new(&file.absolute_path);
            !file.engine_id.is_empty()
                && path.is_absolute()
                && seen_external.insert((file.engine_id.clone(), file.absolute_path.clone()))
        });
        self.selected_external = self.selected_external.filter(|selected| {
            Path::new(selected).is_absolute()
                && self
                    .external_files
                    .iter()
                    .any(|file| file.absolute_path == *selected)
        });
        self.selected_rss = self
            .selected_rss
            .filter(|selected| selected.starts_with("feeds/") && selected.len() > "feeds/".len());
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct PersistedExternalFile {
    pub(crate) engine_id: String,
    pub(crate) absolute_path: String,
}

fn valid_dimension(value: f64, minimum: f64) -> bool {
    value.is_finite() && (minimum..=MAX_WINDOW_DIMENSION).contains(&value)
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct WindowSettings {
    pub(crate) width: f64,
    pub(crate) height: f64,
}

impl Default for WindowSettings {
    fn default() -> Self {
        Self {
            width: DEFAULT_WINDOW_WIDTH,
            height: DEFAULT_WINDOW_HEIGHT,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct SidebarSettings {
    #[serde(default)]
    pub(crate) collapsed: bool,
    pub(crate) width: f64,
    pub(crate) expanded: Vec<PersistedSidebarGroup>,
    pub(crate) creation_group: PersistedSidebarGroup,
    #[serde(default)]
    pub(crate) category_order: Vec<String>,
    #[serde(default)]
    pub(crate) note_sort: Vec<CategoryNoteSortSettings>,
}

impl Default for SidebarSettings {
    fn default() -> Self {
        Self {
            collapsed: false,
            width: 256.0,
            expanded: vec![PersistedSidebarGroup::All],
            creation_group: PersistedSidebarGroup::All,
            category_order: Vec::new(),
            note_sort: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NoteSortField {
    Name,
    Created,
    Modified,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SortDirection {
    Ascending,
    Descending,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct CategoryNoteSortSettings {
    pub(crate) category: String,
    pub(crate) field: NoteSortField,
    pub(crate) direction: SortDirection,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(tag = "kind", content = "tag", rename_all = "snake_case")]
pub(crate) enum PersistedSidebarGroup {
    All,
    Favorites,
    Tag(String),
    Trash,
}

pub(crate) struct SettingsLoad {
    pub(crate) store: UiSettingsStore,
    pub(crate) settings: UiSettings,
    pub(crate) diagnostic: Option<String>,
}

pub(crate) struct UiSettingsStore {
    workspace: Option<PathBuf>,
    persisted: UiSettings,
    pending: Option<UiSettings>,
}

impl UiSettingsStore {
    pub(crate) fn unbound() -> Self {
        Self {
            workspace: None,
            persisted: UiSettings::default(),
            pending: None,
        }
    }

    pub(crate) fn load(workspace: &Path) -> SettingsLoad {
        let path = settings_path(workspace);
        let (settings, diagnostic) = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<UiSettings>(&bytes) {
                Ok(settings) if settings.version == SETTINGS_VERSION => {
                    (settings.validated(), None)
                }
                Ok(settings) => (
                    UiSettings::default(),
                    Some(format!(
                        "settings version {} is unsupported; defaults are used",
                        settings.version
                    )),
                ),
                Err(error) => (
                    UiSettings::default(),
                    Some(format!(
                        "settings could not be read: {error}; defaults are used"
                    )),
                ),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => (UiSettings::default(), None),
            Err(error) => (
                UiSettings::default(),
                Some(format!(
                    "settings could not be opened: {error}; defaults are used"
                )),
            ),
        };
        SettingsLoad {
            store: Self {
                workspace: Some(workspace.to_path_buf()),
                persisted: settings.clone(),
                pending: None,
            },
            settings,
            diagnostic,
        }
    }

    pub(crate) fn stage(&mut self, settings: UiSettings) -> bool {
        let settings = settings.validated();
        if self.pending.as_ref().unwrap_or(&self.persisted) == &settings {
            return false;
        }
        self.pending = Some(settings);
        true
    }

    pub(crate) fn flush(&mut self) -> Result<(), SettingsError> {
        let Some(settings) = self.pending.clone() else {
            return Ok(());
        };
        let workspace = self.workspace.as_deref().ok_or_else(|| {
            SettingsError::UnsafePath("workspace settings store is not bound".to_owned())
        })?;
        let _operation = stillus_platform::OperationLock::directory(workspace)?;
        let current = Self::load(workspace).settings;
        let merged = merge_settings(
            serde_json::to_value(&self.persisted).map_err(SettingsError::Serialize)?,
            serde_json::to_value(&settings).map_err(SettingsError::Serialize)?,
            serde_json::to_value(current).map_err(SettingsError::Serialize)?,
        )?;
        let merged = serde_json::from_value(merged).map_err(SettingsError::Serialize)?;
        atomic_write_settings(workspace, &merged)?;
        self.persisted = settings;
        self.pending = None;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn has_pending(&self) -> bool {
        self.pending.is_some()
    }
}

#[derive(Debug)]
pub(crate) enum SettingsError {
    Conflict,
    Io(io::Error),
    UnsafePath(String),
    Serialize(serde_json::Error),
}

impl fmt::Display for SettingsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict => write!(formatter, "settings changed since they were read"),
            Self::Io(error) => write!(formatter, "settings I/O failed: {error}"),
            Self::UnsafePath(message) => formatter.write_str(message),
            Self::Serialize(error) => write!(formatter, "settings serialization failed: {error}"),
        }
    }
}

impl From<io::Error> for SettingsError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

// Merge independent window changes, but never silently discard a competing edit.
fn merge_settings(
    base: serde_json::Value,
    wanted: serde_json::Value,
    current: serde_json::Value,
) -> Result<serde_json::Value, SettingsError> {
    if wanted == base || wanted == current {
        return Ok(current);
    }
    if current == base {
        return Ok(wanted);
    }
    if let (Some(base), Some(wanted), Some(current)) =
        (base.as_object(), wanted.as_object(), current.as_object())
    {
        let mut merged = current.clone();
        for (key, value) in wanted {
            merged.insert(
                key.clone(),
                merge_settings(
                    base.get(key).cloned().unwrap_or_default(),
                    value.clone(),
                    current.get(key).cloned().unwrap_or_default(),
                )?,
            );
        }
        return Ok(serde_json::Value::Object(merged));
    }
    Err(SettingsError::UnsafePath("settings changed in another window; close and reopen this workspace before changing its settings".to_owned()))
}

fn settings_path(workspace: &Path) -> PathBuf {
    workspace.join(SETTINGS_DIRECTORY).join(SETTINGS_FILE)
}

fn atomic_write_global_settings(
    home: &Path,
    settings: &GlobalSettings,
) -> Result<(), SettingsError> {
    let metadata = fs::symlink_metadata(home)?;
    if !metadata.is_dir() {
        return Err(SettingsError::UnsafePath(format!(
            "HOME is not a directory: {}",
            home.display()
        )));
    }
    let destination = home.join(GLOBAL_CONFIG_FILE);
    reject_symlink_or_directory(&destination)?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = home.join(format!(
        ".stillus.cfg.stillus-tmp-{}-{sequence}",
        std::process::id()
    ));
    let mut bytes = serde_json::to_vec_pretty(settings).map_err(SettingsError::Serialize)?;
    bytes.push(b'\n');
    let write_result = (|| -> Result<(), SettingsError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        stillus_platform::replace_revalidated(&temporary, &destination)?;
        sync_directory(home)?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result
}

fn atomic_write_settings(workspace: &Path, settings: &UiSettings) -> Result<(), SettingsError> {
    let directory = workspace.join(SETTINGS_DIRECTORY);
    ensure_real_directory(&directory)?;
    let destination = directory.join(SETTINGS_FILE);
    reject_symlink_or_directory(&destination)?;

    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = directory.join(format!(
        ".settings.json.stillus-tmp-{}-{sequence}",
        std::process::id()
    ));
    let mut bytes = serde_json::to_vec_pretty(settings).map_err(SettingsError::Serialize)?;
    bytes.push(b'\n');

    let write_result = (|| -> Result<(), SettingsError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        stillus_platform::replace_revalidated(&temporary, &destination)?;
        sync_directory(&directory)?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result
}

fn ensure_real_directory(path: &Path) -> Result<(), SettingsError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(SettingsError::UnsafePath(
            format!("settings directory is a symlink: {}", path.display()),
        )),
        Ok(metadata) if !metadata.is_dir() => Err(SettingsError::UnsafePath(format!(
            "settings path is not a directory: {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(path)?;
            Ok(())
        }
        Err(error) => Err(SettingsError::Io(error)),
    }
}

fn reject_symlink_or_directory(path: &Path) -> Result<(), SettingsError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(SettingsError::UnsafePath(
            format!("settings file is a symlink: {}", path.display()),
        )),
        Ok(metadata) if !metadata.is_file() => Err(SettingsError::UnsafePath(format!(
            "settings path is not a regular file: {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(SettingsError::Io(error)),
    }
}

fn sync_directory(path: &Path) -> Result<(), SettingsError> {
    stillus_platform::sync_directory(path)?;
    Ok(())
}

pub(crate) fn relative_note_path(workspace: &Path, note: &Path) -> Option<String> {
    let relative = note.strip_prefix(workspace).ok()?;
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(value) => parts.push(value.to_str()?.to_owned()),
            _ => return None,
        }
    }
    let path = parts.join("/");
    normalize_relative_note_path(&path)
}

pub(crate) fn resolve_note_path(workspace: &Path, relative: &str) -> Option<PathBuf> {
    let normalized = normalize_relative_note_path(relative)?;
    let mut path = workspace.to_path_buf();
    for component in normalized.split('/') {
        path.push(component);
    }
    Some(path)
}

fn normalize_relative_note_path(path: &str) -> Option<String> {
    let parts = path.split('/').collect::<Vec<_>>();
    if parts.len() < 2
        || parts.first().copied() != Some("notes")
        || parts.iter().any(|part| {
            part.is_empty() || *part == "." || *part == ".." || part.contains(['\\', ':'])
        })
        || !parts.last().is_some_and(|part| part.ends_with(".md"))
    {
        return None;
    }
    Some(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestWorkspace(PathBuf);

    impl TestWorkspace {
        fn new() -> Self {
            static NEXT_WORKSPACE: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "stillus-settings-test-{}-{unique}-{}",
                std::process::id(),
                NEXT_WORKSPACE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            ));
            fs::create_dir(&path).expect("create test workspace");
            Self(path)
        }
    }

    impl Drop for TestWorkspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn legacy_settings_are_ignored_and_preserved_when_stillus_saves() {
        let home = TestWorkspace::new();
        let legacy_global = br#"{"version":1,"locale":"ru"}"#;
        let legacy_local = br#"{"version":1,"future":"preserve"}"#;
        fs::write(home.0.join(".legacy.cfg"), legacy_global).unwrap();
        fs::create_dir(home.0.join(".legacy")).unwrap();
        fs::write(home.0.join(".legacy/settings.json"), legacy_local).unwrap();

        let mut global = GlobalSettingsStore::load(Some(&home.0));
        assert_eq!(global.settings.locale, Locale::English);
        assert!(!home.0.join(".stillus.cfg").exists());
        global.store.set_locale(Locale::Russian).unwrap();
        assert_eq!(
            GlobalSettingsStore::load(Some(&home.0)).settings.locale,
            Locale::Russian
        );
        let mut local = UiSettingsStore::load(&home.0);
        assert!(!home.0.join(".stillus").exists());
        local.settings.window.width = 1100.0;
        assert!(local.store.stage(local.settings));
        local.store.flush().unwrap();
        assert!(home.0.join(".stillus/settings.json").is_file());
        assert_eq!(fs::read(home.0.join(".legacy.cfg")).unwrap(), legacy_global);
        assert_eq!(
            fs::read(home.0.join(".legacy/settings.json")).unwrap(),
            legacy_local
        );
    }

    #[test]
    fn concurrent_settings_merge_independent_fields_and_reject_conflicts() {
        use serde_json::json;
        let base = json!({"window": {"width": 1200, "height": 800}, "external": []});
        let first = json!({"window": {"width": 1000, "height": 800}, "external": []});
        let second = json!({"window": {"width": 1200, "height": 900}, "external": []});
        let merged = super::merge_settings(base.clone(), first.clone(), second).unwrap();
        assert_eq!(merged["window"], json!({"width": 1000, "height": 900}));
        let collision = json!({"window": {"width": 900, "height": 800}, "external": []});
        assert!(super::merge_settings(base, first, collision).is_err());
    }

    #[test]
    fn language_defaults_and_reads_do_not_rewrite_settings() {
        let home = TestWorkspace::new();
        let path = home.0.join(".stillus.cfg");
        assert_eq!(
            GlobalSettingsStore::load(Some(&home.0)).settings.locale,
            Locale::English
        );
        assert!(!path.exists());
        for raw in [
            r#"{"version":1,"last_workspace":null}"#,
            r#"{"version":1,"last_workspace":null,"locale":"future/language"}"#,
        ] {
            fs::write(&path, raw).unwrap();
            let loaded = GlobalSettingsStore::load(Some(&home.0));
            assert_eq!(loaded.settings.locale, Locale::English);
            assert_eq!(fs::read_to_string(&path).unwrap(), raw);
        }
    }

    #[test]
    fn all_languages_persist_without_changing_workspace_or_unknown_fields() {
        let home = TestWorkspace::new();
        let path = home.0.join(".stillus.cfg");
        let remembered = home.0.join("notes");
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "version": 1, "last_workspace": remembered, "future": {"enabled": true}
            }))
            .unwrap(),
        )
        .unwrap();
        let mut store = GlobalSettingsStore::load(Some(&home.0)).store;
        for locale in Locale::ALL {
            store.set_locale(*locale).unwrap();
            let loaded = GlobalSettingsStore::load(Some(&home.0));
            assert_eq!(loaded.settings.locale, *locale);
            assert_eq!(loaded.settings.workspace(), Some(remembered.clone()));
            assert_eq!(
                loaded.settings.additional["future"],
                serde_json::json!({"enabled":true})
            );
        }
        store
            .remember_workspace(&home.0.join("another workspace"))
            .unwrap();
        assert_eq!(
            GlobalSettingsStore::load(Some(&home.0)).settings.locale,
            Locale::Korean
        );
    }

    #[test]
    fn failed_language_save_retains_previous_choice() {
        let mut unbound = GlobalSettingsStore::load(None).store;
        assert!(unbound.set_locale(Locale::Russian).is_err());
        assert_eq!(unbound.locale(), Locale::English);
        let home = TestWorkspace::new();
        let mut store = GlobalSettingsStore::load(Some(&home.0)).store;
        store.set_locale(Locale::Russian).unwrap();
        fs::remove_file(home.0.join(".stillus.cfg")).unwrap();
        fs::create_dir(home.0.join(".stillus.cfg")).unwrap();
        assert!(store.set_locale(Locale::Arabic).is_err());
        assert_eq!(store.locale(), Locale::Russian);
    }

    #[test]
    fn global_settings_remember_absolute_workspace_and_preserve_future_fields() {
        let home = TestWorkspace::new();
        let workspace = home.0.join("Notes Library");
        fs::create_dir(&workspace).expect("create remembered workspace");
        fs::write(
            home.0.join(".stillus.cfg"),
            r#"{
  "version": 1,
  "last_workspace": null,
  "future_global_setting": {"enabled": true}
}"#,
        )
        .expect("write initial global settings");

        let load = GlobalSettingsStore::load(Some(&home.0));
        assert!(load.diagnostic.is_none());
        assert_eq!(load.settings.workspace(), None);
        let mut store = load.store;
        store
            .remember_workspace(&workspace)
            .expect("remember workspace");

        let restored = GlobalSettingsStore::load(Some(&home.0));
        assert_eq!(restored.settings.workspace(), Some(workspace));
        assert_eq!(
            restored.settings.additional["future_global_setting"],
            serde_json::json!({"enabled": true})
        );
    }

    #[test]
    fn global_settings_reject_relative_and_corrupt_workspace_paths() {
        let home = TestWorkspace::new();
        let mut store = GlobalSettingsStore::load(Some(&home.0)).store;
        assert!(store.remember_workspace(Path::new("relative")).is_err());
        assert!(!home.0.join(".stillus.cfg").exists());

        fs::write(
            home.0.join(".stillus.cfg"),
            r#"{"version":1,"last_workspace":"relative"}"#,
        )
        .expect("write invalid global workspace");
        let invalid = GlobalSettingsStore::load(Some(&home.0));
        assert_eq!(invalid.settings.workspace(), None);

        fs::write(home.0.join(".stillus.cfg"), b"not json").expect("write corrupt global settings");
        let corrupt = GlobalSettingsStore::load(Some(&home.0));
        assert_eq!(corrupt.settings, GlobalSettings::default());
        assert!(corrupt.diagnostic.is_some());
    }

    #[cfg(unix)]
    #[test]
    fn global_settings_write_rejects_symlink_target() {
        use std::os::unix::fs::symlink;

        let home = TestWorkspace::new();
        let outside = home.0.with_extension("outside-config");
        fs::write(&outside, b"outside").expect("write outside target");
        symlink(&outside, home.0.join(".stillus.cfg")).expect("create config symlink");
        let workspace = home.0.join("workspace");
        fs::create_dir(&workspace).expect("create workspace");

        let mut store = GlobalSettingsStore::load(Some(&home.0)).store;
        let error = store
            .remember_workspace(&workspace)
            .expect_err("global config symlink must be rejected");
        assert!(error.to_string().contains("symlink"));
        assert_eq!(fs::read(&outside).expect("read outside target"), b"outside");
        fs::remove_file(outside).expect("remove outside target");
    }

    #[test]
    fn settings_round_trip_is_versioned_and_workspace_local() {
        let workspace = TestWorkspace::new();
        let external_path = workspace.0.join("External.md");
        let load = UiSettingsStore::load(&workspace.0);
        assert_eq!(load.settings, UiSettings::default());
        assert!(load.diagnostic.is_none());

        let mut store = load.store;
        let settings = UiSettings {
            version: SETTINGS_VERSION,
            window: WindowSettings {
                width: 1_100.0,
                height: 700.0,
            },
            sidebar: SidebarSettings {
                collapsed: true,
                width: 420.0,
                expanded: vec![
                    PersistedSidebarGroup::Tag("Work".to_owned()),
                    PersistedSidebarGroup::Tag("Work/Planning".to_owned()),
                    PersistedSidebarGroup::Favorites,
                ],
                creation_group: PersistedSidebarGroup::Tag("Work/Planning".to_owned()),
                category_order: vec![
                    "Work".to_owned(),
                    "Work/Planning".to_owned(),
                    "Personal".to_owned(),
                    "Work".to_owned(),
                ],
                note_sort: vec![
                    CategoryNoteSortSettings {
                        category: "__favorited".to_owned(),
                        field: NoteSortField::Created,
                        direction: SortDirection::Ascending,
                    },
                    CategoryNoteSortSettings {
                        category: "Work".to_owned(),
                        field: NoteSortField::Modified,
                        direction: SortDirection::Descending,
                    },
                ],
            },
            selected_note: Some("notes/Project Alpha.md".to_owned()),
            external_files: vec![PersistedExternalFile {
                engine_id: "markdown".to_owned(),
                absolute_path: external_path.display().to_string(),
            }],
            selected_external: Some(external_path.display().to_string()),
            selected_rss: Some("feeds/0123456789abcdef".to_owned()),
            selected_chat: None,
        };
        assert!(store.stage(settings.clone()));
        assert!(store.has_pending());
        store.flush().expect("flush settings");

        let restored = UiSettingsStore::load(&workspace.0);
        assert_eq!(restored.settings, settings.validated());
        let settings_path = workspace.0.join(".stillus/settings.json");
        assert!(settings_path.is_file());
        let json = fs::read_to_string(settings_path).expect("read persisted settings");
        assert!(json.contains("\"version\": 1"));
        assert!(json.contains("\"tag\": \"Work\""));
        assert!(json.contains("\"tag\": \"Work/Planning\""));
        assert_eq!(
            restored.settings.sidebar.category_order,
            ["Work", "Work/Planning", "Personal"]
        );
        assert!(json.contains("\"category_order\""));
        assert!(json.contains("\"category\": \"__favorited\""));
        assert!(json.contains("\"field\": \"modified\""));
        assert!(json.contains("\"external_files\""));
        assert_eq!(
            restored.settings.selected_external.as_deref(),
            external_path.to_str()
        );
        assert_eq!(
            restored.settings.selected_rss.as_deref(),
            Some("feeds/0123456789abcdef")
        );
    }

    #[test]
    fn invalid_values_fall_back_without_escaping_workspace() {
        let workspace = TestWorkspace::new();
        fs::create_dir(workspace.0.join(".stillus")).expect("create state directory");
        fs::write(
            workspace.0.join(".stillus/settings.json"),
            r#"{
  "version": 1,
  "window": { "width": 10.0, "height": 999999.0 },
  "sidebar": {
    "width": 999.0,
    "expanded": [{"kind":"all"}],
    "creation_group": {"kind":"all"}
  },
  "selected_note": "../outside.md"
}"#,
        )
        .expect("write invalid settings");

        let restored = UiSettingsStore::load(&workspace.0).settings;
        assert_eq!(restored.window, WindowSettings::default());
        assert_eq!(restored.sidebar.width, 480.0);
        assert!(restored.sidebar.category_order.is_empty());
        assert_eq!(restored.selected_note, None);
        assert!(restored.external_files.is_empty());
        assert_eq!(restored.selected_external, None);
        assert!(resolve_note_path(&workspace.0, "../outside.md").is_none());
    }

    #[test]
    fn corrupt_or_unknown_settings_use_defaults() {
        let workspace = TestWorkspace::new();
        fs::create_dir(workspace.0.join(".stillus")).expect("create state directory");
        let path = workspace.0.join(".stillus/settings.json");
        fs::write(&path, b"not json").expect("write corrupt settings");
        let corrupt = UiSettingsStore::load(&workspace.0);
        assert_eq!(corrupt.settings, UiSettings::default());
        assert!(corrupt.diagnostic.is_some());

        fs::write(
            &path,
            serde_json::to_vec(&UiSettings {
                version: SETTINGS_VERSION + 1,
                ..UiSettings::default()
            })
            .expect("serialize unknown version"),
        )
        .expect("write unknown settings");
        let unknown = UiSettingsStore::load(&workspace.0);
        assert_eq!(unknown.settings, UiSettings::default());
        assert!(unknown.diagnostic.is_some());
    }

    #[cfg(unix)]
    #[test]
    fn settings_write_rejects_symlink_targets() {
        use std::os::unix::fs::symlink;

        let workspace = TestWorkspace::new();
        let outside = workspace.0.with_extension("outside");
        fs::create_dir(&outside).expect("create outside directory");
        symlink(&outside, workspace.0.join(".stillus")).expect("create state symlink");

        let mut store = UiSettingsStore::load(&workspace.0).store;
        let mut settings = UiSettings::default();
        settings.sidebar.width = 300.0;
        assert!(store.stage(settings));
        let error = store.flush().expect_err("symlink must be rejected");
        assert!(error.to_string().contains("symlink"));
        assert!(!outside.join("settings.json").exists());
        fs::remove_dir_all(outside).expect("remove outside directory");
    }
}
