// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

//! Engine-neutral contracts for file-backed Stillus item types.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EngineId(String);

impl EngineId {
    pub fn new(value: impl Into<String>) -> Result<Self, EngineError> {
        let value = value.into();
        validate_slash_key(&value, false)?;
        if !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'/' || byte == b'_'
        }) {
            return Err(EngineError::InvalidIdentifier(value));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EngineId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ItemId(String);

impl ItemId {
    pub fn new(value: impl Into<String>) -> Result<Self, EngineError> {
        let value = value.into();
        validate_slash_key(&value, true)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ItemId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ItemAvailability {
    Ready,
    NeedsUnlock,
    Invalid(String),
    Unavailable(String),
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct CommonMetadata {
    pub title: String,
    pub categories: Vec<String>,
    pub pinned: bool,
    pub favorited: bool,
    pub deleted: bool,
    pub created: Option<String>,
    pub modified: Option<String>,
    pub order: BTreeMap<String, u32>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ItemSummary {
    pub engine_id: EngineId,
    pub item_id: ItemId,
    pub metadata_version: String,
    pub metadata: CommonMetadata,
    pub availability: ItemAvailability,
    pub badge: Option<u64>,
}

impl ItemSummary {
    pub fn reference(&self) -> (EngineId, ItemId) {
        (self.engine_id.clone(), self.item_id.clone())
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct CommonMetadataPatch {
    pub title: Option<String>,
    pub categories: Option<Vec<String>>,
    pub pinned: Option<bool>,
    pub favorited: Option<bool>,
    pub deleted: Option<bool>,
    pub order: Option<BTreeMap<String, u32>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternalFileSummary {
    pub engine_id: EngineId,
    pub item_id: ItemId,
    pub path: PathBuf,
    pub title: String,
    pub availability: ItemAvailability,
    pub recovery_available: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct EngineCapabilities {
    pub create: bool,
    pub settings: bool,
    pub global_search: bool,
    pub local_search: bool,
    pub scheduled_tasks: bool,
    pub manual_tasks: bool,
    pub external_files: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ItemPresentation {
    Document,
    Feed,
    Chat,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum EngineIcon {
    Document,
    Rss,
    Chat,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ToolbarAction {
    Filters,
    Refresh,
    Rename,
    Categories,
    Pin,
    Favorite,
    Delete,
    Restore,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EngineUiCapabilities {
    pub icon: EngineIcon,
    pub presentation: ItemPresentation,
    pub toolbar_actions: Vec<ToolbarAction>,
}

impl Default for EngineUiCapabilities {
    fn default() -> Self {
        Self {
            icon: EngineIcon::Document,
            presentation: ItemPresentation::Document,
            toolbar_actions: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SettingFieldType {
    Text,
    Secret,
    Url,
    Integer {
        minimum: Option<i64>,
        maximum: Option<i64>,
    },
    Boolean,
    Choice {
        values: Vec<String>,
    },
    Duration {
        minimum_seconds: u64,
        maximum_seconds: Option<u64>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SettingValue {
    Text(String),
    Url(String),
    Integer(i64),
    Boolean(bool),
    Choice(String),
    DurationSeconds(u64),
    SecretRef(SecretRef),
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretRef(String);

impl SecretRef {
    pub fn new(value: impl Into<String>) -> Result<Self, EngineError> {
        let value = value.into();
        if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(EngineError::InvalidSecretRef);
        }
        Ok(Self(value.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SettingField {
    pub key: String,
    pub label: String,
    pub description: String,
    pub required: bool,
    pub default: Option<SettingValue>,
    pub field_type: SettingFieldType,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SettingsSchema {
    pub fields: Vec<SettingField>,
}

impl SettingsSchema {
    pub fn validate(&self) -> Result<(), EngineError> {
        let mut keys = BTreeSet::new();
        for field in &self.fields {
            validate_slash_key(&field.key, false)?;
            if !keys.insert(field.key.clone()) {
                return Err(EngineError::DuplicateSetting(field.key.clone()));
            }
            if let Some(default) = &field.default {
                validate_setting_value(field, default)?;
                if matches!(field.field_type, SettingFieldType::Secret) {
                    return Err(EngineError::SecretDefault(field.key.clone()));
                }
            }
        }
        Ok(())
    }

    pub fn validate_candidate(
        &self,
        candidate: &SettingsCandidate,
        configured_secrets: &BTreeSet<String>,
    ) -> Result<(), EngineError> {
        self.validate()?;
        let known = self
            .fields
            .iter()
            .map(|field| field.key.as_str())
            .collect::<BTreeSet<_>>();
        if candidate
            .public
            .keys()
            .chain(candidate.secrets.keys())
            .any(|key| !known.contains(key.as_str()))
        {
            return Err(EngineError::InvalidSetting(
                "candidate contains an unknown key".to_owned(),
            ));
        }
        for field in &self.fields {
            if matches!(field.field_type, SettingFieldType::Secret) {
                let available = match candidate.secrets.get(&field.key) {
                    Some(SecretEdit::Replace(value)) => !value.expose().is_empty(),
                    Some(SecretEdit::Remove) => false,
                    Some(SecretEdit::Keep) | None => configured_secrets.contains(&field.key),
                };
                if field.required && !available {
                    return Err(EngineError::InvalidSetting(field.key.clone()));
                }
            } else if let Some(value) = candidate.public.get(&field.key) {
                validate_setting_value(field, value)?;
            } else if field.required && field.default.is_none() {
                return Err(EngineError::InvalidSetting(field.key.clone()));
            }
        }
        Ok(())
    }
}

pub struct SecretValue(Vec<u8>);

impl SecretValue {
    pub const MAX_BYTES: usize = 64 * 1024;

    pub fn new(value: Vec<u8>) -> Result<Self, EngineError> {
        if value.len() > Self::MAX_BYTES {
            return Err(EngineError::SecretTooLarge);
        }
        Ok(Self(value))
    }

    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for SecretValue {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// UI-facing secret buffer. Copy and cut deliberately return no payload;
/// paste/insert remain available and the storage is wiped on drop.
pub struct SecretInputBuffer(Vec<u8>);

impl SecretInputBuffer {
    pub const MAX_BYTES: usize = 1024;

    pub fn new() -> Self {
        Self(Vec::new())
    }

    pub fn insert(&mut self, bytes: &[u8]) -> Result<(), EngineError> {
        if self.0.len().saturating_add(bytes.len()) > Self::MAX_BYTES {
            return Err(EngineError::SecretTooLarge);
        }
        self.0.extend_from_slice(bytes);
        Ok(())
    }

    pub fn clear(&mut self) {
        self.0.zeroize();
    }

    pub fn copy(&self) -> Option<&[u8]> {
        None
    }

    pub fn cut(&mut self) -> Option<Vec<u8>> {
        None
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn take_secret(&mut self) -> Result<SecretValue, EngineError> {
        let bytes = std::mem::take(&mut self.0);
        SecretValue::new(bytes)
    }
}

impl Default for SecretInputBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for SecretInputBuffer {
    fn drop(&mut self) {
        self.clear();
    }
}

pub enum SecretEdit {
    Keep,
    Replace(SecretValue),
    Remove,
}

#[derive(Default)]
pub struct SettingsCandidate {
    pub public: BTreeMap<String, SettingValue>,
    pub secrets: BTreeMap<String, SecretEdit>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReferencedSecret {
    pub reference: SecretRef,
    pub engine_id: EngineId,
    pub owner: ItemId,
    pub field_key: String,
}

pub trait SecretResolver {
    fn resolve(&self, secret: &ReferencedSecret) -> Result<SecretValue, EngineError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchRequest {
    pub query: String,
    pub limit: usize,
    pub generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchHit {
    pub engine_id: EngineId,
    pub item_id: ItemId,
    pub title: String,
    pub snippet: String,
    pub score_micros: u64,
}

pub trait SearchProvider: Send + Sync {
    fn search(&self, request: &SearchRequest) -> Result<Vec<SearchHit>, EngineError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalSearchRequest {
    pub item_id: ItemId,
    pub query: String,
    pub limit: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalSearchMatch {
    pub start_byte: usize,
    pub end_byte: usize,
}

/// Bounded search primitives exposed by the active editor buffer to an engine.
/// The engine owns the decision whether and how local search is supported.
pub trait LocalSearchDocument {
    fn find_case_insensitive(&self, query: &str, limit: usize) -> Vec<LocalSearchMatch>;
}

pub trait LocalSearchProvider: Send + Sync {
    fn search_document(
        &self,
        request: &LocalSearchRequest,
        document: &dyn LocalSearchDocument,
    ) -> Result<Vec<LocalSearchMatch>, EngineError>;
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TaskId(pub String);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskTrigger {
    Manual,
    Scheduled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackgroundTaskDescriptor {
    pub id: TaskId,
    pub label: String,
    pub scheduled: bool,
    pub manual: bool,
}

pub trait FileEngineFactory: Send + Sync {
    fn id(&self) -> EngineId;
    fn display_name(&self) -> &str;
    fn capabilities(&self) -> EngineCapabilities;
    fn settings_schema(&self) -> SettingsSchema;
    fn ui_capabilities(&self) -> EngineUiCapabilities {
        EngineUiCapabilities::default()
    }
    fn external_file_extensions(&self) -> Vec<String> {
        Vec::new()
    }
    fn validate_settings(&self, candidate: &SettingsCandidate) -> Result<(), EngineError> {
        self.settings_schema()
            .validate_candidate(candidate, &BTreeSet::new())
    }
    fn open(&self, workspace: &Path) -> Result<Box<dyn FileEngine>, EngineError>;
}

pub trait FileEngine: Send {
    fn id(&self) -> EngineId;
    fn items(&self) -> Result<Vec<ItemSummary>, EngineError>;
    fn external_files(&self) -> Vec<ExternalFileSummary> {
        Vec::new()
    }
    fn open_external_file(&mut self, _path: &Path) -> Result<ExternalFileSummary, EngineError> {
        Err(EngineError::Unsupported(
            "engine does not support external files".to_owned(),
        ))
    }
    fn close_external_file(&mut self, _item: &ItemId) -> Result<bool, EngineError> {
        Ok(false)
    }
    fn create(&mut self, settings: SettingsCandidate) -> Result<ItemId, EngineError>;
    fn update_settings(
        &mut self,
        item: &ItemId,
        expected_version: &str,
        settings: SettingsCandidate,
    ) -> Result<String, EngineError>;
    fn update_metadata(
        &mut self,
        _item: &ItemId,
        _expected_version: &str,
        _patch: CommonMetadataPatch,
    ) -> Result<String, EngineError> {
        Err(EngineError::Unsupported(
            "engine does not support generic metadata patches".to_owned(),
        ))
    }
    fn referenced_secrets(&self) -> Result<Vec<ReferencedSecret>, EngineError>;
    fn search_provider(&self) -> Option<&dyn SearchProvider>;
    fn local_search_provider(&self) -> Option<&dyn LocalSearchProvider> {
        None
    }
    fn background_tasks(&self) -> Vec<BackgroundTaskDescriptor>;
    fn quiesce(&mut self) -> Result<(), EngineError>;
    fn resume(&mut self);
    fn security_rotated(&mut self);
}

pub trait EngineUi: Send + Sync {
    fn engine_id(&self) -> EngineId;
    fn capabilities(&self) -> EngineUiCapabilities;
    fn status_label(&self, item: &ItemSummary) -> Option<String>;
}

#[derive(Default)]
pub struct EngineRegistry {
    factories: BTreeMap<EngineId, Arc<dyn FileEngineFactory>>,
    external_extensions: BTreeMap<String, EngineId>,
}

impl EngineRegistry {
    pub fn register(&mut self, factory: Arc<dyn FileEngineFactory>) -> Result<(), EngineError> {
        let id = factory.id();
        factory.settings_schema().validate()?;
        let extensions = factory
            .external_file_extensions()
            .into_iter()
            .map(|extension| normalize_external_extension(&extension))
            .collect::<Result<Vec<_>, _>>()?;
        for extension in &extensions {
            if let Some(existing) = self.external_extensions.get(extension) {
                return Err(EngineError::ExternalTypeConflict {
                    extension: extension.clone(),
                    first: existing.clone(),
                    second: id.clone(),
                });
            }
        }
        if self.factories.insert(id.clone(), factory).is_some() {
            return Err(EngineError::DuplicateEngine(id));
        }
        for extension in extensions {
            self.external_extensions.insert(extension, id.clone());
        }
        Ok(())
    }

    pub fn get(&self, id: &EngineId) -> Option<Arc<dyn FileEngineFactory>> {
        self.factories.get(id).cloned()
    }

    pub fn ids(&self) -> impl Iterator<Item = &EngineId> {
        self.factories.keys()
    }

    pub fn external_engine_for_path(&self, path: &Path) -> Option<EngineId> {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        self.external_extensions.get(&extension).cloned()
    }

    pub fn external_file_extensions(&self) -> impl Iterator<Item = &str> {
        self.external_extensions.keys().map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.factories.len()
    }

    pub fn is_empty(&self) -> bool {
        self.factories.is_empty()
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TaskKey {
    pub engine_id: EngineId,
    pub item_id: Option<ItemId>,
    pub task_id: TaskId,
}

#[derive(Clone)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GenerationToken(pub u64);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScheduledTaskState {
    Running {
        generation: GenerationToken,
        trigger: TaskTrigger,
        completed: u64,
        total: Option<u64>,
    },
    Backoff {
        failures: u32,
        retry_at_ms: u64,
        message: String,
    },
}

pub struct TaskScheduler {
    next_generation: u64,
    max_parallel: usize,
    active: BTreeMap<TaskKey, (ScheduledTaskState, CancellationToken)>,
}

impl Default for TaskScheduler {
    fn default() -> Self {
        Self::with_max_parallel(4)
    }
}

impl TaskScheduler {
    pub fn with_max_parallel(max_parallel: usize) -> Self {
        Self {
            next_generation: 0,
            max_parallel: max_parallel.max(1),
            active: BTreeMap::new(),
        }
    }

    pub fn begin(
        &mut self,
        key: TaskKey,
    ) -> Result<(GenerationToken, CancellationToken), EngineError> {
        self.begin_triggered(key, TaskTrigger::Manual)
    }

    pub fn begin_triggered(
        &mut self,
        key: TaskKey,
        trigger: TaskTrigger,
    ) -> Result<(GenerationToken, CancellationToken), EngineError> {
        if self.active.contains_key(&key) {
            return Err(EngineError::TaskAlreadyRunning);
        }
        let running = self
            .active
            .values()
            .filter(|(state, _)| matches!(state, ScheduledTaskState::Running { .. }))
            .count();
        if running >= self.max_parallel {
            return Err(EngineError::TaskCapacity);
        }
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let generation = GenerationToken(self.next_generation);
        let cancellation = CancellationToken(Arc::new(AtomicBool::new(false)));
        self.active.insert(
            key,
            (
                ScheduledTaskState::Running {
                    generation,
                    trigger,
                    completed: 0,
                    total: None,
                },
                cancellation.clone(),
            ),
        );
        Ok((generation, cancellation))
    }

    pub fn retry_ready(
        &mut self,
        key: &TaskKey,
        now_ms: u64,
    ) -> Result<(GenerationToken, CancellationToken), EngineError> {
        let retry_at_ms = match self.active.get(key).map(|(state, _)| state) {
            Some(ScheduledTaskState::Backoff { retry_at_ms, .. }) => *retry_at_ms,
            Some(ScheduledTaskState::Running { .. }) => {
                return Err(EngineError::TaskAlreadyRunning);
            }
            None => return Err(EngineError::TaskMissing),
        };
        if now_ms < retry_at_ms {
            return Err(EngineError::TaskBackoff);
        }
        let running = self
            .active
            .values()
            .filter(|(state, _)| matches!(state, ScheduledTaskState::Running { .. }))
            .count();
        if running >= self.max_parallel {
            return Err(EngineError::TaskCapacity);
        }
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let generation = GenerationToken(self.next_generation);
        let cancellation = CancellationToken(Arc::new(AtomicBool::new(false)));
        self.active.insert(
            key.clone(),
            (
                ScheduledTaskState::Running {
                    generation,
                    trigger: TaskTrigger::Scheduled,
                    completed: 0,
                    total: None,
                },
                cancellation.clone(),
            ),
        );
        Ok((generation, cancellation))
    }

    pub fn progress(
        &mut self,
        key: &TaskKey,
        generation: GenerationToken,
        completed: u64,
        total: Option<u64>,
    ) -> bool {
        let Some((
            ScheduledTaskState::Running {
                generation: current,
                completed: prior,
                total: current_total,
                ..
            },
            _,
        )) = self.active.get_mut(key)
        else {
            return false;
        };
        if *current != generation
            || completed < *prior
            || total.is_some_and(|value| completed > value)
        {
            return false;
        }
        *prior = completed;
        *current_total = total;
        true
    }

    pub fn complete(&mut self, key: &TaskKey, generation: GenerationToken) -> bool {
        let matches = self.active.get(key).is_some_and(|(state, _)| {
            matches!(state, ScheduledTaskState::Running { generation: current, .. } if *current == generation)
        });
        if matches {
            self.active.remove(key);
        }
        matches
    }

    pub fn fail(
        &mut self,
        key: &TaskKey,
        generation: GenerationToken,
        failures: u32,
        now_ms: u64,
        message: String,
    ) -> bool {
        let Some((state, _)) = self.active.get_mut(key) else {
            return false;
        };
        if !matches!(state, ScheduledTaskState::Running { generation: current, .. } if *current == generation)
        {
            return false;
        }
        let shift = failures.min(10);
        let delay = 1_000_u64.saturating_mul(1_u64 << shift);
        *state = ScheduledTaskState::Backoff {
            failures,
            retry_at_ms: now_ms.saturating_add(delay.min(3_600_000)),
            message,
        };
        true
    }

    pub fn cancel(&mut self, key: &TaskKey) -> bool {
        let Some((_, token)) = self.active.remove(key) else {
            return false;
        };
        token.cancel();
        true
    }

    pub fn cancel_all(&mut self) {
        for (_, token) in self.active.values() {
            token.cancel();
        }
        self.active.clear();
    }

    pub fn state(&self, key: &TaskKey) -> Option<&ScheduledTaskState> {
        self.active.get(key).map(|(state, _)| state)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EngineError {
    InvalidIdentifier(String),
    InvalidSecretRef,
    DuplicateSetting(String),
    SecretDefault(String),
    SecretTooLarge,
    InvalidSetting(String),
    DuplicateEngine(EngineId),
    ExternalTypeConflict {
        extension: String,
        first: EngineId,
        second: EngineId,
    },
    TaskAlreadyRunning,
    TaskCapacity,
    TaskBackoff,
    TaskMissing,
    NeedsUnlock,
    Conflict,
    Io(String),
    Unsupported(String),
}

impl fmt::Display for EngineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIdentifier(value) => {
                write!(formatter, "invalid engine identifier: {value}")
            }
            Self::InvalidSecretRef => formatter.write_str("invalid secret reference"),
            Self::DuplicateSetting(key) => write!(formatter, "duplicate setting key: {key}"),
            Self::SecretDefault(key) => {
                write!(formatter, "secret setting cannot have a default: {key}")
            }
            Self::SecretTooLarge => formatter.write_str("secret value is too large"),
            Self::InvalidSetting(key) => write!(formatter, "invalid setting value: {key}"),
            Self::DuplicateEngine(id) => write!(formatter, "engine is already registered: {id}"),
            Self::ExternalTypeConflict {
                extension,
                first,
                second,
            } => write!(
                formatter,
                "external extension {extension} is claimed by both {first} and {second}"
            ),
            Self::TaskAlreadyRunning => formatter.write_str("task is already running"),
            Self::TaskCapacity => formatter.write_str("task scheduler capacity is exhausted"),
            Self::TaskBackoff => formatter.write_str("task is waiting for its retry deadline"),
            Self::TaskMissing => formatter.write_str("task is not scheduled"),
            Self::NeedsUnlock => formatter.write_str("workspace security must be unlocked"),
            Self::Conflict => formatter.write_str("engine configuration changed externally"),
            Self::Io(message) => write!(formatter, "engine I/O error: {message}"),
            Self::Unsupported(message) => {
                write!(formatter, "engine operation is unsupported: {message}")
            }
        }
    }
}

fn normalize_external_extension(extension: &str) -> Result<String, EngineError> {
    let extension = extension.trim_start_matches('.').to_ascii_lowercase();
    if extension.is_empty()
        || !extension
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        return Err(EngineError::InvalidIdentifier(extension));
    }
    Ok(extension)
}

impl std::error::Error for EngineError {}

fn validate_slash_key(value: &str, allow_extension: bool) -> Result<(), EngineError> {
    if value.is_empty()
        || value.starts_with('/')
        || value.ends_with('/')
        || value.split('/').any(|part| {
            part.is_empty()
                || part == "."
                || part == ".."
                || part.chars().any(|character| {
                    character.is_control()
                        || character == '\\'
                        || (!allow_extension && character == '.')
                })
        })
    {
        return Err(EngineError::InvalidIdentifier(value.to_owned()));
    }
    Ok(())
}

fn validate_setting_value(field: &SettingField, value: &SettingValue) -> Result<(), EngineError> {
    let valid = match (&field.field_type, value) {
        (SettingFieldType::Text, SettingValue::Text(_)) => true,
        (SettingFieldType::Url, SettingValue::Url(value)) => {
            value.starts_with("https://") || value.starts_with("http://")
        }
        (SettingFieldType::Integer { minimum, maximum }, SettingValue::Integer(value)) => {
            minimum.is_none_or(|minimum| value >= &minimum)
                && maximum.is_none_or(|maximum| value <= &maximum)
        }
        (SettingFieldType::Boolean, SettingValue::Boolean(_)) => true,
        (SettingFieldType::Choice { values }, SettingValue::Choice(value)) => {
            values.contains(value)
        }
        (
            SettingFieldType::Duration {
                minimum_seconds,
                maximum_seconds,
            },
            SettingValue::DurationSeconds(value),
        ) => value >= minimum_seconds && maximum_seconds.is_none_or(|maximum| value <= &maximum),
        (SettingFieldType::Secret, SettingValue::SecretRef(_)) => true,
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(EngineError::InvalidSetting(field.key.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestFactory;

    struct ExternalFactory {
        id: &'static str,
        extensions: &'static [&'static str],
    }

    impl FileEngineFactory for ExternalFactory {
        fn id(&self) -> EngineId {
            EngineId::new(self.id).unwrap()
        }

        fn display_name(&self) -> &str {
            self.id
        }

        fn capabilities(&self) -> EngineCapabilities {
            EngineCapabilities {
                external_files: true,
                ..EngineCapabilities::default()
            }
        }

        fn external_file_extensions(&self) -> Vec<String> {
            self.extensions
                .iter()
                .map(|value| (*value).to_owned())
                .collect()
        }

        fn settings_schema(&self) -> SettingsSchema {
            SettingsSchema::default()
        }

        fn open(&self, _workspace: &Path) -> Result<Box<dyn FileEngine>, EngineError> {
            Err(EngineError::Unsupported("test factory".to_owned()))
        }
    }

    struct TestSearch;

    impl SearchProvider for TestSearch {
        fn search(&self, request: &SearchRequest) -> Result<Vec<SearchHit>, EngineError> {
            Ok(vec![SearchHit {
                engine_id: EngineId::new("test").unwrap(),
                item_id: ItemId::new("items/one").unwrap(),
                title: "Remote item".to_owned(),
                snippet: request.query.clone(),
                score_micros: 1_000_000,
            }])
        }
    }

    struct TestLocalSearch;

    impl LocalSearchProvider for TestLocalSearch {
        fn search_document(
            &self,
            request: &LocalSearchRequest,
            document: &dyn LocalSearchDocument,
        ) -> Result<Vec<LocalSearchMatch>, EngineError> {
            Ok(document.find_case_insensitive(&request.query, request.limit))
        }
    }

    struct TestDocument;

    impl LocalSearchDocument for TestDocument {
        fn find_case_insensitive(&self, query: &str, limit: usize) -> Vec<LocalSearchMatch> {
            (query == "needle" && limit > 0)
                .then_some(LocalSearchMatch {
                    start_byte: 4,
                    end_byte: 10,
                })
                .into_iter()
                .collect()
        }
    }

    struct TestEngine {
        search: TestSearch,
        local_search: TestLocalSearch,
    }

    impl FileEngine for TestEngine {
        fn id(&self) -> EngineId {
            EngineId::new("test").unwrap()
        }

        fn items(&self) -> Result<Vec<ItemSummary>, EngineError> {
            Ok(vec![ItemSummary {
                engine_id: self.id(),
                item_id: ItemId::new("items/one").unwrap(),
                metadata_version: "1".to_owned(),
                metadata: CommonMetadata {
                    title: "Remote item".to_owned(),
                    categories: vec!["Inbox".to_owned()],
                    favorited: true,
                    ..CommonMetadata::default()
                },
                availability: ItemAvailability::Ready,
                badge: Some(3),
            }])
        }

        fn create(&mut self, _settings: SettingsCandidate) -> Result<ItemId, EngineError> {
            Ok(ItemId::new("items/new").unwrap())
        }

        fn update_settings(
            &mut self,
            _item: &ItemId,
            expected_version: &str,
            _settings: SettingsCandidate,
        ) -> Result<String, EngineError> {
            if expected_version != "v1" {
                return Err(EngineError::Conflict);
            }
            Ok("v2".to_owned())
        }

        fn referenced_secrets(&self) -> Result<Vec<ReferencedSecret>, EngineError> {
            Ok(vec![ReferencedSecret {
                reference: SecretRef::new("0123456789abcdef0123456789abcdef").unwrap(),
                engine_id: self.id(),
                owner: ItemId::new("items/one").unwrap(),
                field_key: "connection/token".to_owned(),
            }])
        }

        fn search_provider(&self) -> Option<&dyn SearchProvider> {
            Some(&self.search)
        }

        fn local_search_provider(&self) -> Option<&dyn LocalSearchProvider> {
            Some(&self.local_search)
        }

        fn background_tasks(&self) -> Vec<BackgroundTaskDescriptor> {
            vec![BackgroundTaskDescriptor {
                id: TaskId("sync".to_owned()),
                label: "Sync".to_owned(),
                scheduled: true,
                manual: true,
            }]
        }

        fn quiesce(&mut self) -> Result<(), EngineError> {
            Ok(())
        }

        fn resume(&mut self) {}

        fn security_rotated(&mut self) {}
    }

    impl FileEngineFactory for TestFactory {
        fn id(&self) -> EngineId {
            EngineId::new("test").unwrap()
        }

        fn display_name(&self) -> &str {
            "Test"
        }

        fn capabilities(&self) -> EngineCapabilities {
            EngineCapabilities {
                settings: true,
                local_search: true,
                manual_tasks: true,
                ..EngineCapabilities::default()
            }
        }

        fn settings_schema(&self) -> SettingsSchema {
            SettingsSchema {
                fields: vec![SettingField {
                    key: "connection/token".to_owned(),
                    label: "Token".to_owned(),
                    description: "Test token".to_owned(),
                    required: true,
                    default: None,
                    field_type: SettingFieldType::Secret,
                }],
            }
        }

        fn open(&self, _workspace: &Path) -> Result<Box<dyn FileEngine>, EngineError> {
            Ok(Box::new(TestEngine {
                search: TestSearch,
                local_search: TestLocalSearch,
            }))
        }
    }

    #[test]
    fn registry_validates_schema_and_rejects_duplicate_engine() {
        let mut registry = EngineRegistry::default();
        registry.register(Arc::new(TestFactory)).unwrap();
        assert_eq!(registry.ids().next().unwrap().as_str(), "test");
        assert!(matches!(
            registry.register(Arc::new(TestFactory)),
            Err(EngineError::DuplicateEngine(_))
        ));
    }

    #[test]
    fn registry_normalizes_external_extensions_and_rejects_ambiguity() {
        let mut registry = EngineRegistry::default();
        registry
            .register(Arc::new(ExternalFactory {
                id: "first",
                extensions: &[".MD", "txt"],
            }))
            .unwrap();
        assert_eq!(
            registry
                .external_engine_for_path(Path::new("Example.mD"))
                .unwrap()
                .as_str(),
            "first"
        );
        assert_eq!(
            registry.external_file_extensions().collect::<Vec<_>>(),
            ["md", "txt"]
        );
        assert!(matches!(
            registry.register(Arc::new(ExternalFactory {
                id: "second",
                extensions: &["md"],
            })),
            Err(EngineError::ExternalTypeConflict { extension, .. }) if extension == "md"
        ));
    }

    #[test]
    fn test_engine_exercises_generic_catalog_search_secret_and_task_contracts() {
        let engine = TestFactory.open(Path::new("unused")).unwrap();
        let items = engine.items().unwrap();
        assert_eq!(items[0].metadata.title, "Remote item");
        assert_eq!(items[0].metadata.categories, ["Inbox"]);
        assert!(items[0].metadata.favorited);
        assert_eq!(items[0].badge, Some(3));

        let secret = engine.referenced_secrets().unwrap().pop().unwrap();
        assert_eq!(secret.engine_id.as_str(), "test");
        assert_eq!(secret.owner.as_str(), "items/one");
        assert_eq!(secret.field_key, "connection/token");

        let hits = engine
            .search_provider()
            .unwrap()
            .search(&SearchRequest {
                query: "needle".to_owned(),
                limit: 10,
                generation: 7,
            })
            .unwrap();
        assert_eq!(hits[0].snippet, "needle");
        let local_matches = engine
            .local_search_provider()
            .unwrap()
            .search_document(
                &LocalSearchRequest {
                    item_id: ItemId::new("items/one").unwrap(),
                    query: "needle".to_owned(),
                    limit: 10,
                },
                &TestDocument,
            )
            .unwrap();
        assert_eq!(
            local_matches,
            [LocalSearchMatch {
                start_byte: 4,
                end_byte: 10,
            }]
        );
        assert_eq!(engine.background_tasks()[0].id.0, "sync");
    }

    #[test]
    fn scheduler_rejects_parallel_and_stale_results() {
        let key = TaskKey {
            engine_id: EngineId::new("test").unwrap(),
            item_id: Some(ItemId::new("items/one").unwrap()),
            task_id: TaskId("sync".to_owned()),
        };
        let mut scheduler = TaskScheduler::default();
        let (generation, cancellation) = scheduler.begin(key.clone()).unwrap();
        assert!(matches!(
            scheduler.begin(key.clone()),
            Err(EngineError::TaskAlreadyRunning)
        ));
        assert!(scheduler.progress(&key, generation, 2, Some(4)));
        assert!(!scheduler.progress(&key, GenerationToken(generation.0 + 1), 3, Some(4)));
        assert!(!scheduler.progress(&key, generation, 1, Some(4)));
        assert!(scheduler.cancel(&key));
        assert!(cancellation.is_cancelled());
    }

    #[test]
    fn scheduler_enforces_global_parallelism_limit() {
        let mut scheduler = TaskScheduler::with_max_parallel(1);
        let first = TaskKey {
            engine_id: EngineId::new("test").unwrap(),
            item_id: None,
            task_id: TaskId("first".to_owned()),
        };
        let second = TaskKey {
            engine_id: EngineId::new("test").unwrap(),
            item_id: None,
            task_id: TaskId("second".to_owned()),
        };
        let (generation, _) = scheduler.begin(first.clone()).unwrap();
        assert!(matches!(
            scheduler.begin(second.clone()),
            Err(EngineError::TaskCapacity)
        ));
        assert!(scheduler.complete(&first, generation));
        assert!(scheduler.begin(second).is_ok());
    }

    #[test]
    fn scheduler_retries_backoff_with_a_new_generation() {
        let key = TaskKey {
            engine_id: EngineId::new("test").unwrap(),
            item_id: None,
            task_id: TaskId("scheduled".to_owned()),
        };
        let mut scheduler = TaskScheduler::default();
        let (first, _) = scheduler
            .begin_triggered(key.clone(), TaskTrigger::Scheduled)
            .unwrap();
        assert!(scheduler.fail(&key, first, 1, 10_000, "temporary".to_owned()));
        assert!(matches!(
            scheduler.retry_ready(&key, 11_999),
            Err(EngineError::TaskBackoff)
        ));
        let (second, _) = scheduler.retry_ready(&key, 12_000).unwrap();
        assert!(second.0 > first.0);
        assert!(matches!(
            scheduler.state(&key),
            Some(ScheduledTaskState::Running {
                trigger: TaskTrigger::Scheduled,
                ..
            })
        ));
    }

    #[test]
    fn secret_values_zeroize_and_secret_defaults_are_rejected() {
        let schema = SettingsSchema {
            fields: vec![SettingField {
                key: "token".to_owned(),
                label: "Token".to_owned(),
                description: String::new(),
                required: false,
                default: Some(SettingValue::SecretRef(
                    SecretRef::new("0123456789abcdef0123456789abcdef").unwrap(),
                )),
                field_type: SettingFieldType::Secret,
            }],
        };
        assert!(matches!(
            schema.validate(),
            Err(EngineError::SecretDefault(_))
        ));
        let value = SecretValue::new(b"secret".to_vec()).unwrap();
        assert_eq!(value.expose(), b"secret");
    }

    #[test]
    fn secret_form_keeps_existing_values_and_swallows_clipboard_actions() {
        let schema = TestFactory.settings_schema();
        let mut configured = BTreeSet::new();
        configured.insert("connection/token".to_owned());
        let mut candidate = SettingsCandidate::default();
        candidate
            .secrets
            .insert("connection/token".to_owned(), SecretEdit::Keep);
        schema.validate_candidate(&candidate, &configured).unwrap();

        candidate
            .secrets
            .insert("connection/token".to_owned(), SecretEdit::Remove);
        assert!(matches!(
            schema.validate_candidate(&candidate, &configured),
            Err(EngineError::InvalidSetting(key)) if key == "connection/token"
        ));

        let mut input = SecretInputBuffer::new();
        input.insert(b"pasted secret").unwrap();
        assert_eq!(input.copy(), None);
        assert_eq!(input.cut(), None);
        assert!(!input.is_empty());
        assert_eq!(input.take_secret().unwrap().expose(), b"pasted secret");
        assert!(input.is_empty());
        assert!(
            input
                .insert(&vec![b'x'; SecretInputBuffer::MAX_BYTES + 1])
                .is_err()
        );
    }
}
