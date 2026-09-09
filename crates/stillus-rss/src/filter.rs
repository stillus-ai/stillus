// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

use super::*;
use regex::{RegexBuilder, RegexSet, RegexSetBuilder};

pub const MAX_PREFERENCE_BYTES: usize = 16 * 1024;
const MAX_REGEX_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RssPreferences {
    pub blacklist: String,
    pub whitelist: String,
    pub version: u64,
    #[serde(flatten)]
    pub additional: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RssFilterError {
    TooLong,
    Invalid { blacklist: bool, line: usize },
    TooComplex { blacklist: bool },
}

impl From<RssFilterError> for EngineError {
    fn from(error: RssFilterError) -> Self {
        let path = match error {
            RssFilterError::TooLong => "rss/preferences/size".into(),
            RssFilterError::Invalid { blacklist, line } => format!(
                "rss/filter/{}/line/{line}",
                if blacklist { "blacklist" } else { "whitelist" }
            ),
            RssFilterError::TooComplex { blacklist } => format!(
                "rss/filter/{}/size",
                if blacklist { "blacklist" } else { "whitelist" }
            ),
        };
        Self::InvalidSetting(path)
    }
}

pub struct RssFilter {
    blacklist: RegexSet,
    whitelist: RegexSet,
}

impl RssPreferences {
    pub fn compile(&self) -> Result<RssFilter, RssFilterError> {
        if self.blacklist.len() > MAX_PREFERENCE_BYTES
            || self.whitelist.len() > MAX_PREFERENCE_BYTES
        {
            return Err(RssFilterError::TooLong);
        }
        Ok(RssFilter {
            blacklist: compile_list(&self.blacklist, true)?,
            whitelist: compile_list(&self.whitelist, false)?,
        })
    }

    pub fn validate(&self) -> Result<(), EngineError> {
        self.compile().map(|_| ()).map_err(Into::into)
    }
}

fn compile_list(text: &str, blacklist: bool) -> Result<RegexSet, RssFilterError> {
    let lines = text
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .collect::<Vec<_>>();
    // Compile the whole list once in the normal path, with an aggregate memory limit.
    if let Ok(set) = RegexSetBuilder::new(lines.iter().map(|(_, line)| *line))
        .case_insensitive(true)
        .size_limit(MAX_REGEX_BYTES)
        .dfa_size_limit(MAX_REGEX_BYTES)
        .build()
    {
        return Ok(set);
    }
    // On failure, identify the original physical line without exposing the pattern.
    // Keep nonempty patterns verbatim: spaces can be meaningful in regexp.
    for (index, pattern) in lines {
        RegexBuilder::new(pattern)
            .case_insensitive(true)
            .size_limit(MAX_REGEX_BYTES)
            .dfa_size_limit(MAX_REGEX_BYTES)
            .build()
            .map_err(|_| RssFilterError::Invalid {
                blacklist,
                line: index + 1,
            })?;
    }
    Err(RssFilterError::TooComplex { blacklist })
}

impl RssFilter {
    pub fn decision(&self, entry: &RssEntry) -> RssDecision {
        let text = format!("{}\n{}", entry.title, entry.summary);
        if self.blacklist.is_match(&text) && !self.whitelist.is_match(&text) {
            RssDecision::Hide
        } else {
            RssDecision::Keep
        }
    }

    pub(crate) fn apply(
        &self,
        feed: &RssFeedCache,
        state: &mut RssReadState,
        version: u64,
        mode: RssFilterMode,
    ) {
        for article in &feed.entries {
            let content_version = content_version(article);
            let entry = state.entries.entry(article.id.clone()).or_default();
            if mode == RssFilterMode::All || entry.content_version != content_version {
                entry.decision = Some(self.decision(article));
                entry.preferences_version = version;
                entry.content_version = content_version;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RssFilterMode {
    Changed,
    All,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RssDecision {
    Keep,
    Hide,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RssEntryState {
    pub decision: Option<RssDecision>,
    pub preferences_version: u64,
    pub content_version: String,
    #[serde(flatten)]
    pub additional: BTreeMap<String, serde_json::Value>,
}

impl RssEntryState {
    pub fn hidden(&self) -> bool {
        self.decision == Some(RssDecision::Hide)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RssSchedule {
    pub iteration: u32,
    pub next_check: u64,
    pub paused: bool,
    pub forced: bool,
    pub filter_pending: bool,
    pub visit: u64,
    #[serde(flatten)]
    pub additional: BTreeMap<String, serde_json::Value>,
}

impl RssSchedule {
    pub fn visit(&mut self, now: u64) {
        self.iteration = 0;
        self.next_check = now;
        self.paused = false;
        self.forced = true;
        self.filter_pending = false;
        self.visit = self.visit.saturating_add(1);
    }
    pub fn allowed(&mut self, unread: usize) -> bool {
        if !self.forced && unread >= 99 {
            self.paused = true;
        }
        self.forced || !self.paused
    }
    pub fn finish(&mut self, now: u64, unread: usize) {
        let base = if unread > 0 { 2_f64 } else { 1.5_f64 };
        let delay =
            ((60.0 + base.powi(self.iteration.min(64) as i32)).min(86400.0) * 1000.0).ceil() as u64;
        self.next_check = now.saturating_add(delay);
        self.iteration = self.iteration.saturating_add(1);
        self.filter_pending = true;
    }
    pub fn finish_cycle(&mut self, unread: usize) {
        self.forced = false;
        self.filter_pending = false;
        self.allowed(unread);
    }
}

impl RssReadState {
    pub fn hidden(&self, id: &str) -> bool {
        self.entries.get(id).is_some_and(RssEntryState::hidden)
    }
    pub fn all_unread(&self, feed: &RssFeedCache) -> usize {
        feed.entries
            .iter()
            .filter(|entry| !self.read_entry_ids.contains(&entry.id))
            .count()
    }
}

pub fn content_version(entry: &RssEntry) -> String {
    digest_string(&serde_json::to_string(entry).expect("RSS entries serialize"))
}

impl RssEngine {
    pub fn preferences(&self, id: &ItemId) -> Result<RssPreferences, EngineError> {
        self.subscriptions
            .iter()
            .find(|s| &s.id == id && !s.deleted)
            .map(|s| s.preferences.clone())
            .ok_or(EngineError::Conflict)
    }

    pub fn save_preferences(
        &mut self,
        id: &ItemId,
        expected: u64,
        value: RssPreferences,
    ) -> Result<(), EngineError> {
        self.save_filter(id, expected, value, false)
    }

    pub fn save_filter(
        &mut self,
        id: &ItemId,
        expected: u64,
        mut value: RssPreferences,
        apply: bool,
    ) -> Result<(), EngineError> {
        let filter = value.compile()?;
        let _lock = self.operation_lock()?;
        let current = self.preferences(id)?;
        if current.version != expected {
            return Err(EngineError::Conflict);
        }
        value.version = current
            .version
            .checked_add(1)
            .ok_or(EngineError::Conflict)?;
        value.additional = current.additional;
        let version = value.version;
        let feed = self.load_cache(id)?;
        // Read state before writing preferences, so malformed state cannot cause a partial save.
        self.load_read_state(id)?;
        self.update_subscription(id, |s| s.preferences = value)?;
        self.update_state(id, |state| {
            if apply {
                filter.apply(&feed, state, version, RssFilterMode::All);
            } else {
                // Establish a baseline for cached entries without filter state.
                // Save alone must not turn them into new articles on the next refresh.
                for article in &feed.entries {
                    let entry = state.entries.entry(article.id.clone()).or_default();
                    if entry.content_version.is_empty() {
                        entry.content_version = content_version(article);
                    }
                }
            }
            Ok(())
        })
    }

    pub fn apply_filter(&self, id: &ItemId, mode: RssFilterMode) -> Result<(), EngineError> {
        let _lock = self.operation_lock()?;
        let preferences = self.preferences(id)?;
        if Self::open(&self.workspace)?.preferences(id)?.version != preferences.version {
            return Err(EngineError::Conflict);
        }
        let filter = preferences.compile()?;
        let feed = self.load_cache(id)?;
        self.update_state(id, |state| {
            filter.apply(&feed, state, preferences.version, mode);
            Ok(())
        })
    }

    pub fn update_state<T>(
        &self,
        id: &ItemId,
        update: impl FnOnce(&mut RssReadState) -> Result<T, EngineError>,
    ) -> Result<T, EngineError> {
        let _lock = self.operation_lock()?;
        self.preferences(id)?;
        let mut state = self.load_read_state(id)?;
        let expected = state.revision;
        let result = update(&mut state)?;
        if self.load_read_state(id)?.revision != expected {
            return Err(EngineError::Conflict);
        }
        state.revision = expected.checked_add(1).ok_or(EngineError::Conflict)?;
        write_json_atomic(&read_state_path(&self.workspace, id), &state)?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn article(id: &str, title: &str, summary: &str) -> RssEntry {
        RssEntry {
            id: id.into(),
            title: title.into(),
            summary: summary.into(),
            author: None,
            published: None,
            updated: None,
            link: None,
        }
    }
    fn preferences(blacklist: &str, whitelist: &str) -> RssPreferences {
        RssPreferences {
            blacklist: blacklist.into(),
            whitelist: whitelist.into(),
            ..Default::default()
        }
    }
    fn fixture() -> (tempfile::TempDir, RssEngine, ItemId) {
        let root = tempfile::tempdir().unwrap();
        let mut engine = RssEngine::open(root.path()).unwrap();
        let id = engine
            .create_subscription("https://example.test/feed", vec![], false, "now")
            .unwrap();
        engine
            .apply_refresh(RssRefreshResult::Fetched {
                item_id: id.clone(),
                cache: RssFeedCache {
                    entries: vec![
                        article("first", "Promotion", "Buy now"),
                        article("second", "Rust", "Sponsored news"),
                    ],
                    ..Default::default()
                },
            })
            .unwrap();
        (root, engine, id)
    }

    #[test]
    fn blacklist_then_whitelist_truth_table_and_empty_lists() {
        let entry = article("id", "Rust Promotion", "Useful news");
        for (blacklist, whitelist, expected) in [
            ("", "", RssDecision::Keep),
            ("", "Rust", RssDecision::Keep),
            ("unrelated", "", RssDecision::Keep),
            ("promotion", "", RssDecision::Hide),
            ("promotion", "unrelated", RssDecision::Hide),
            ("promotion", "rust", RssDecision::Keep),
            ("unrelated\nPROMOTION", "other\nuseful", RssDecision::Keep),
            (" \n\t\n", "", RssDecision::Keep),
        ] {
            assert_eq!(
                preferences(blacklist, whitelist)
                    .compile()
                    .unwrap()
                    .decision(&entry),
                expected
            );
        }
    }

    #[test]
    fn regexp_unicode_case_flags_boundaries_and_full_cached_text() {
        let entry = article(
            "id",
            "РЕКЛАМА Rust",
            &format!("{}\nUseful", "a".repeat(20_000)),
        );
        for (pattern, expected) in [
            (r"\bреклама\b", RssDecision::Hide),
            ("(?-i)rust", RssDecision::Keep),
            ("(?-i)Rust", RssDecision::Hide),
            ("USEFUL$", RssDecision::Hide),
            ("Rust\\na", RssDecision::Hide),
            ("^Useful", RssDecision::Keep),
            ("(?m)^Useful", RssDecision::Hide),
            (" Rust ", RssDecision::Keep),
        ] {
            assert_eq!(
                preferences(pattern, "").compile().unwrap().decision(&entry),
                expected,
                "{pattern}"
            );
        }
        assert_eq!(
            preferences("Реклама\r\nOther", "")
                .compile()
                .unwrap()
                .decision(&entry),
            RssDecision::Hide
        );
    }

    #[test]
    fn validation_reports_physical_line_and_bounds() {
        assert_eq!(
            preferences("\nvalid\n[", "").compile().err(),
            Some(RssFilterError::Invalid {
                blacklist: true,
                line: 3
            })
        );
        assert_eq!(
            preferences("", "ok\n(?=no)").compile().err(),
            Some(RssFilterError::Invalid {
                blacklist: false,
                line: 2
            })
        );
        assert_eq!(
            preferences(&"я".repeat(8193), "").compile().err(),
            Some(RssFilterError::TooLong)
        );
        assert!(
            preferences(&" ".repeat(MAX_PREFERENCE_BYTES), "")
                .validate()
                .is_ok()
        );
        assert!(preferences("a{100000000}", "").validate().is_err());
    }

    #[test]
    fn save_only_preserves_decisions_across_restart_and_refresh() {
        let (root, mut engine, id) = fixture();
        engine
            .save_preferences(&id, 0, preferences("promotion|sponsored", ""))
            .unwrap();
        let mut engine = RssEngine::open(root.path()).unwrap();
        let (mut feed, state) = engine.feed(&id).unwrap();
        assert!(!state.hidden("first") && !state.hidden("second"));
        engine
            .apply_refresh(RssRefreshResult::Fetched {
                item_id: id.clone(),
                cache: feed.clone(),
            })
            .unwrap();
        assert!(!engine.feed(&id).unwrap().1.hidden("first"));
        feed.entries[1].summary.push('!');
        feed.entries.push(article("third", "PROMOTION", "New"));
        engine.mark_read(&id, "second", "read").unwrap();
        engine
            .apply_refresh(RssRefreshResult::Fetched {
                item_id: id.clone(),
                cache: feed,
            })
            .unwrap();
        let (_, state) = engine.feed(&id).unwrap();
        assert!(!state.hidden("first"));
        assert!(state.hidden("second") && state.hidden("third"));
        assert!(state.read_entry_ids.contains("second"));
        engine
            .save_preferences(&id, 1, preferences("", ""))
            .unwrap();
        engine.apply_filter(&id, RssFilterMode::Changed).unwrap();
        assert!(engine.feed(&id).unwrap().1.hidden("second"));
    }

    #[test]
    fn http_304_recovers_changed_cache_without_reapplying_rules_to_unchanged_entries() {
        let (root, mut engine, id) = fixture();
        engine
            .save_preferences(&id, 0, preferences("promotion|sponsored", ""))
            .unwrap();
        let state_path = read_state_path(root.path(), &id);
        let before = fs::read(&state_path).unwrap();
        let (mut cache, _) = engine.feed(&id).unwrap();
        cache.entries[1].summary.push('!');
        engine
            .apply_refresh(RssRefreshResult::Fetched {
                item_id: id.clone(),
                cache,
            })
            .unwrap();
        // Reproduce a cache commit followed by failure before the state replacement.
        fs::write(&state_path, before).unwrap();
        let mut engine = RssEngine::open(root.path()).unwrap();
        engine
            .apply_refresh(RssRefreshResult::NotModified {
                item_id: id.clone(),
                fetched_at: "later".into(),
            })
            .unwrap();
        let (_, state) = engine.feed(&id).unwrap();
        assert!(!state.hidden("first"));
        assert!(state.hidden("second"));
    }

    #[test]
    fn full_apply_hides_and_unhides_read_articles_even_when_paused() {
        let (root, mut engine, id) = fixture();
        engine.mark_read(&id, "first", "read").unwrap();
        engine
            .update_state(&id, |s| {
                s.schedule.paused = true;
                Ok(())
            })
            .unwrap();
        engine
            .save_filter(&id, 0, preferences("promotion|sponsored", "rust"), true)
            .unwrap();
        let (_, state) = engine.feed(&id).unwrap();
        assert!(state.hidden("first"));
        assert!(!state.hidden("second"));
        assert!(state.schedule.paused);
        assert_eq!(engine.summaries()[0].unread, 1);
        let mut engine = RssEngine::open(root.path()).unwrap();
        assert!(engine.feed(&id).unwrap().1.hidden("first"));
        engine
            .save_filter(&id, 1, preferences("", ""), true)
            .unwrap();
        let (_, after) = engine.feed(&id).unwrap();
        assert!(!after.hidden("first"));
        assert_eq!(state.read_entry_ids, after.read_entry_ids);
        assert_eq!(state.last_read_at, after.last_read_at);
    }

    #[test]
    fn full_filter_has_no_old_ai_visit_or_batch_limit() {
        let (_root, mut engine, id) = fixture();
        engine
            .apply_refresh(RssRefreshResult::Fetched {
                item_id: id.clone(),
                cache: RssFeedCache {
                    entries: (0..150)
                        .map(|i| article(&format!("entry/{i}"), "Promotion", ""))
                        .collect(),
                    ..Default::default()
                },
            })
            .unwrap();
        engine
            .save_filter(&id, 0, preferences("promotion", ""), true)
            .unwrap();
        let (feed, state) = engine.feed(&id).unwrap();
        assert_eq!(feed.entries.len(), 150);
        assert!(feed.entries.iter().all(|e| state.hidden(&e.id)));
    }

    #[test]
    fn invalid_rules_and_conflicts_do_not_change_files() {
        let (root, mut engine, id) = fixture();
        let config = fs::read(config_path(root.path())).unwrap();
        let state = fs::read(read_state_path(root.path(), &id)).unwrap();
        assert!(
            engine
                .save_filter(&id, 0, preferences("[", ""), true)
                .is_err()
        );
        assert!(
            engine
                .save_filter(&id, 2, preferences("rust", ""), true)
                .is_err()
        );
        assert_eq!(fs::read(config_path(root.path())).unwrap(), config);
        assert_eq!(fs::read(read_state_path(root.path(), &id)).unwrap(), state);
        let mut stale = RssEngine::open(root.path()).unwrap();
        engine
            .save_preferences(&id, 0, preferences("new", ""))
            .unwrap();
        assert!(
            stale
                .save_filter(&id, 0, preferences("old", ""), true)
                .is_err()
        );
        assert!(stale.apply_filter(&id, RssFilterMode::All).is_err());
        assert_eq!(
            RssEngine::open(root.path())
                .unwrap()
                .preferences(&id)
                .unwrap()
                .blacklist,
            "new"
        );
    }

    #[test]
    fn unknown_fields_survive_and_open_does_not_migrate() {
        let (root, _, id) = fixture();
        let config_path = config_path(root.path());
        let state_path = read_state_path(root.path(), &id);
        let mut config: serde_json::Value =
            serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
        config["future"] = serde_json::json!(true);
        config["subscriptions"][0]["preferences"]["future"] = serde_json::json!(42);
        let mut state: serde_json::Value =
            serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        state["entries"]["first"]["reaction"] = serde_json::json!("dislike");
        state["entries"]["first"]["future"] = serde_json::json!(43);
        state["future"] = serde_json::json!(44);
        fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
        fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
        let before = (
            fs::read(&config_path).unwrap(),
            fs::read(&state_path).unwrap(),
        );
        let mut engine = RssEngine::open(root.path()).unwrap();
        assert!(!engine.feed(&id).unwrap().1.hidden("first"));
        assert_eq!(
            before,
            (
                fs::read(&config_path).unwrap(),
                fs::read(&state_path).unwrap()
            )
        );
        engine
            .save_filter(&id, 0, preferences("promotion", ""), true)
            .unwrap();
        let config: serde_json::Value =
            serde_json::from_slice(&fs::read(config_path).unwrap()).unwrap();
        let state: serde_json::Value =
            serde_json::from_slice(&fs::read(state_path).unwrap()).unwrap();
        assert_eq!(config["future"], true);
        assert_eq!(config["subscriptions"][0]["preferences"]["future"], 42);
        assert_eq!(state["entries"]["first"]["future"], 43);
        assert_eq!(state["future"], 44);
    }

    #[test]
    fn schedule_formulas_pause_and_forced_cycle_survive_restart() {
        for unread in [0, 1] {
            let mut schedule = RssSchedule::default();
            for i in 0..80 {
                let base = if unread == 0 { 1.5_f64 } else { 2_f64 };
                schedule.finish(1_000_000, unread);
                assert_eq!(
                    schedule.next_check,
                    1_000_000 + ((60.0 + base.powi(i)).min(86400.0) * 1000.0).ceil() as u64
                );
                schedule = serde_json::from_slice(&serde_json::to_vec(&schedule).unwrap()).unwrap();
            }
        }
        for unread in [98, 99, 100] {
            let mut schedule = RssSchedule::default();
            assert_eq!(schedule.allowed(unread), unread < 99);
            schedule.visit(10);
            assert!(schedule.allowed(unread));
            assert_eq!(schedule.iteration, 0);
            assert_eq!(schedule.next_check, 10);
            schedule.finish(20, unread);
            let restored: RssSchedule =
                serde_json::from_value(serde_json::to_value(&schedule).unwrap()).unwrap();
            assert!(restored.filter_pending);
            schedule.finish_cycle(unread);
            assert!(!schedule.filter_pending);
            assert_eq!(schedule.allowed(unread), unread < 99);
        }
    }
}
