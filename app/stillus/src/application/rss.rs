// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

//! One coordinator per workspace session. Only workers touch network/save files.
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{
    Arc, Mutex,
    mpsc::{self, Receiver, SyncSender},
};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use stillus_core::{ItemId, RssEngine, RssPreferences, RssRefreshResult, execute_rss_refresh};
use stillus_engine::EngineError;

static RSS_REQUESTS: AtomicUsize = AtomicUsize::new(0);

struct RequestSlot(&'static AtomicUsize);
impl RequestSlot {
    fn take(counter: &'static AtomicUsize, limit: usize) -> Option<Self> {
        counter
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                (used < limit).then_some(used + 1)
            })
            .ok()
            .map(|_| Self(counter))
    }
}
impl Drop for RequestSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(crate) enum Command {
    Addressed(
        Addressed,
        SyncSender<Result<AddressedResult, super::actions::ActionError>>,
    ),
    Visit(ItemId),
    Refresh(ItemId, SyncSender<Result<(), super::actions::ActionError>>),
    Preferences(ItemId, u64, u64, RssPreferences, bool),
    Read(ItemId, String, String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Status {
    Idle,
    Paused,
    Retry,
    Conflict,
    Saved,
}

pub(crate) struct Snapshot {
    pub engine: RssEngine,
    pub refreshing: BTreeSet<String>,
    pub status: BTreeMap<String, Status>,
    pub saves: BTreeMap<String, (u64, bool)>,
    pub errors: BTreeMap<String, EngineError>,
    /// Monotonic per-subscription fetch completions survive coalesced snapshots.
    pub completed: BTreeMap<String, u64>,
}

pub(crate) type Executor = Arc<
    dyn Fn(stillus_core::RssRefreshRequest) -> Result<RssRefreshResult, EngineError> + Send + Sync,
>;

pub(crate) struct Service {
    pub sender: SyncSender<Command>,
    pub receiver: Receiver<Snapshot>,
    alive: Arc<Mutex<bool>>,
}

impl Drop for Service {
    fn drop(&mut self) {
        *self.alive.lock().expect("RSS session gate") = false;
    }
}

enum Completion {
    Fetch(ItemId, u64, Result<RssRefreshResult, EngineError>),
}

impl Service {
    pub fn start(root: PathBuf) -> Self {
        Self::with_executor(root, Arc::new(execute_rss_refresh))
    }
    pub(crate) fn with_executor(root: PathBuf, executor: Executor) -> Self {
        let (sender, commands) = mpsc::sync_channel(64);
        let (snapshots, receiver) = mpsc::sync_channel(1);
        let alive = Arc::new(Mutex::new(true));
        let gate = alive.clone();
        thread::spawn(move || run_with_executor(root, commands, snapshots, gate, executor));
        Self {
            sender,
            receiver,
            alive,
        }
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

struct Coordinator {
    root: PathBuf,
    fetching: BTreeSet<String>,
    cycles: BTreeSet<String>,
    replies: BTreeMap<String, SyncSender<Result<(), super::actions::ActionError>>>,
    status: BTreeMap<String, Status>,
    saves: BTreeMap<String, (u64, bool)>,
    errors: BTreeMap<String, EngineError>,
    completed: BTreeMap<String, u64>,
}

#[cfg(test)]
fn run(
    root: PathBuf,
    commands: Receiver<Command>,
    snapshots: SyncSender<Snapshot>,
    alive: Arc<Mutex<bool>>,
) {
    run_with_executor(
        root,
        commands,
        snapshots,
        alive,
        Arc::new(execute_rss_refresh),
    );
}
fn run_with_executor(
    root: PathBuf,
    commands: Receiver<Command>,
    snapshots: SyncSender<Snapshot>,
    alive: Arc<Mutex<bool>>,
    executor: Executor,
) {
    let (results, completions) = mpsc::sync_channel(3);
    let mut coordinator = Coordinator {
        root,
        fetching: BTreeSet::new(),
        cycles: BTreeSet::new(),
        replies: BTreeMap::new(),
        status: BTreeMap::new(),
        saves: BTreeMap::new(),
        errors: BTreeMap::new(),
        completed: BTreeMap::new(),
    };
    let mut dirty = true;
    let mut last_tick = 0;
    loop {
        let command = match commands.recv_timeout(Duration::from_millis(100)) {
            Ok(c) => Some(c),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(_) => break,
        };
        // Cancellation and durable writes share a gate. A disposed session cannot apply results.
        let active = alive.lock().expect("RSS session gate");
        if !*active {
            break;
        }
        let Ok(mut engine) = RssEngine::open(&coordinator.root) else {
            if let Some(command) = command {
                coordinator.reject(command);
            }
            continue;
        };
        let Ok(_lock) = engine.operation_lock() else {
            // Do not lose a save token when the workspace becomes unwritable.
            if let Some(command) = command {
                coordinator.reject(command);
                dirty = true;
            }
            if dirty && snapshots.try_send(coordinator.snapshot(engine)).is_ok() {
                dirty = false;
            }
            continue;
        };
        let Ok(fresh) = RssEngine::open(&coordinator.root) else {
            if let Some(command) = command {
                coordinator.reject(command);
            }
            continue;
        };
        engine = fresh;
        if let Some(command) = command {
            dirty = true;
            coordinator.command(&mut engine, command);
        }
        // Drain only the bounded command capacity before applying refresh results.
        for _ in 0..64 {
            let Ok(command) = commands.try_recv() else {
                break;
            };
            dirty = true;
            coordinator.command(&mut engine, command);
        }
        while let Ok(result) = completions.try_recv() {
            dirty = true;
            coordinator.complete(&mut engine, result);
        }
        if last_tick != now() / 1000 || dirty {
            last_tick = now() / 1000;
            dirty |= coordinator.dispatch(&mut engine, &results, &executor);
        }
        if dirty && snapshots.try_send(coordinator.snapshot(engine)).is_ok() {
            dirty = false;
        }
    }
}

impl Coordinator {
    fn reply(&mut self, id: &ItemId, result: Result<(), super::actions::ActionError>) {
        if let Some(reply) = self.replies.remove(id.as_str()) {
            let _ = reply.send(result);
        }
    }
    fn snapshot(&self, engine: RssEngine) -> Snapshot {
        Snapshot {
            engine,
            refreshing: self.fetching.clone(),
            status: self.status.clone(),
            saves: self.saves.clone(),
            errors: self.errors.clone(),
            completed: self.completed.clone(),
        }
    }

    fn reject(&mut self, command: Command) {
        let id = match command {
            Command::Addressed(_, reply) => {
                let _ = reply.send(Err(super::actions::ActionError::Conflict));
                return;
            }
            Command::Preferences(id, token, ..) => {
                self.saves.insert(id.as_str().into(), (token, false));
                id
            }
            Command::Refresh(id, reply) => {
                let _ = reply.send(Err(super::actions::ActionError::Conflict));
                id
            }
            Command::Visit(id) | Command::Read(id, ..) => id,
        };
        self.status.insert(id.as_str().into(), Status::Conflict);
    }

    fn command(&mut self, engine: &mut RssEngine, command: Command) {
        let command = match command {
            Command::Addressed(action, reply) => {
                let _ = reply.send(addressed(engine, action));
                return;
            }
            command => command,
        };
        let id = match &command {
            Command::Addressed(..) => unreachable!("addressed request handled above"),
            Command::Visit(id)
            | Command::Refresh(id, ..)
            | Command::Preferences(id, ..)
            | Command::Read(id, ..) => id.clone(),
        };
        let command = match command {
            Command::Refresh(_, reply) => {
                if self.replies.contains_key(id.as_str()) || self.replies.len() >= 64 {
                    let _ = reply.send(Err(super::actions::ActionError::Busy));
                    return;
                }
                if !engine
                    .subscriptions()
                    .iter()
                    .any(|item| item.id == id && !item.deleted)
                {
                    let _ = reply.send(Err(super::actions::ActionError::NotFound));
                    return;
                }
                self.replies.insert(id.as_str().into(), reply);
                Command::Visit(id.clone())
            }
            command => command,
        };
        let result = match command {
            Command::Addressed(..) => unreachable!("addressed request handled above"),
            Command::Refresh(..) => unreachable!("refresh is normalized above"),
            Command::Visit(_) => {
                self.cycles.remove(id.as_str());
                engine.update_state(&id, |s| {
                    s.schedule.visit(now());
                    Ok(())
                })
            }
            Command::Preferences(_, token, expected, preferences, apply) => {
                #[cfg(feature = "test-utils")]
                if std::env::var_os("STILLUS_TEST_RSS_SAVE_DELAY").is_some() {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                }
                let result = engine.save_filter(&id, expected, preferences, apply);
                self.saves
                    .insert(id.as_str().into(), (token, result.is_ok()));
                result
            }
            Command::Read(_, entry, timestamp) => {
                engine.mark_read(&id, &entry, &timestamp).map(|_| ())
            }
        };
        if result.is_err() {
            self.reply(&id, Err(super::actions::ActionError::Conflict));
        }
        self.status.insert(
            id.as_str().into(),
            if result.is_ok() {
                Status::Saved
            } else {
                Status::Conflict
            },
        );
    }

    fn complete(&mut self, engine: &mut RssEngine, result: Completion) {
        match result {
            Completion::Fetch(id, visit, result) => {
                self.fetching.remove(id.as_str());
                let completed = self.completed.entry(id.as_str().into()).or_default();
                *completed = completed.saturating_add(1);
                if engine.preferences(&id).is_err() {
                    self.reply(&id, Err(super::actions::ActionError::NotFound));
                    return;
                }
                let success = result.is_ok();
                let fetch_error = result.as_ref().err().cloned();
                let apply = result.map_or(Ok(()), |result| engine.apply_refresh(result));
                let Ok((feed, state)) = engine.feed(&id) else {
                    self.reply(&id, Err(super::actions::ActionError::NotFound));
                    return;
                };
                let unread = state.all_unread(&feed);
                let saved = engine.update_state(&id, |s| {
                    // An explicit visit during an in-flight fetch reuses that fetch.
                    if s.schedule.visit == visit || s.schedule.forced {
                        s.schedule.finish(now(), unread);
                    }
                    Ok(())
                });
                let failure = apply
                    .as_ref()
                    .err()
                    .cloned()
                    .or_else(|| saved.as_ref().err().cloned())
                    .or(fetch_error);
                if let Some(error) = &failure {
                    self.errors.insert(id.as_str().into(), error.clone());
                } else {
                    self.errors.remove(id.as_str());
                }
                self.reply(
                    &id,
                    if apply.is_err() || saved.is_err() {
                        Err(super::actions::ActionError::Conflict)
                    } else if success {
                        Ok(())
                    } else {
                        Err(super::actions::ActionError::Failed(
                            failure
                                .expect("failed refresh retains its cause")
                                .to_string(),
                        ))
                    },
                );
                self.cycles.insert(id.as_str().into());
                self.status.insert(
                    id.as_str().into(),
                    if apply.is_err() || saved.is_err() {
                        Status::Conflict
                    } else if success {
                        Status::Idle
                    } else {
                        Status::Retry
                    },
                );
            }
        }
    }

    fn dispatch(
        &mut self,
        engine: &mut RssEngine,
        results: &SyncSender<Completion>,
        executor: &Executor,
    ) -> bool {
        let mut changed = false;
        let ids = engine
            .subscriptions()
            .iter()
            .filter(|s| !s.deleted)
            .map(|s| s.id.clone())
            .collect::<Vec<_>>();
        self.errors
            .retain(|key, _| ids.iter().any(|id| id.as_str() == key));
        self.completed
            .retain(|key, _| ids.iter().any(|id| id.as_str() == key));
        for id in &ids {
            if engine
                .feed(id)
                .is_ok_and(|(_, state)| state.schedule.filter_pending)
            {
                self.cycles.insert(id.as_str().into());
            }
        }
        for id in &ids {
            if self.fetching.len() >= 2 {
                break;
            }
            if self.fetching.contains(id.as_str()) {
                continue;
            }
            let Ok((feed, state)) = engine.feed(id) else {
                continue;
            };
            // Finish an earlier cycle before starting its next download.
            if self.cycles.contains(id.as_str()) && state.schedule.iteration != 0 {
                continue;
            }
            if state.schedule.next_check > now() {
                continue;
            }
            let mut schedule = state.schedule.clone();
            if !schedule.allowed(state.all_unread(&feed)) {
                if schedule != state.schedule {
                    changed = true;
                    if engine
                        .update_state(id, |s| {
                            s.schedule = schedule;
                            Ok(())
                        })
                        .is_err()
                    {
                        self.status.insert(id.as_str().into(), Status::Conflict);
                    } else {
                        self.status.insert(id.as_str().into(), Status::Paused);
                    }
                }
                continue;
            }
            let Ok(request) = engine.refresh_request(id) else {
                continue;
            };
            let Some(slot) = RequestSlot::take(&RSS_REQUESTS, 2) else {
                break;
            };
            self.fetching.insert(id.as_str().into());
            changed = true;
            let results = results.clone();
            let id = id.clone();
            let visit = state.schedule.visit;
            let executor = executor.clone();
            thread::spawn(move || {
                let _slot = slot;
                let result = executor(request);
                let _ = results.send(Completion::Fetch(id, visit, result));
            });
        }
        // Release completed download cycles, including HTTP 304 and errors.
        let finished = self
            .cycles
            .iter()
            .filter(|id| !self.fetching.contains(*id))
            .cloned()
            .collect::<Vec<_>>();
        for key in finished {
            let Some(id) = ids.iter().find(|id| id.as_str() == key) else {
                self.cycles.remove(&key);
                continue;
            };
            if let Ok((feed, state)) = engine.feed(id) {
                let unread = state.all_unread(&feed);
                if engine
                    .update_state(id, |s| {
                        s.schedule.finish_cycle(unread);
                        Ok(())
                    })
                    .is_err()
                {
                    self.status.insert(key.clone(), Status::Conflict);
                }
            }
            self.cycles.remove(&key);
            changed = true;
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancelled_session_does_not_write_queued_preferences() {
        let root = std::env::temp_dir().join(format!("rss-cancelled-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut engine = RssEngine::open(&root).unwrap();
        let id = engine
            .create_subscription("https://example.test/feed", vec![], false, "now")
            .unwrap();
        let before = engine.feed(&id).unwrap().1;
        let (sender, commands) = mpsc::sync_channel(1);
        sender
            .send(Command::Preferences(
                id.clone(),
                1,
                0,
                RssPreferences {
                    blacklist: "promotion".into(),
                    ..Default::default()
                },
                true,
            ))
            .unwrap();
        let (snapshots, receiver) = mpsc::sync_channel(1);
        run(
            root.clone(),
            commands,
            snapshots,
            Arc::new(Mutex::new(false)),
        );
        let engine = RssEngine::open(&root).unwrap();
        assert_eq!(engine.preferences(&id).unwrap(), RssPreferences::default());
        assert_eq!(engine.feed(&id).unwrap().1, before);
        assert!(receiver.try_recv().is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn save_conflicts_are_acknowledged_without_scheduling_a_download() {
        let root = std::env::temp_dir().join(format!("rss-save-conflict-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut engine = RssEngine::open(&root).unwrap();
        let id = engine
            .create_subscription("https://example.test/feed", vec![], false, "now")
            .unwrap();
        let mut coordinator = Coordinator {
            root: root.clone(),
            fetching: BTreeSet::new(),
            cycles: BTreeSet::new(),
            replies: BTreeMap::new(),
            status: BTreeMap::new(),
            saves: BTreeMap::new(),
            errors: BTreeMap::new(),
            completed: BTreeMap::new(),
        };
        coordinator.command(
            &mut engine,
            Command::Preferences(id.clone(), 1, 0, RssPreferences::default(), false),
        );
        assert_eq!(coordinator.saves[id.as_str()], (1, true));
        coordinator.command(
            &mut engine,
            Command::Preferences(id.clone(), 2, 0, RssPreferences::default(), true),
        );
        assert_eq!(coordinator.saves[id.as_str()], (2, false));
        assert_eq!(coordinator.status[id.as_str()], Status::Conflict);
        assert!(coordinator.cycles.is_empty() && coordinator.fetching.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unwritable_workspace_reports_save_failure_instead_of_losing_token() {
        let root = std::env::temp_dir().join(format!("rss-unwritable-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut engine = RssEngine::open(&root).unwrap();
        let id = engine
            .create_subscription("https://example.test/feed", vec![], false, "now")
            .unwrap();
        let directory = root.join(".stillus/engines/rss");
        std::fs::rename(&directory, root.join("saved_rss")).unwrap();
        std::fs::write(directory, "blocks directory creation").unwrap();
        let service = Service::start(root.clone());
        service
            .sender
            .send(Command::Preferences(
                id.clone(),
                7,
                0,
                RssPreferences::default(),
                true,
            ))
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            assert!(std::time::Instant::now() < deadline);
            let snapshot = service
                .receiver
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
            if snapshot.saves.get(id.as_str()) == Some(&(7, false)) {
                assert_eq!(snapshot.status[id.as_str()], Status::Conflict);
                break;
            }
        }
        drop(service);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn request_slots_survive_session_replacement_and_release_on_drop() {
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        let first = RequestSlot::take(&COUNT, 2).unwrap();
        let second = RequestSlot::take(&COUNT, 2).unwrap();
        assert!(RequestSlot::take(&COUNT, 2).is_none());
        drop(first);
        assert!(RequestSlot::take(&COUNT, 2).is_some());
        drop(second);
        assert_eq!(COUNT.load(Ordering::Acquire), 0);
    }
    #[test]
    fn http_304_and_errors_advance_once_and_visit_reuses_inflight_attempt() {
        let root = std::env::temp_dir().join(format!("rss-coordinator-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut engine = RssEngine::open(&root).unwrap();
        let id = engine
            .create_subscription("https://example.test/feed", vec![], false, "now")
            .unwrap();
        let mut c = Coordinator {
            root: root.clone(),
            fetching: BTreeSet::new(),
            cycles: BTreeSet::new(),
            replies: BTreeMap::new(),
            status: BTreeMap::new(),
            saves: BTreeMap::new(),
            errors: BTreeMap::new(),
            completed: BTreeMap::new(),
        };
        c.command(&mut engine, Command::Visit(id.clone()));
        c.fetching.insert(id.as_str().into());
        c.command(&mut engine, Command::Visit(id.clone()));
        assert_eq!(c.fetching.len(), 1);
        c.complete(
            &mut engine,
            Completion::Fetch(
                id.clone(),
                1,
                Ok(RssRefreshResult::NotModified {
                    item_id: id.clone(),
                    fetched_at: "now".into(),
                }),
            ),
        );
        assert_eq!(engine.feed(&id).unwrap().1.schedule.iteration, 1);
        assert!(c.fetching.is_empty());
        assert_eq!(c.completed[id.as_str()], 1);
        let cause = EngineError::Io("feed connection timed out".into());
        c.complete(
            &mut engine,
            Completion::Fetch(id.clone(), 2, Err(cause.clone())),
        );
        assert_eq!(engine.feed(&id).unwrap().1.schedule.iteration, 2);
        assert_eq!(c.status[id.as_str()], Status::Retry);
        assert_eq!(
            c.snapshot(RssEngine::open(&root).unwrap()).errors[id.as_str()],
            cause
        );
        c.command(&mut engine, Command::Visit(id.clone()));
        assert_eq!(c.errors[id.as_str()], cause);
        // Reading only this snapshot still observes both completions even if
        // the initial refreshing state was coalesced by the bounded channel.
        assert_eq!(
            c.snapshot(RssEngine::open(&root).unwrap()).completed[id.as_str()],
            2
        );
        c.complete(
            &mut engine,
            Completion::Fetch(
                id.clone(),
                3,
                Ok(RssRefreshResult::NotModified {
                    item_id: id.clone(),
                    fetched_at: "later".into(),
                }),
            ),
        );
        assert_eq!(c.status[id.as_str()], Status::Idle);
        assert!(!c.errors.contains_key(id.as_str()));
        assert_eq!(c.completed[id.as_str()], 3);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FilterSettings {
    pub blacklist: String,
    pub whitelist: String,
    #[serde(default)]
    pub version: u64,
}
impl From<RssPreferences> for FilterSettings {
    fn from(value: RssPreferences) -> Self {
        Self {
            blacklist: value.blacklist,
            whitelist: value.whitelist,
            version: value.version,
        }
    }
}

/// Addressed requests executed against a fresh engine under its operation lock.
pub(crate) enum Addressed {
    Create {
        url: String,
        categories: Vec<String>,
        favorited: bool,
    },
    Metadata {
        id: ItemId,
        version: u64,
        patch: stillus_engine::CommonMetadataPatch,
    },
    Read {
        id: ItemId,
        offset: usize,
        limit: usize,
    },
    MarkRead {
        id: ItemId,
        version: u64,
        entry: String,
    },
    Filters {
        id: ItemId,
        version: u64,
        preferences: FilterSettings,
        apply: bool,
    },
}
#[derive(Clone, serde::Serialize)]
#[serde(untagged)]
pub(crate) enum AddressedResult {
    Created {
        id: ItemId,
        saved: bool,
    },
    Saved {
        saved: bool,
    },
    Feed {
        id: ItemId,
        version: u64,
        metadata_version: u64,
        preferences: FilterSettings,
        entries: Vec<stillus_core::RssEntry>,
        read: Vec<String>,
    },
}
fn addressed(
    engine: &mut RssEngine,
    action: Addressed,
) -> Result<AddressedResult, super::actions::ActionError> {
    use super::actions::ActionError;
    let timestamp = stillus_core::format_utc_timestamp(SystemTime::now())?;
    match action {
        Addressed::Create {
            url,
            categories,
            favorited,
        } => {
            let categories = stillus_core::normalize_rss_categories(&categories)
                .map_err(|_| ActionError::InvalidArguments)?;
            let id = engine
                .create_subscription(&url, categories, favorited, &timestamp)
                .map_err(map_engine_error)?;
            Ok(AddressedResult::Created { id, saved: true })
        }
        Addressed::Metadata { id, version, patch } => {
            stillus_core::apply_rss_metadata(engine, &id, version, patch, &timestamp).map_err(
                |error| match error {
                    stillus_core::CoreError::Engine(error) => map_engine_error(error),
                    _ => ActionError::InvalidArguments,
                },
            )?;
            Ok(AddressedResult::Saved { saved: true })
        }
        Addressed::Read { id, offset, limit } => {
            let subscription = engine
                .subscriptions()
                .iter()
                .find(|item| item.id == id)
                .ok_or(ActionError::NotFound)?;
            let metadata_version = subscription.revision;
            let preferences = subscription.preferences.clone();
            let (feed, state) = engine.feed(&id).map_err(map_engine_error)?;
            Ok(AddressedResult::Feed {
                id,
                version: state.revision,
                metadata_version,
                preferences: preferences.into(),
                entries: feed
                    .entries
                    .into_iter()
                    .skip(offset)
                    .take(limit.min(100))
                    .collect(),
                read: state.read_entry_ids.into_iter().take(100).collect(),
            })
        }
        Addressed::MarkRead { id, version, entry } => {
            let (_, state) = engine.feed(&id).map_err(map_engine_error)?;
            if state.revision != version {
                return Err(ActionError::Conflict);
            }
            engine
                .mark_read(&id, &entry, &timestamp)
                .map_err(map_engine_error)?;
            Ok(AddressedResult::Saved { saved: true })
        }
        Addressed::Filters {
            id,
            version,
            preferences,
            apply,
        } => {
            let mut current = engine.preferences(&id).map_err(map_engine_error)?;
            current.blacklist = preferences.blacklist;
            current.whitelist = preferences.whitelist;
            engine
                .save_filter(&id, version, current, apply)
                .map_err(map_engine_error)?;
            Ok(AddressedResult::Saved { saved: true })
        }
    }
}
fn map_engine_error(error: stillus_engine::EngineError) -> super::actions::ActionError {
    match error {
        stillus_engine::EngineError::Conflict => super::actions::ActionError::Conflict,
        stillus_engine::EngineError::InvalidSetting(_) => {
            super::actions::ActionError::InvalidArguments
        }
        stillus_engine::EngineError::Unsupported(_) => {
            super::actions::ActionError::RequiresUserInteraction
        }
        _ => super::actions::ActionError::NotFound,
    }
}
