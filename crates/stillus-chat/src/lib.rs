// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]

//! File-backed conversations. Callers perform I/O on workers and never retain a
//! transaction lock across a provider request. Reading never repairs or migrates files.
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
use stillus_engine::*;

pub const MAX_RECORD_BYTES: usize = 1024 * 1024;
pub const MAX_PAGE: usize = 64;
pub const MAX_PAGE_BYTES: usize = 2 * 1024 * 1024;
const FORMAT: u32 = 1;
static NEXT: AtomicU64 = AtomicU64::new(1);
pub fn engine_id() -> EngineId {
    EngineId::new("ai/chat").expect("static engine id")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChatError {
    Conflict,
    Missing,
    Unsupported(u32),
    Corrupt,
    TooLarge,
    Io,
}
impl From<std::io::Error> for ChatError {
    fn from(e: std::io::Error) -> Self {
        if e.kind() == std::io::ErrorKind::NotFound {
            Self::Missing
        } else {
            Self::Io
        }
    }
}
impl From<ChatError> for EngineError {
    fn from(e: ChatError) -> Self {
        match e {
            ChatError::Conflict => Self::Conflict,
            ChatError::Unsupported(_) => Self::Unsupported("chat format".into()),
            _ => Self::Io(format!("chat: {e:?}")),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Versioned<T> {
    pub revision: String,
    pub value: T,
}
#[derive(Serialize, Deserialize)]
struct Envelope<T> {
    version: u32,
    data: T,
    #[serde(flatten)]
    additional: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Metadata {
    pub common: CommonMetadata,
    pub automatic_title: bool,
    pub alias: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Draft {
    pub text: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    Tool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Partial,
    Complete,
    Interrupted,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub run: String,
    pub role: Role,
    pub text: String,
    pub delivery: Delivery,
    pub created_ms: u64,
    #[serde(default)]
    pub tool: Option<ToolCall>,
    #[serde(default)]
    pub provider_state: Option<Value>,
    #[serde(default)]
    pub request_id: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolState {
    Prepared,
    Completed,
    Failed,
    Unknown,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
    pub state: ToolState,
    pub result: Option<Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Running,
    Paused,
    Completed,
    Stopped,
    Interrupted,
    Failed,
}
impl RunStatus {
    pub fn active(&self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub status: RunStatus,
    pub provider: String,
    pub model: String,
    pub parameters: Value,
    pub requests: u32,
    pub tools: u32,
    #[serde(default = "default_request_limit")]
    pub request_limit: u32,
    #[serde(default = "default_tool_limit")]
    pub tool_limit: u32,
    pub error: Option<String>,
    #[serde(default)]
    pub unread: bool,
    #[serde(default)]
    pub pending_calls: Vec<ToolCall>,
    #[serde(default)]
    pub pending_request: Option<String>,
    #[serde(default = "default_awaiting_model")]
    pub awaiting_model: bool,
}
fn default_request_limit() -> u32 {
    20
}
fn default_tool_limit() -> u32 {
    50
}
fn default_awaiting_model() -> bool {
    true
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ContextSummary {
    pub through: Option<String>,
    pub text: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatSnapshot {
    pub item: ItemSummary,
    pub metadata: Option<Versioned<Metadata>>,
    pub run: Option<Versioned<Run>>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: String,
    pub message: Option<Versioned<Message>>,
    pub diagnostic: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryPage {
    pub entries: Vec<HistoryEntry>,
    pub next: Option<String>,
}

#[derive(Clone)]
pub struct ChatStore {
    root: PathBuf,
    workspace: PathBuf,
}
impl ChatStore {
    pub fn open(workspace: &Path) -> Result<Self, ChatError> {
        let root = workspace.join(".stillus/engines/ai/chat");
        // Validate existing ancestors without creating the engine on workspace open.
        validate_path(&root)?;
        Ok(Self {
            root,
            workspace: workspace.to_owned(),
        })
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    fn directory(&self, id: &ItemId) -> Result<PathBuf, ChatError> {
        valid_id(id.as_str())?;
        Ok(self.root.join(id.as_str()))
    }
    pub fn list(&self) -> Result<Vec<ChatSnapshot>, ChatError> {
        validate_path(&self.root)?;
        let entries = match fs::read_dir(&self.root) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut result = Vec::new();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if valid_id(&name).is_err() {
                continue;
            }
            let id = ItemId::new(name).map_err(|_| ChatError::Corrupt)?;
            let metadata = self.metadata(&id);
            let (common, revision, availability) = match &metadata {
                Ok(m) => (
                    m.value.common.clone(),
                    m.revision.clone(),
                    ItemAvailability::Ready,
                ),
                Err(e) => (
                    CommonMetadata {
                        title: id.to_string(),
                        ..Default::default()
                    },
                    String::new(),
                    ItemAvailability::Unavailable(format!("{e:?}")),
                ),
            };
            let run = self.run(&id).ok().flatten();
            result.push(ChatSnapshot {
                item: ItemSummary {
                    engine_id: engine_id(),
                    item_id: id,
                    metadata_version: revision,
                    metadata: common,
                    availability,
                    badge: run.as_ref().filter(|r| r.value.unread).map(|_| 1),
                },
                metadata: metadata.ok(),
                run,
            });
        }
        result.sort_by(|a, b| a.item.item_id.cmp(&b.item.item_id));
        Ok(result)
    }
    pub fn create_chat(&self, metadata: Metadata) -> Result<ItemId, ChatError> {
        let _workspace_lock = stillus_platform::OperationLock::directory(&self.workspace)?;
        ensure_directory(&self.root)?;
        let _lock = stillus_platform::OperationLock::directory(&self.root)?;
        for _ in 0..32 {
            let id = ItemId::new(new_id()).map_err(|_| ChatError::Corrupt)?;
            let directory = self.directory(&id)?;
            match fs::create_dir(&directory) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
            write_record(&directory.join("metadata.json"), None, &metadata)?;
            write_record(&directory.join("draft.json"), None, &Draft::default())?;
            stillus_platform::sync_directory(&self.root)?;
            return Ok(id);
        }
        Err(ChatError::Io)
    }
    pub fn metadata(&self, id: &ItemId) -> Result<Versioned<Metadata>, ChatError> {
        read_record(&self.directory(id)?.join("metadata.json"))
    }
    pub fn patch_metadata(
        &self,
        id: &ItemId,
        expected: &str,
        patch: CommonMetadataPatch,
        alias: Option<String>,
        _manual: bool,
    ) -> Result<Versioned<Metadata>, ChatError> {
        let _workspace_lock = stillus_platform::OperationLock::directory(&self.workspace)?;
        let path = self.directory(id)?.join("metadata.json");
        let _lock =
            stillus_platform::OperationLock::directory(path.parent().ok_or(ChatError::Io)?)?;
        let mut metadata: Versioned<Metadata> = read_record(&path)?;
        if metadata.revision != expected {
            return Err(ChatError::Conflict);
        }
        if let Some(title) = patch.title {
            if title.trim().is_empty() || title.len() > 4096 {
                return Err(ChatError::TooLarge);
            }
            metadata.value.common.title = title;
            metadata.value.automatic_title = false;
        }
        if let Some(categories) = patch.categories {
            metadata.value.common.categories = categories;
        }
        if let Some(pinned) = patch.pinned {
            metadata.value.common.pinned = pinned;
        }
        if let Some(favorited) = patch.favorited {
            metadata.value.common.favorited = favorited;
        }
        if let Some(deleted) = patch.deleted {
            metadata.value.common.deleted = deleted;
        }
        if let Some(order) = patch.order {
            metadata.value.common.order = order;
        }
        if let Some(alias) = alias {
            if alias.is_empty() || alias.len() > 1024 {
                return Err(ChatError::TooLarge);
            }
            metadata.value.alias = alias;
        }
        write_record(&path, Some(expected), &metadata.value)
    }
    pub fn draft(&self, id: &ItemId) -> Result<Versioned<Draft>, ChatError> {
        read_record(&self.directory(id)?.join("draft.json"))
    }
    pub fn save_draft(
        &self,
        id: &ItemId,
        expected: &str,
        draft: &Draft,
    ) -> Result<Versioned<Draft>, ChatError> {
        self.save(id, "draft.json", Some(expected), draft)
    }
    pub fn run(&self, id: &ItemId) -> Result<Option<Versioned<Run>>, ChatError> {
        let mut run: Option<Versioned<Run>> =
            optional_record(&self.directory(id)?.join("run.json"))?;
        // A read-only restart projection: persistence is an explicit coordinator operation.
        if let Some(run) = &mut run {
            if run.value.status.active() {
                run.value.status = RunStatus::Interrupted;
            }
        }
        Ok(run)
    }
    pub fn save_run(
        &self,
        id: &ItemId,
        expected: Option<&str>,
        run: &Run,
    ) -> Result<Versioned<Run>, ChatError> {
        self.save(id, "run.json", expected, run)
    }
    pub fn summary(&self, id: &ItemId) -> Result<Option<Versioned<ContextSummary>>, ChatError> {
        optional_record(&self.directory(id)?.join("summary.json"))
    }
    pub fn save_summary(
        &self,
        id: &ItemId,
        expected: Option<&str>,
        summary: &ContextSummary,
    ) -> Result<Versioned<ContextSummary>, ChatError> {
        self.save(id, "summary.json", expected, summary)
    }
    fn save<T: Serialize + DeserializeOwned>(
        &self,
        id: &ItemId,
        name: &str,
        expected: Option<&str>,
        value: &T,
    ) -> Result<Versioned<T>, ChatError> {
        let directory = self.directory(id)?;
        let _lock = stillus_platform::OperationLock::directory(&directory)?;
        write_record(&directory.join(name), expected, value)
    }
    pub fn save_message(
        &self,
        id: &ItemId,
        expected: Option<&str>,
        message: &Message,
    ) -> Result<Versioned<Message>, ChatError> {
        valid_id(&message.id)?;
        let directory = self.directory(id)?;
        let _lock = stillus_platform::OperationLock::directory(&directory)?;
        let messages = directory.join("messages");
        ensure_directory(&messages)?;
        write_record(
            &messages.join(format!("{}.json", message.id)),
            expected,
            message,
        )
    }
    /// The cursor is an opaque message id. A page reads at most 64 bounded records.
    pub fn history(
        &self,
        id: &ItemId,
        before: Option<&str>,
        limit: usize,
    ) -> Result<HistoryPage, ChatError> {
        if let Some(cursor) = before {
            valid_id(cursor)?;
        }
        let directory = self.directory(id)?.join("messages");
        validate_path(&directory)?;
        let entries = match fs::read_dir(&directory) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(HistoryPage {
                    entries: Vec::new(),
                    next: None,
                });
            }
            Err(e) => return Err(e.into()),
        };
        let limit = limit.clamp(1, MAX_PAGE);
        let mut paths = BTreeMap::new();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(id) = name.strip_suffix(".json") {
                if valid_id(id).is_ok() && before.is_none_or(|b| id < b) {
                    paths.insert(id.to_string(), entry.path());
                    if paths.len() > limit + 1 {
                        paths.pop_first();
                    }
                }
            }
        }
        let more = paths.len() > limit;
        if more {
            paths.pop_first();
        }
        let next = if more {
            paths.first_key_value().map(|(id, _)| id.clone())
        } else {
            None
        };
        let mut next = next;
        let mut page_bytes = 0;
        let mut entries = Vec::new();
        for (id, path) in paths.into_iter().rev() {
            let size = fs::symlink_metadata(&path)
                .map(|m| m.len().min(MAX_RECORD_BYTES as u64) as usize)
                .unwrap_or(0);
            if !entries.is_empty() && page_bytes + size > MAX_PAGE_BYTES {
                next = entries.last().map(|entry: &HistoryEntry| entry.id.clone());
                break;
            }
            page_bytes += size;
            entries.push(match read_record::<Message>(&path) {
                Ok(mut m) => {
                    if let Some(tool) = &mut m.value.tool {
                        if tool.state == ToolState::Prepared {
                            tool.state = ToolState::Unknown;
                        }
                    }
                    HistoryEntry {
                        id,
                        message: Some(m),
                        diagnostic: None,
                    }
                }
                Err(e) => HistoryEntry {
                    id,
                    message: None,
                    diagnostic: Some(format!("{e:?}")),
                },
            });
        }
        entries.reverse();
        Ok(HistoryPage { entries, next })
    }
}

pub fn new_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{:024x}{:08x}",
        nanos,
        NEXT.fetch_add(1, Ordering::Relaxed) as u32
    )
}
fn valid_id(id: &str) -> Result<(), ChatError> {
    if id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        Ok(())
    } else {
        Err(ChatError::Corrupt)
    }
}
fn validate_path(path: &Path) -> Result<(), ChatError> {
    let mut current = PathBuf::new();
    for c in path.components() {
        current.push(c);
        match fs::symlink_metadata(&current) {
            Ok(m) if stillus_platform::is_link(&m) => return Err(ChatError::Io),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
fn ensure_directory(path: &Path) -> Result<(), ChatError> {
    validate_path(path)?;
    fs::create_dir_all(path)?;
    Ok(())
}
fn bytes(path: &Path) -> Result<Vec<u8>, ChatError> {
    validate_path(path)?;
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(ChatError::Corrupt);
    }
    let file = stillus_platform::fs::File::open(path)?;
    let info = stillus_platform::file_information(&file)?;
    if info.links != 1 {
        return Err(ChatError::Io);
    }
    if file.metadata()?.len() > MAX_RECORD_BYTES as u64 {
        return Err(ChatError::TooLarge);
    }
    let mut bytes = Vec::new();
    file.take(MAX_RECORD_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(ChatError::TooLarge);
    }
    Ok(bytes)
}
fn read_record<T: DeserializeOwned>(path: &Path) -> Result<Versioned<T>, ChatError> {
    let bytes = bytes(path)?;
    let raw: Value = serde_json::from_slice(&bytes).map_err(|_| ChatError::Corrupt)?;
    let version = raw["version"].as_u64().ok_or(ChatError::Corrupt)?;
    if version != u64::from(FORMAT) {
        return Err(ChatError::Unsupported(
            version.min(u64::from(u32::MAX)) as u32
        ));
    }
    let value: Envelope<T> = serde_json::from_value(raw).map_err(|_| ChatError::Corrupt)?;
    Ok(Versioned {
        revision: format!("{:x}", Sha256::digest(&bytes)),
        value: value.data,
    })
}
fn optional_record<T: DeserializeOwned>(path: &Path) -> Result<Option<Versioned<T>>, ChatError> {
    match read_record(path) {
        Ok(v) => Ok(Some(v)),
        Err(ChatError::Missing) => Ok(None),
        Err(e) => Err(e),
    }
}
fn write_record<T: Serialize + DeserializeOwned>(
    path: &Path,
    expected: Option<&str>,
    value: &T,
) -> Result<Versioned<T>, ChatError> {
    validate_path(path)?;
    let old: Option<Versioned<Value>> = optional_record(path)?;
    if old.as_ref().map(|v| v.revision.as_str()) != expected {
        return Err(ChatError::Conflict);
    }
    let mut raw = serde_json::to_value(Envelope {
        version: FORMAT,
        data: value,
        additional: BTreeMap::new(),
    })
    .map_err(|_| ChatError::Corrupt)?;
    if let Ok(previous) = bytes(path)
        .and_then(|v| serde_json::from_slice::<Value>(&v).map_err(|_| ChatError::Corrupt))
    {
        preserve(&mut raw, &previous);
    }
    let encoded = serde_json::to_vec(&raw).map_err(|_| ChatError::Corrupt)?;
    if encoded.len() > MAX_RECORD_BYTES {
        return Err(ChatError::TooLarge);
    }
    let directory = path.parent().ok_or(ChatError::Io)?;
    let temporary = directory.join(format!(".{}.tmp", new_id()));
    let result = (|| {
        let mut file = stillus_platform::create_private_file(&temporary)?;
        file.write_all(&encoded)?;
        file.sync_all()?;
        drop(file);
        if optional_record::<Value>(path)?
            .as_ref()
            .map(|v| v.revision.as_str())
            != expected
        {
            return Err(ChatError::Conflict);
        }
        if expected.is_none() {
            stillus_platform::publish(&temporary, path)?;
            #[cfg(unix)]
            fs::remove_file(&temporary)?;
        } else {
            fs::rename(&temporary, path)?;
        }
        stillus_platform::sync_directory(directory)?;
        Ok(Versioned {
            revision: format!("{:x}", Sha256::digest(&encoded)),
            value: serde_json::from_value(raw["data"].clone()).map_err(|_| ChatError::Corrupt)?,
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
fn preserve(value: &mut Value, old: &Value) {
    if let (Some(v), Some(o)) = (value.as_object_mut(), old.as_object()) {
        for (k, x) in o {
            if let Some(y) = v.get_mut(k) {
                // Order is a user-editable map: clearing keys must not restore old ranks.
                if k != "order" {
                    preserve(y, x);
                }
            } else {
                v.insert(k.clone(), x.clone());
            }
        }
    }
}

pub struct ChatEngineFactory;
impl FileEngineFactory for ChatEngineFactory {
    fn id(&self) -> EngineId {
        engine_id()
    }
    fn display_name(&self) -> &str {
        "AI Chat"
    }
    fn capabilities(&self) -> EngineCapabilities {
        EngineCapabilities {
            create: true,
            ..Default::default()
        }
    }
    fn settings_schema(&self) -> SettingsSchema {
        SettingsSchema::default()
    }
    fn ui_capabilities(&self) -> EngineUiCapabilities {
        EngineUiCapabilities {
            icon: EngineIcon::Chat,
            presentation: ItemPresentation::Chat,
            toolbar_actions: vec![
                ToolbarAction::Rename,
                ToolbarAction::Categories,
                ToolbarAction::Pin,
                ToolbarAction::Favorite,
                ToolbarAction::Delete,
                ToolbarAction::Restore,
            ],
        }
    }
    fn open(&self, workspace: &Path) -> Result<Box<dyn FileEngine>, EngineError> {
        Ok(Box::new(ChatStore::open(workspace)?))
    }
}
impl FileEngine for ChatStore {
    fn id(&self) -> EngineId {
        engine_id()
    }
    fn items(&self) -> Result<Vec<ItemSummary>, EngineError> {
        Ok(self.list()?.into_iter().map(|s| s.item).collect())
    }
    fn create(&mut self, settings: SettingsCandidate) -> Result<ItemId, EngineError> {
        ChatEngineFactory.validate_settings(&settings)?;
        Ok(self.create_chat(Metadata {
            common: CommonMetadata {
                title: "New chat".into(),
                ..Default::default()
            },
            automatic_title: true,
            alias: "default".into(),
        })?)
    }
    fn update_settings(
        &mut self,
        _: &ItemId,
        _: &str,
        _: SettingsCandidate,
    ) -> Result<String, EngineError> {
        Err(EngineError::Unsupported(
            "chat settings use typed commands".into(),
        ))
    }
    fn update_metadata(
        &mut self,
        item: &ItemId,
        expected: &str,
        patch: CommonMetadataPatch,
    ) -> Result<String, EngineError> {
        Ok(self
            .patch_metadata(item, expected, patch, None, true)?
            .revision)
    }
    fn referenced_secrets(&self) -> Result<Vec<ReferencedSecret>, EngineError> {
        Ok(Vec::new())
    }
    fn search_provider(&self) -> Option<&dyn SearchProvider> {
        None
    }
    fn background_tasks(&self) -> Vec<BackgroundTaskDescriptor> {
        Vec::new()
    }
    fn quiesce(&mut self) -> Result<(), EngineError> {
        Ok(())
    }
    fn resume(&mut self) {}
    fn security_rotated(&mut self) {}
}

#[cfg(test)]
mod tests;
