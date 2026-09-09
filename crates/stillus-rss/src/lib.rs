// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

//! Persistent RSS/Atom subscriptions, bounded feed caches and durable per-feed state.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use stillus_platform::fs::{self, OpenOptions};

use feed_rs::model::FeedType;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use stillus_engine::{
    CommonMetadata, CommonMetadataPatch, EngineCapabilities, EngineError, EngineIcon, EngineId,
    EngineUiCapabilities, FileEngine, FileEngineFactory, ItemAvailability, ItemId,
    ItemPresentation, ItemSummary, ReferencedSecret, SettingField, SettingFieldType, SettingValue,
    SettingsCandidate, SettingsSchema, ToolbarAction,
};
use url::Url;

pub const RSS_ENGINE_ID: &str = "rss";
pub const MAX_RESPONSE_BYTES: u64 = 5 * 1024 * 1024;
pub const MAX_ENTRY_TEXT_BYTES: usize = 64 * 1024;
pub const MAX_CACHED_ENTRIES: usize = 500;
const CONFIG_VERSION: u32 = 1;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
mod filter;
pub use filter::*;

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct RssEntry {
    pub id: String,
    pub title: String,
    pub author: Option<String>,
    pub published: Option<String>,
    pub updated: Option<String>,
    pub summary: String,
    pub link: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RssFeedCache {
    pub title: Option<String>,
    pub entries: Vec<RssEntry>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub fetched_at: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RssReadState {
    pub read_entry_ids: BTreeSet<String>,
    pub last_read_at: Option<String>,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub entries: BTreeMap<String, RssEntryState>,
    #[serde(default)]
    pub schedule: RssSchedule,
    #[serde(flatten)]
    pub additional: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RssSubscription {
    pub id: ItemId,
    pub url: String,
    pub title_override: Option<String>,
    pub created: String,
    pub modified: String,
    pub categories: Vec<String>,
    pub pinned: bool,
    pub favorited: bool,
    pub deleted: bool,
    pub order: BTreeMap<String, u32>,
    pub revision: u64,
    #[serde(default)]
    pub preferences: RssPreferences,
    #[serde(flatten)]
    pub additional: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RssSubscriptionSummary {
    pub subscription: RssSubscription,
    pub display_title: String,
    pub unread: u64,
    pub availability: ItemAvailability,
}

#[derive(Clone, Debug)]
pub struct RssRefreshRequest {
    pub item_id: ItemId,
    pub url: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

#[derive(Clone, Debug)]
pub enum RssRefreshResult {
    NotModified {
        item_id: ItemId,
        fetched_at: String,
    },
    Fetched {
        item_id: ItemId,
        cache: RssFeedCache,
    },
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct SubscriptionFile {
    version: u32,
    #[serde(default)]
    revision: u64,
    subscriptions: Vec<RssSubscription>,
    #[serde(flatten)]
    additional: BTreeMap<String, serde_json::Value>,
}

#[derive(Default)]
pub struct RssEngineFactory;

impl FileEngineFactory for RssEngineFactory {
    fn id(&self) -> EngineId {
        rss_engine_id()
    }

    fn display_name(&self) -> &str {
        "RSS"
    }

    fn capabilities(&self) -> EngineCapabilities {
        EngineCapabilities {
            create: true,
            settings: true,
            manual_tasks: true,
            ..EngineCapabilities::default()
        }
    }

    fn settings_schema(&self) -> SettingsSchema {
        SettingsSchema {
            fields: vec![SettingField {
                key: "source/url".to_owned(),
                label: "URL ленты".to_owned(),
                description: "Прямой HTTP/HTTPS URL RSS или Atom".to_owned(),
                required: true,
                default: None,
                field_type: SettingFieldType::Url,
            }],
        }
    }

    fn ui_capabilities(&self) -> EngineUiCapabilities {
        EngineUiCapabilities {
            icon: EngineIcon::Rss,
            presentation: ItemPresentation::Feed,
            toolbar_actions: vec![
                ToolbarAction::Refresh,
                ToolbarAction::Filters,
                ToolbarAction::Rename,
                ToolbarAction::Categories,
                ToolbarAction::Pin,
                ToolbarAction::Favorite,
                ToolbarAction::Delete,
                ToolbarAction::Restore,
            ],
        }
    }

    fn validate_settings(&self, candidate: &SettingsCandidate) -> Result<(), EngineError> {
        self.settings_schema()
            .validate_candidate(candidate, &BTreeSet::new())?;
        let Some(SettingValue::Url(url)) = candidate.public.get("source/url") else {
            return Err(EngineError::InvalidSetting("source/url".to_owned()));
        };
        normalize_feed_url(url).map(|_| ())
    }

    fn open(&self, workspace: &Path) -> Result<Box<dyn FileEngine>, EngineError> {
        Ok(Box::new(RssEngine::open(workspace)?))
    }
}

#[derive(Clone)]
pub struct RssEngine {
    workspace: PathBuf,
    subscriptions: Vec<RssSubscription>,
    config_revision: u64,
    diagnostic: Option<String>,
}

impl RssEngine {
    pub fn open(workspace: impl AsRef<Path>) -> Result<Self, EngineError> {
        let workspace = workspace.as_ref().to_path_buf();
        let path = config_path(&workspace);
        let (subscriptions, config_revision, diagnostic) = match read_bounded(&path) {
            Ok(bytes) => match serde_json::from_slice::<SubscriptionFile>(&bytes) {
                Ok(file) if file.version == CONFIG_VERSION => {
                    match validate_subscription_file(&file) {
                        Ok(()) => (file.subscriptions, file.revision, None),
                        Err(error) => (Vec::new(), 0, Some(error.to_string())),
                    }
                }
                Ok(file) => (
                    Vec::new(),
                    0,
                    Some(format!("unsupported RSS config version {}", file.version)),
                ),
                Err(error) => (
                    Vec::new(),
                    0,
                    Some(format!("RSS config is invalid: {error}")),
                ),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (Vec::new(), 0, None),
            Err(error) => (
                Vec::new(),
                0,
                Some(format!("RSS config is unavailable: {error}")),
            ),
        };
        Ok(Self {
            workspace,
            subscriptions,
            config_revision,
            diagnostic,
        })
    }

    pub fn diagnostic(&self) -> Option<&str> {
        self.diagnostic.as_deref()
    }

    pub fn subscriptions(&self) -> &[RssSubscription] {
        &self.subscriptions
    }

    pub fn config_revision(&self) -> u64 {
        self.config_revision
    }

    pub fn summaries(&self) -> Vec<RssSubscriptionSummary> {
        self.subscriptions
            .iter()
            .cloned()
            .map(|subscription| {
                let cache = self.load_cache(&subscription.id).unwrap_or_default();
                let state = self.load_read_state(&subscription.id).unwrap_or_default();
                let unread = cache
                    .entries
                    .iter()
                    .filter(|entry| {
                        !state.read_entry_ids.contains(&entry.id) && !state.hidden(&entry.id)
                    })
                    .count() as u64;
                let display_title = subscription
                    .title_override
                    .clone()
                    .or(cache.title)
                    .unwrap_or_else(|| provisional_title(&subscription.url));
                RssSubscriptionSummary {
                    subscription,
                    display_title,
                    unread,
                    availability: ItemAvailability::Ready,
                }
            })
            .collect()
    }

    pub fn create_subscription(
        &mut self,
        url: &str,
        categories: Vec<String>,
        favorited: bool,
        timestamp: &str,
    ) -> Result<ItemId, EngineError> {
        self.ensure_mutable()?;
        let normalized = normalize_feed_url(url)?;
        if self.subscriptions.iter().any(|item| item.url == normalized) {
            return Err(EngineError::Conflict);
        }
        let id = subscription_id(&normalized)?;
        self.subscriptions.push(RssSubscription {
            id: id.clone(),
            url: normalized,
            title_override: None,
            created: timestamp.to_owned(),
            modified: timestamp.to_owned(),
            categories,
            pinned: false,
            favorited,
            deleted: false,
            order: BTreeMap::new(),
            revision: 1,
            preferences: RssPreferences::default(),
            additional: BTreeMap::new(),
        });
        if let Err(error) = self.persist() {
            self.subscriptions.pop();
            return Err(error);
        }
        Ok(id)
    }

    pub fn update_subscription(
        &mut self,
        item_id: &ItemId,
        update: impl FnOnce(&mut RssSubscription),
    ) -> Result<(), EngineError> {
        self.ensure_mutable()?;
        let index = self
            .subscriptions
            .iter()
            .position(|item| &item.id == item_id)
            .ok_or_else(|| EngineError::Io("unknown RSS subscription".to_owned()))?;
        let previous = self.subscriptions[index].clone();
        let item = &mut self.subscriptions[index];
        update(item);
        item.revision = item.revision.saturating_add(1);
        if let Err(error) = self.persist() {
            self.subscriptions[index] = previous;
            return Err(error);
        }
        Ok(())
    }

    pub fn feed(&self, item_id: &ItemId) -> Result<(RssFeedCache, RssReadState), EngineError> {
        if !self.subscriptions.iter().any(|item| &item.id == item_id) {
            return Err(EngineError::Io("unknown RSS subscription".to_owned()));
        }
        Ok((
            self.load_cache(item_id).unwrap_or_default(),
            self.load_read_state(item_id)?,
        ))
    }

    pub fn mark_read(
        &self,
        item_id: &ItemId,
        entry_id: &str,
        timestamp: &str,
    ) -> Result<bool, EngineError> {
        let _lock = self.operation_lock()?;
        let (_, mut state) = self.feed(item_id)?;
        if !state.read_entry_ids.insert(entry_id.to_owned()) {
            return Ok(false);
        }
        state.last_read_at = Some(timestamp.to_owned());
        state.revision = state.revision.checked_add(1).ok_or(EngineError::Conflict)?;
        write_json_atomic(&read_state_path(&self.workspace, item_id), &state)?;
        Ok(true)
    }

    pub fn refresh_request(&self, item_id: &ItemId) -> Result<RssRefreshRequest, EngineError> {
        let subscription = self
            .subscriptions
            .iter()
            .find(|item| &item.id == item_id)
            .ok_or_else(|| EngineError::Io("unknown RSS subscription".to_owned()))?;
        let cache = self.load_cache(item_id).unwrap_or_default();
        Ok(RssRefreshRequest {
            item_id: item_id.clone(),
            url: subscription.url.clone(),
            etag: cache.etag,
            last_modified: cache.last_modified,
        })
    }

    pub fn apply_refresh(&mut self, result: RssRefreshResult) -> Result<(), EngineError> {
        let _lock = self.operation_lock()?;
        let item_id = match &result {
            RssRefreshResult::NotModified { item_id, .. }
            | RssRefreshResult::Fetched { item_id, .. } => item_id.clone(),
        };
        if !self
            .subscriptions
            .iter()
            .any(|item| item.id == item_id && !item.deleted)
        {
            return Err(EngineError::Io("unknown RSS subscription".to_owned()));
        }
        match result {
            RssRefreshResult::NotModified { fetched_at, .. } => {
                let mut cache = self.load_cache(&item_id).unwrap_or_default();
                cache.fetched_at = Some(fetched_at);
                write_json_atomic(&cache_path(&self.workspace, &item_id), &cache)?;
                // A previous refresh may have committed the cache but failed to
                // persist its decisions. HTTP 304 must finish that local work.
                self.apply_filter(&item_id, RssFilterMode::Changed)?;
            }
            RssRefreshResult::Fetched { mut cache, .. } => {
                let previous = self.load_cache(&item_id).unwrap_or_default();
                let state_missing = previous.fetched_at.is_none() && previous.entries.is_empty();
                let mut state = self.load_read_state(&item_id)?;
                let mut seen = HashSet::new();
                cache.entries.retain(|entry| seen.insert(entry.id.clone()));
                cache.entries.truncate(MAX_CACHED_ENTRIES);
                if state_missing {
                    state
                        .read_entry_ids
                        .extend(cache.entries.iter().skip(10).map(|entry| entry.id.clone()));
                }
                let current_ids = cache
                    .entries
                    .iter()
                    .map(|entry| entry.id.as_str())
                    .collect::<HashSet<_>>();
                state
                    .read_entry_ids
                    .retain(|entry_id| current_ids.contains(entry_id.as_str()));
                state
                    .entries
                    .retain(|entry_id, _| current_ids.contains(entry_id.as_str()));
                let preferences = self.preferences(&item_id)?;
                preferences.compile()?.apply(
                    &cache,
                    &mut state,
                    preferences.version,
                    RssFilterMode::Changed,
                );
                state.revision = state.revision.checked_add(1).ok_or(EngineError::Conflict)?;
                let changed = previous.title != cache.title || previous.entries != cache.entries;
                write_json_atomic(&cache_path(&self.workspace, &item_id), &cache)?;
                write_json_atomic(&read_state_path(&self.workspace, &item_id), &state)?;
                if changed {
                    let timestamp = cache.fetched_at.clone().unwrap_or_else(now_timestamp);
                    self.update_subscription(&item_id, |item| item.modified = timestamp)?;
                }
            }
        }
        Ok(())
    }

    fn ensure_mutable(&self) -> Result<(), EngineError> {
        if let Some(diagnostic) = &self.diagnostic {
            Err(EngineError::Io(diagnostic.clone()))
        } else {
            Ok(())
        }
    }

    fn persist(&mut self) -> Result<(), EngineError> {
        let _lock = self.operation_lock()?;
        let path = config_path(&self.workspace);
        let additional = read_json_or_default::<SubscriptionFile>(&path)?.additional;
        let disk_revision = match read_bounded(&path) {
            Ok(bytes) => {
                serde_json::from_slice::<SubscriptionFile>(&bytes)
                    .map_err(|error| EngineError::Io(format!("RSS config is invalid: {error}")))?
                    .revision
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => return Err(EngineError::Io(error.to_string())),
        };
        if disk_revision != self.config_revision {
            return Err(EngineError::Conflict);
        }
        let next_revision = self
            .config_revision
            .checked_add(1)
            .ok_or_else(|| EngineError::Io("RSS config revision overflow".to_owned()))?;
        write_json_atomic(
            &path,
            &SubscriptionFile {
                version: CONFIG_VERSION,
                revision: next_revision,
                subscriptions: self.subscriptions.clone(),
                additional,
            },
        )?;
        self.config_revision = next_revision;
        Ok(())
    }

    fn load_cache(&self, item_id: &ItemId) -> Result<RssFeedCache, EngineError> {
        read_json_or_default(&cache_path(&self.workspace, item_id))
    }

    fn load_read_state(&self, item_id: &ItemId) -> Result<RssReadState, EngineError> {
        read_json_or_default(&read_state_path(&self.workspace, item_id))
    }

    pub fn operation_lock(&self) -> Result<stillus_platform::OperationLock, EngineError> {
        let directory = self.workspace.join(".stillus/engines/rss");
        ensure_directories(&directory)?;
        stillus_platform::OperationLock::directory(&directory)
            .map_err(|_| EngineError::Io("RSS lock unavailable".into()))
    }
}

impl FileEngine for RssEngine {
    fn id(&self) -> EngineId {
        rss_engine_id()
    }

    fn items(&self) -> Result<Vec<ItemSummary>, EngineError> {
        Ok(self
            .summaries()
            .into_iter()
            .map(|summary| ItemSummary {
                engine_id: rss_engine_id(),
                item_id: summary.subscription.id,
                metadata_version: summary.subscription.revision.to_string(),
                metadata: CommonMetadata {
                    title: summary.display_title,
                    categories: summary.subscription.categories,
                    pinned: summary.subscription.pinned,
                    favorited: summary.subscription.favorited,
                    deleted: summary.subscription.deleted,
                    created: Some(summary.subscription.created),
                    modified: Some(summary.subscription.modified),
                    order: summary.subscription.order,
                },
                availability: summary.availability,
                badge: Some(summary.unread),
            })
            .collect())
    }

    fn create(&mut self, settings: SettingsCandidate) -> Result<ItemId, EngineError> {
        let url = match settings.public.get("source/url") {
            Some(SettingValue::Url(url)) => url,
            _ => return Err(EngineError::InvalidSetting("source/url".to_owned())),
        };
        self.create_subscription(url, Vec::new(), false, &now_timestamp())
    }

    fn update_settings(
        &mut self,
        _item: &ItemId,
        _expected_version: &str,
        _settings: SettingsCandidate,
    ) -> Result<String, EngineError> {
        Err(EngineError::Unsupported(
            "use RSS subscription metadata operations".to_owned(),
        ))
    }

    fn update_metadata(
        &mut self,
        item_id: &ItemId,
        expected_version: &str,
        patch: CommonMetadataPatch,
    ) -> Result<String, EngineError> {
        self.update_metadata_at(item_id, expected_version, patch, &now_timestamp())
    }

    fn referenced_secrets(&self) -> Result<Vec<ReferencedSecret>, EngineError> {
        Ok(Vec::new())
    }

    fn search_provider(&self) -> Option<&dyn stillus_engine::SearchProvider> {
        None
    }

    fn background_tasks(&self) -> Vec<stillus_engine::BackgroundTaskDescriptor> {
        vec![stillus_engine::BackgroundTaskDescriptor {
            id: stillus_engine::TaskId("refresh".to_owned()),
            label: "Обновить".to_owned(),
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

pub fn execute_refresh(request: RssRefreshRequest) -> Result<RssRefreshResult, EngineError> {
    let url = normalize_feed_url(&request.url)?;
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .https_only(false)
        .max_redirects(5)
        .build();
    let agent = ureq::Agent::new_with_config(config);
    let mut call = agent
        .get(&url)
        .header("User-Agent", concat!("Stillus/", env!("CARGO_PKG_VERSION")))
        .header(
            "Accept",
            "application/rss+xml, application/atom+xml, application/xml, text/xml",
        );
    if let Some(etag) = request.etag.as_deref() {
        call = call.header("If-None-Match", etag);
    }
    if let Some(last_modified) = request.last_modified.as_deref() {
        call = call.header("If-Modified-Since", last_modified);
    }
    let mut response = call
        .call()
        .map_err(|error| EngineError::Io(error.to_string()))?;
    let fetched_at = now_timestamp();
    if response.status().as_u16() == 304 {
        return Ok(RssRefreshResult::NotModified {
            item_id: request.item_id,
            fetched_at,
        });
    }
    let etag = response
        .headers()
        .get("etag")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let last_modified = response
        .headers()
        .get("last-modified")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let bytes = response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_vec()
        .map_err(|error| EngineError::Io(error.to_string()))?;
    let mut cache = parse_feed(&bytes)?;
    cache.etag = etag;
    cache.last_modified = last_modified;
    cache.fetched_at = Some(fetched_at);
    Ok(RssRefreshResult::Fetched {
        item_id: request.item_id,
        cache,
    })
}

pub fn open_original(url: &str) -> Result<(), EngineError> {
    let parsed =
        Url::parse(url).map_err(|error| EngineError::Io(format!("invalid entry URL: {error}")))?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(EngineError::Io(
            "entry URL must be an unauthenticated HTTP or HTTPS URL".to_owned(),
        ));
    }
    webbrowser::open(parsed.as_str())
        .map_err(|error| EngineError::Io(format!("cannot open system browser: {error}")))
}

pub fn parse_feed(bytes: &[u8]) -> Result<RssFeedCache, EngineError> {
    if bytes.len() as u64 > MAX_RESPONSE_BYTES {
        return Err(EngineError::Io("RSS response exceeds 5 MiB".to_owned()));
    }
    let feed = feed_rs::parser::parse(bytes).map_err(|error| EngineError::Io(error.to_string()))?;
    if feed.feed_type == FeedType::JSON {
        return Err(EngineError::Unsupported(
            "JSON Feed is not supported".to_owned(),
        ));
    }
    let title = feed.title.map(|title| clean_text(&title.content));
    let mut entries = feed
        .entries
        .into_iter()
        .enumerate()
        .map(|(source_index, entry)| {
            let title = entry
                .title
                .as_ref()
                .map(|value| clean_text(&value.content))
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "Без названия".to_owned());
            let summary_source = entry
                .summary
                .as_ref()
                .map(|value| value.content.as_str())
                .or_else(|| entry.content.as_ref()?.body.as_deref())
                .unwrap_or_default();
            // Keep safe textual Markdown and link definitions for the native
            // card renderer; never retain executable HTML or fetch its assets.
            let summary = truncate_utf8(
                html2text::from_read(summary_source.as_bytes(), 120)
                    .unwrap_or_else(|_| summary_source.to_owned())
                    .trim()
                    .to_owned(),
                MAX_ENTRY_TEXT_BYTES,
            );
            let link = entry
                .links
                .iter()
                .find(|link| link.rel.as_deref().is_none_or(|rel| rel == "alternate"))
                .or_else(|| entry.links.first())
                .and_then(|link| normalize_entry_link(&link.href));
            let published = entry.published.map(|date| date.to_rfc3339());
            let updated = entry.updated.map(|date| date.to_rfc3339());
            let raw_id = if entry.id.trim().is_empty() {
                link.clone().unwrap_or_else(|| {
                    // Keep fallback IDs compatible with previously cached feeds.
                    let legacy_summary =
                        truncate_utf8(clean_text(summary_source), MAX_ENTRY_TEXT_BYTES);
                    digest_string(&format!(
                        "{title}\n{:?}\n{legacy_summary}",
                        published.as_deref()
                    ))
                })
            } else {
                entry.id
            };
            (
                source_index,
                RssEntry {
                    id: digest_string(&raw_id),
                    title,
                    author: entry.authors.first().map(|author| author.name.clone()),
                    published,
                    updated,
                    summary,
                    link,
                },
            )
        })
        .collect::<Vec<_>>();
    entries.sort_by(|(left_index, left), (right_index, right)| {
        let left_date = left.published.as_ref().or(left.updated.as_ref());
        let right_date = right.published.as_ref().or(right.updated.as_ref());
        match (left_date, right_date) {
            (Some(left), Some(right)) => right.cmp(left).then(left_index.cmp(right_index)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => left_index.cmp(right_index),
        }
    });
    let entries = entries
        .into_iter()
        .map(|(_, entry)| entry)
        .take(MAX_CACHED_ENTRIES)
        .collect();
    Ok(RssFeedCache {
        title,
        entries,
        ..RssFeedCache::default()
    })
}

pub fn normalize_feed_url(value: &str) -> Result<String, EngineError> {
    if value.len() > 2_048 {
        return Err(EngineError::InvalidSetting("source/url".to_owned()));
    }
    let mut url = Url::parse(value.trim())
        .map_err(|_| EngineError::InvalidSetting("source/url".to_owned()))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(EngineError::InvalidSetting("source/url".to_owned()));
    }
    url.set_fragment(None);
    // Url already removes the default port for each scheme. Keep other ports,
    // including HTTP on port 443.
    Ok(url.to_string())
}

pub fn rss_engine_id() -> EngineId {
    EngineId::new(RSS_ENGINE_ID).expect("built-in RSS engine id is valid")
}

fn validate_subscription_file(file: &SubscriptionFile) -> Result<(), EngineError> {
    let mut urls = HashSet::new();
    let mut ids = HashSet::new();
    for item in &file.subscriptions {
        let normalized = normalize_feed_url(&item.url)?;
        if normalized != item.url || subscription_id(&normalized)? != item.id {
            return Err(EngineError::Io(
                "RSS subscription ID or URL is not canonical".to_owned(),
            ));
        }
        if !urls.insert(item.url.as_str()) || !ids.insert(item.id.as_str()) {
            return Err(EngineError::Io(
                "RSS subscriptions contain a duplicate".to_owned(),
            ));
        }
        if item
            .title_override
            .as_ref()
            .is_some_and(|title| title.trim().is_empty() || title.len() > 200)
        {
            return Err(EngineError::Io(
                "RSS subscription title is invalid".to_owned(),
            ));
        }
    }
    Ok(())
}

fn subscription_id(url: &str) -> Result<ItemId, EngineError> {
    ItemId::new(format!("feeds/{}", digest_string(url)))
}

fn provisional_title(url: &str) -> String {
    Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_else(|| "RSS лента".to_owned())
}

fn normalize_entry_link(value: &str) -> Option<String> {
    let mut url = Url::parse(value).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    url.set_fragment(None);
    Some(url.to_string())
}

fn clean_text(value: &str) -> String {
    let rendered = html2text::from_read(value.as_bytes(), 120).unwrap_or_else(|_| value.to_owned());
    let plain = rendered
        .chars()
        .filter(|character| !matches!(character, '*' | '`'))
        .collect::<String>();
    plain
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_owned()
}

fn truncate_utf8(mut value: String, limit: usize) -> String {
    if value.len() <= limit {
        return value;
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value
}

fn digest_string(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn config_path(workspace: &Path) -> PathBuf {
    workspace.join(".stillus/engines/rss/subscriptions.json")
}

fn cache_directory(workspace: &Path, item_id: &ItemId) -> PathBuf {
    let digest = item_id.as_str().rsplit('/').next().unwrap_or("invalid");
    workspace.join(".stillus/cache/rss").join(digest)
}

fn cache_path(workspace: &Path, item_id: &ItemId) -> PathBuf {
    cache_directory(workspace, item_id).join("feed.json")
}

fn read_state_path(workspace: &Path, item_id: &ItemId) -> PathBuf {
    cache_directory(workspace, item_id).join("state.json")
}

fn read_json_or_default<T>(path: &Path) -> Result<T, EngineError>
where
    T: serde::de::DeserializeOwned + Default,
{
    match read_bounded(path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).map_err(|_| EngineError::Io("invalid RSS state".into()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(EngineError::Io(error.to_string())),
    }
}

fn read_bounded(path: &Path) -> std::io::Result<Vec<u8>> {
    let file = OpenOptions::new().read(true).open(path)?;
    let mut bytes = Vec::new();
    file.take(64 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 64 * 1024 * 1024 {
        return Err(std::io::Error::other("RSS file exceeds limit"));
    }
    Ok(bytes)
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), EngineError> {
    let directory = path
        .parent()
        .ok_or_else(|| EngineError::Io("RSS path has no parent".to_owned()))?;
    ensure_directories(directory)?;
    if fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.file_type().is_symlink() || metadata.is_dir())
    {
        return Err(EngineError::Io(
            "RSS destination is not a regular file".to_owned(),
        ));
    }
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = directory.join(format!(
        ".rss.stillus-tmp-{}-{sequence}",
        std::process::id()
    ));
    let mut value =
        serde_json::to_value(value).map_err(|_| EngineError::Io("invalid RSS data".into()))?;
    // Cache formats predate typed extension maps. Preserve future object fields
    // and future fields of surviving entries without resurrecting removed IDs.
    if let Ok(bytes) = read_bounded(path)
        && let Ok(old) = serde_json::from_slice::<serde_json::Value>(&bytes)
    {
        preserve_unknown(&mut value, &old);
    }
    let mut bytes = serde_json::to_vec_pretty(&value)
        .map_err(|_| EngineError::Io("invalid RSS data".into()))?;
    if bytes.len() > 64 * 1024 * 1024 {
        return Err(EngineError::Io("RSS file exceeds limit".into()));
    }
    bytes.push(b'\n');
    let write_result = (|| -> Result<(), EngineError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| EngineError::Io(error.to_string()))?;
        file.write_all(&bytes)
            .map_err(|error| EngineError::Io(error.to_string()))?;
        file.sync_all()
            .map_err(|error| EngineError::Io(error.to_string()))?;
        drop(file);
        fs::rename(&temporary, path).map_err(|error| EngineError::Io(error.to_string()))?;
        stillus_platform::sync_directory(directory)
            .map_err(|error| EngineError::Io(error.to_string()))?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result
}

fn preserve_unknown(value: &mut serde_json::Value, old: &serde_json::Value) {
    if let (Some(current), Some(previous)) = (value.as_object_mut(), old.as_object()) {
        for (key, field) in previous {
            if !current.contains_key(key) {
                current.insert(key.clone(), field.clone());
            }
        }
        if let Some(entries) = current
            .get_mut("entries")
            .and_then(serde_json::Value::as_array_mut)
            && let Some(previous) = previous
                .get("entries")
                .and_then(serde_json::Value::as_array)
        {
            for entry in entries {
                if let Some(old) = previous.iter().find(|old| old["id"] == entry["id"]) {
                    preserve_unknown(entry, old);
                }
            }
        }
    }
}

fn ensure_directories(path: &Path) -> Result<(), EngineError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(EngineError::Io(format!(
                    "invalid RSS directory: {}",
                    current.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::create_dir(&current) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        let metadata = fs::symlink_metadata(&current)
                            .map_err(|error| EngineError::Io(error.to_string()))?;
                        if metadata.file_type().is_symlink() || !metadata.is_dir() {
                            return Err(EngineError::Io(format!(
                                "invalid RSS directory: {}",
                                current.display()
                            )));
                        }
                    }
                    Err(error) => return Err(EngineError::Io(error.to_string())),
                }
            }
            Err(error) => return Err(EngineError::Io(error.to_string())),
        }
    }
    Ok(())
}

fn now_timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

impl RssEngine {
    pub fn update_metadata_at(
        &mut self,
        item_id: &ItemId,
        expected_version: &str,
        patch: CommonMetadataPatch,
        timestamp: &str,
    ) -> Result<String, EngineError> {
        if patch
            .title
            .as_ref()
            .is_some_and(|title| title.trim().is_empty() || title.len() > 200)
        {
            return Err(EngineError::InvalidSetting("title".to_owned()));
        }
        let expected = expected_version
            .parse::<u64>()
            .map_err(|_| EngineError::Conflict)?;
        let current = self
            .subscriptions
            .iter()
            .find(|item| &item.id == item_id)
            .ok_or_else(|| EngineError::Io("unknown RSS subscription".to_owned()))?;
        if current.revision != expected {
            return Err(EngineError::Conflict);
        }
        self.update_subscription(item_id, |item| {
            item.modified = timestamp.to_owned();
            if let Some(title) = patch.title {
                item.title_override = Some(title);
            }
            if let Some(categories) = patch.categories {
                item.categories = categories;
            }
            if let Some(pinned) = patch.pinned {
                item.pinned = pinned;
            }
            if let Some(favorited) = patch.favorited {
                item.favorited = favorited;
            }
            if let Some(deleted) = patch.deleted {
                item.deleted = deleted;
            }
            if let Some(order) = patch.order {
                item.order = order;
            }
        })?;
        Ok(self
            .subscriptions
            .iter()
            .find(|item| &item.id == item_id)
            .expect("updated RSS subscription remains available")
            .revision
            .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RSS: &str = r#"<?xml version="1.0"?><rss version="2.0"><channel><title>Example</title><link>https://example.test/</link><description>Feed</description><item><guid>one</guid><title>First</title><link>https://example.test/one</link><description><![CDATA[<b>Hello</b> world]]></description><pubDate>Mon, 01 Sep 2025 10:00:00 +0000</pubDate></item></channel></rss>"#;
    const ATOM: &str = r#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom"><title>Atom Example</title><id>https://example.test/</id><updated>2025-09-01T10:00:00Z</updated><entry><title>Atom entry</title><id>tag:example.test,2025:1</id><updated>2025-09-01T10:00:00Z</updated><author><name>Ada</name></author><link rel="alternate" href="https://example.test/atom"/><content type="html">&lt;p&gt;Safe &lt;strong&gt;text&lt;/strong&gt;&lt;/p&gt;</content></entry></feed>"#;
    const RSS_ONE: &str = r#"<?xml version="1.0"?><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns="http://purl.org/rss/1.0/"><channel rdf:about="https://example.test/feed"><title>RSS One</title><link>https://example.test/</link><description>Feed</description><items><rdf:Seq><rdf:li resource="https://example.test/one"/></rdf:Seq></items></channel><item rdf:about="https://example.test/one"><title>One</title><link>https://example.test/one</link><description>Body</description></item></rdf:RDF>"#;

    #[test]
    fn parses_rss_as_bounded_native_markdown() {
        let feed = parse_feed(RSS.as_bytes()).unwrap();
        assert_eq!(feed.title.as_deref(), Some("Example"));
        assert_eq!(feed.entries[0].summary, "**Hello** world");
        assert_eq!(
            feed.entries[0].link.as_deref(),
            Some("https://example.test/one")
        );
    }

    #[test]
    fn parses_atom_and_rss_one_without_executing_markup() {
        let atom = parse_feed(ATOM.as_bytes()).unwrap();
        assert_eq!(atom.title.as_deref(), Some("Atom Example"));
        assert_eq!(atom.entries[0].author.as_deref(), Some("Ada"));
        assert_eq!(atom.entries[0].summary, "Safe **text**");
        let rss_one = parse_feed(RSS_ONE.as_bytes()).unwrap();
        assert_eq!(rss_one.title.as_deref(), Some("RSS One"));
        assert_eq!(rss_one.entries[0].title, "One");
    }

    #[test]
    fn preserves_read_more_references_and_strips_executable_html() {
        let xml = RSS.replace("<b>Hello</b> world", r#"<p><b>Hello</b> world</p><a href="https://example.test/read">Читать далее</a><script>alert('never')</script>"#);
        let feed = parse_feed(xml.as_bytes()).unwrap();
        let summary = &feed.entries[0].summary;
        assert!(summary.contains("**Hello** world"));
        assert!(summary.contains("[Читать далее][1]"));
        assert!(summary.contains("[1]: https://example.test/read"));
        assert!(!summary.contains("script"));
        assert!(!summary.contains("alert"));
    }

    #[test]
    fn rejects_malformed_xml_json_feed_and_oversized_input() {
        assert!(parse_feed(b"<rss><broken>").is_err());
        assert!(
            parse_feed(
                br#"{"version":"https://jsonfeed.org/version/1.1","title":"JSON","items":[]}"#
            )
            .is_err()
        );
        assert!(parse_feed(&vec![b'x'; MAX_RESPONSE_BYTES as usize + 1]).is_err());
    }

    #[test]
    fn deduplicates_ids_and_bounds_entry_text() {
        let long = "word ".repeat(MAX_ENTRY_TEXT_BYTES);
        let xml = format!(
            "<?xml version=\"1.0\"?><rss version=\"2.0\"><channel><title>X</title><link>https://example.test/</link><description>X</description><item><guid>same</guid><description>{long}</description></item><item><guid>same</guid><title>duplicate</title></item></channel></rss>"
        );
        let mut engine_cache = parse_feed(xml.as_bytes()).unwrap();
        assert!(engine_cache.entries[0].summary.len() <= MAX_ENTRY_TEXT_BYTES);
        let root = tempfile::tempdir().unwrap();
        let mut engine = RssEngine::open(root.path()).unwrap();
        let id = engine
            .create_subscription("https://example.test/feed", Vec::new(), false, "1")
            .unwrap();
        engine_cache.fetched_at = Some("2".to_owned());
        engine
            .apply_refresh(RssRefreshResult::Fetched {
                item_id: id.clone(),
                cache: engine_cache,
            })
            .unwrap();
        assert_eq!(engine.feed(&id).unwrap().0.entries.len(), 1);
        assert_eq!(engine.feed(&id).unwrap().0.entries[0].title, "Без названия");
    }

    #[test]
    fn accepts_http_and_https_without_host_or_port_restrictions() {
        assert_eq!(
            normalize_feed_url(" https://EXAMPLE.test:443/feed#x ").unwrap(),
            "https://example.test/feed"
        );
        assert_eq!(
            normalize_feed_url(" http://EXAMPLE.test:80/feed#x ").unwrap(),
            "http://example.test/feed"
        );
        for url in [
            "http://localhost:8080/feed",
            "http://127.0.0.1:443/feed",
            "http://192.168.1.10/feed",
            "http://[::1]:8080/feed",
            "https://example.test:8443/feed",
        ] {
            assert_eq!(normalize_feed_url(url).unwrap(), url);
        }
    }

    #[test]
    fn rejects_non_web_urls_and_credentials() {
        for url in [
            "ftp://example.test/feed",
            "file:///tmp/feed.xml",
            "https://user@example.test/feed",
            "http://user:pass@example.test/feed",
        ] {
            assert!(normalize_feed_url(url).is_err());
            assert!(normalize_entry_link(url).is_none());
            assert!(open_original(url).is_err());
        }
    }

    #[test]
    fn http_subscription_survives_reopening_and_preserves_article_links() {
        let root = tempfile::tempdir().unwrap();
        let mut engine = RssEngine::open(root.path()).unwrap();
        let id = engine
            .create_subscription("http://localhost:8080/feed", Vec::new(), false, "1")
            .unwrap();
        let reopened = RssEngine::open(root.path()).unwrap();
        assert!(reopened.diagnostic().is_none());
        assert_eq!(
            reopened.refresh_request(&id).unwrap().url,
            "http://localhost:8080/feed"
        );
        for source in [RSS, ATOM, RSS_ONE] {
            let cache = parse_feed(
                source
                    .replace("https://example.test", "http://example.test")
                    .as_bytes(),
            )
            .unwrap();
            assert!(
                cache.entries[0]
                    .link
                    .as_deref()
                    .unwrap()
                    .starts_with("http://example.test/")
            );
        }
    }

    #[test]
    fn subscription_survives_cache_loss_and_first_ten_are_unread() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("notes")).unwrap();
        let mut engine = RssEngine::open(root.path()).unwrap();
        let id = engine
            .create_subscription(
                "https://example.test/feed",
                vec!["Work".to_owned()],
                false,
                "1",
            )
            .unwrap();
        let mut cache = parse_feed(RSS.as_bytes()).unwrap();
        cache.entries = (0..12)
            .map(|index| RssEntry {
                id: index.to_string(),
                title: index.to_string(),
                author: None,
                published: None,
                updated: None,
                summary: String::new(),
                link: None,
            })
            .collect();
        cache.fetched_at = Some("2".to_owned());
        engine
            .apply_refresh(RssRefreshResult::Fetched {
                item_id: id.clone(),
                cache,
            })
            .unwrap();
        assert_eq!(engine.summaries()[0].unread, 10);
        fs::remove_dir_all(root.path().join(".stillus/cache/rss")).unwrap();
        let reopened = RssEngine::open(root.path()).unwrap();
        assert_eq!(reopened.subscriptions().len(), 1);
        assert_eq!(reopened.feed(&id).unwrap().0.entries.len(), 0);
    }

    #[test]
    fn duplicate_url_is_rejected_and_read_state_survives_updates() {
        let root = tempfile::tempdir().unwrap();
        let mut engine = RssEngine::open(root.path()).unwrap();
        let id = engine
            .create_subscription("https://example.test/feed", Vec::new(), false, "1")
            .unwrap();
        assert!(
            engine
                .create_subscription(
                    "https://EXAMPLE.test:443/feed#fragment",
                    Vec::new(),
                    false,
                    "1"
                )
                .is_err()
        );
        let mut cache = parse_feed(RSS.as_bytes()).unwrap();
        cache.fetched_at = Some("2".to_owned());
        engine
            .apply_refresh(RssRefreshResult::Fetched {
                item_id: id.clone(),
                cache: cache.clone(),
            })
            .unwrap();
        assert!(engine.mark_read(&id, &cache.entries[0].id, "3").unwrap());
        cache.entries[0].summary = "updated body".to_owned();
        engine
            .apply_refresh(RssRefreshResult::Fetched {
                item_id: id.clone(),
                cache,
            })
            .unwrap();
        assert_eq!(engine.summaries()[0].unread, 0);
    }

    #[test]
    fn stale_config_revision_cannot_overwrite_a_newer_subscription() {
        let root = tempfile::tempdir().unwrap();
        let mut first = RssEngine::open(root.path()).unwrap();
        let mut stale = RssEngine::open(root.path()).unwrap();
        first
            .create_subscription("https://example.test/first", Vec::new(), false, "1")
            .unwrap();
        assert!(matches!(
            stale.create_subscription("https://example.test/stale", Vec::new(), false, "1"),
            Err(EngineError::Conflict)
        ));
        assert!(stale.subscriptions().is_empty());
        assert_eq!(
            RssEngine::open(root.path()).unwrap().subscriptions().len(),
            1
        );
    }

    #[test]
    fn generic_metadata_patch_checks_item_revision() {
        let root = tempfile::tempdir().unwrap();
        let mut engine = RssEngine::open(root.path()).unwrap();
        let id = engine
            .create_subscription("https://example.test/feed", Vec::new(), false, "1")
            .unwrap();
        let version = engine.items().unwrap()[0].metadata_version.clone();
        let next = engine
            .update_metadata(
                &id,
                &version,
                CommonMetadataPatch {
                    title: Some("Renamed".to_owned()),
                    favorited: Some(true),
                    ..CommonMetadataPatch::default()
                },
            )
            .unwrap();
        assert_ne!(version, next);
        assert!(matches!(
            engine.update_metadata(&id, &version, CommonMetadataPatch::default()),
            Err(EngineError::Conflict)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn cache_write_rejects_symlinked_directory() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let mut engine = RssEngine::open(root.path()).unwrap();
        let id = engine
            .create_subscription("https://example.test/feed", Vec::new(), false, "1")
            .unwrap();
        fs::create_dir_all(root.path().join(".stillus/cache")).unwrap();
        symlink(outside.path(), root.path().join(".stillus/cache/rss")).unwrap();
        let mut cache = parse_feed(RSS.as_bytes()).unwrap();
        cache.fetched_at = Some("2".to_owned());
        assert!(
            engine
                .apply_refresh(RssRefreshResult::Fetched { item_id: id, cache })
                .is_err()
        );
    }
}
