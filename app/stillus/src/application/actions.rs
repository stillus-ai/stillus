// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

//! Typed application commands and their direct-call adapter. No model or transport runtime.
use super::workspace::Workspace;
use serde::Serialize;
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
    mpsc,
};
use stillus_core::{
    AddressedEdit, CoreError, NoteEdit, NoteMetadataEdit, NoteProtection, format_utc_timestamp,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) enum ActionError {
    Conflict,
    SessionChanged,
    Busy,
    Cancelled,
    NotFound,
    InvalidArguments,
    RequiresUserInteraction,
    Failed(String),
}

impl From<CoreError> for ActionError {
    fn from(error: CoreError) -> Self {
        match error {
            CoreError::Save(stillus_storage::SaveError::Conflict) => Self::Conflict,
            CoreError::UnsavedChanges => Self::Busy,
            CoreError::MasterPasswordRequired => Self::RequiresUserInteraction,
            CoreError::NoteUnavailable(_) => Self::NotFound,
            CoreError::Secure(_) | CoreError::Security(_) | CoreError::PasswordChange(_) => {
                Self::RequiresUserInteraction
            }
            error => Self::Failed(error.to_string()),
        }
    }
}

pub(crate) enum Action {
    List {
        offset: usize,
        limit: usize,
    },
    Read {
        id: String,
        offset: usize,
        limit: usize,
    },
    Edit {
        id: String,
        version: String,
        edit: NoteEdit,
    },
    Create {
        title: String,
    },
    Restore {
        id: String,
        version: String,
    },
    Rename {
        id: String,
        version: String,
        title: String,
    },
    Metadata {
        id: String,
        version: String,
        edit: NoteMetadataEdit,
    },
    Open {
        id: String,
    },
    Search {
        query: String,
    },
    RssList,
    RssRefresh {
        id: String,
    },
    Status {
        id: String,
    },
    Cancel {
        id: String,
    },
    UserInteraction,
}

#[derive(Clone, Serialize)]
pub(crate) enum OperationStatus {
    Pending,
    Running,
    Cancelled,
    Saved(OperationOutput),
    Completed(OperationOutput),
    Failed(ActionError),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct OperationProgress {
    pub phase: String,
    pub completed: u64,
    pub total: Option<u64>,
}
impl Default for OperationProgress {
    fn default() -> Self {
        Self {
            phase: "running".into(),
            completed: 0,
            total: None,
        }
    }
}
struct Operation {
    state: OperationStatus,
    progress: OperationProgress,
    gate: Arc<AtomicU8>,
    writes: bool,
}
struct Completion {
    operation: String,
    started: bool,
    effect: Option<Result<OperationOutput, ActionError>>,
    path: PathBuf,
    result: Result<PathBuf, ActionError>,
    search: Option<Result<Vec<stillus_search::SearchResult>, ActionError>>,
}

pub(crate) struct Operations {
    next: u64,
    entries: BTreeMap<String, Operation>,
    completed: VecDeque<String>,
    searching: Option<String>,
    sender: mpsc::SyncSender<Completion>,
    receiver: mpsc::Receiver<Completion>,
}
impl Default for Operations {
    fn default() -> Self {
        let (sender, receiver) = mpsc::sync_channel(4);
        Self {
            next: 0,
            entries: BTreeMap::new(),
            completed: VecDeque::new(),
            searching: None,
            sender,
            receiver,
        }
    }
}
impl Operations {
    pub(crate) fn writing(&self) -> bool {
        self.entries.values().any(|op| {
            op.writes
                && matches!(
                    op.state,
                    OperationStatus::Pending | OperationStatus::Running
                )
        })
    }
    pub(crate) fn busy(&self) -> bool {
        self.entries.values().any(|op| {
            matches!(
                op.state,
                OperationStatus::Pending | OperationStatus::Running
            )
        })
    }
}

impl Workspace {
    pub(crate) fn poll_actions(&mut self) -> bool {
        let mut changed = std::mem::take(&mut self.actions_dirty);
        while let Ok(completion) = self.operations.receiver.try_recv() {
            if completion.started {
                if let Some(operation) = self.operations.entries.get_mut(&completion.operation) {
                    if !matches!(operation.state, OperationStatus::Cancelled) {
                        operation.state = OperationStatus::Running;
                        operation.progress.phase = "running".into();
                        changed = true;
                    }
                }
                continue;
            }
            if let Some(result) = completion.effect {
                self.operations.finish(&completion.operation, result);
                changed = true;
                continue;
            }
            let status = if let Some(result) = completion.search {
                match result {
                    Ok(hits) => {
                        let results = hits
                            .into_iter()
                            .filter_map(|hit| {
                                let path = self.root().join(&hit.relative_path);
                                let id = self.target_id(&path).ok()?;
                                Some(SearchItem {
                                    id,
                                    path: hit.relative_path,
                                    title: hit.title,
                                })
                            })
                            .collect();
                        OperationStatus::Completed(OperationOutput::Search { results })
                    }
                    Err(error) => OperationStatus::Failed(error),
                }
            } else {
                match completion.result {
                    Ok(path) => {
                        // The disk commit and its subsequent catalogue reconciliation have distinct outcomes.
                        let reconcile_error = self
                            .core
                            .finish_note_edit(&completion.path, &path)
                            .err()
                            .map(ActionError::from);
                        self.remap(&completion.path, &path);
                        changed = true;
                        OperationStatus::Saved(OperationOutput::Saved {
                            saved: true,
                            reconcile_error,
                        })
                    }
                    Err(ActionError::Cancelled) => OperationStatus::Cancelled,
                    Err(error) => OperationStatus::Failed(error),
                }
            };
            self.operations.complete(&completion.operation, status);
        }
        changed
    }

    fn schedule_file_job(
        &mut self,
        id: &str,
        job: impl FnOnce() -> Result<PathBuf, CoreError> + Send + 'static,
    ) -> Result<ActionResult, ActionError> {
        if !self.operations.has_capacity() || self.operations.writing() {
            return Err(ActionError::Busy);
        }
        self.operations.next += 1;
        let operation = format!(
            "operations/{:x}/{:016x}",
            self.session_id(),
            self.operations.next
        );
        let gate = Arc::new(AtomicU8::new(0));
        self.operations.entries.insert(
            operation.clone(),
            Operation {
                progress: OperationProgress {
                    phase: "queued".into(),
                    ..Default::default()
                },
                state: OperationStatus::Pending,
                gate: gate.clone(),
                writes: true,
            },
        );
        self.operations.trim();
        let sender = self.operations.sender.clone();
        let operation_id = operation.clone();
        let path = self.resolve_target(id)?;
        std::thread::spawn(move || {
            if gate
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return;
            }
            let _ = sender.send(Completion {
                operation: operation_id.clone(),
                started: true,
                effect: None,
                path: PathBuf::new(),
                result: Err(ActionError::Cancelled),
                search: None,
            });
            let result = job().map_err(ActionError::from);
            let _ = sender.send(Completion {
                operation: operation_id,
                started: false,
                effect: None,
                path,
                result,
                search: None,
            });
        });
        Ok(ActionResult::Accepted {
            operation,
            saved: false,
        })
    }

    pub(crate) fn execute_action(
        &mut self,
        action: Action,
        now_ms: u64,
    ) -> Result<ActionResult, ActionError> {
        let changed = self.poll_actions();
        self.actions_dirty |= changed;
        let mutates = matches!(
            &action,
            Action::Edit { .. }
                | Action::Create { .. }
                | Action::Restore { .. }
                | Action::Rename { .. }
                | Action::Metadata { .. }
                | Action::Open { .. }
        );
        let result = match action {
            Action::List { offset, limit } => {
                let notes = self
                    .target_page(offset, limit.clamp(1, 100))
                    .into_iter()
                    .map(|(id, n)| NoteItem {
                        id,
                        title: n.title.clone(),
                        tags: n.tags.clone(),
                        pinned: n.pinned,
                        favorited: n.favorited,
                        deleted: n.deleted,
                        protected: n.protection == NoteProtection::Protected,
                    })
                    .collect();
                Ok(ActionResult::Notes { notes })
            }
            Action::Read { id, offset, limit } => {
                let (read, version) = self.read_target(&id, offset, limit)?;
                Ok(ActionResult::Read {
                    id,
                    version,
                    offset: read.offset,
                    text: read.text,
                    total_bytes: read.total_bytes,
                })
            }
            Action::Edit { id, version, edit } => {
                if self.operations.writing() {
                    return Err(ActionError::Busy);
                }
                match self.edit_target(&id, &version, edit, now_ms)? {
                    AddressedEdit::Applied(outcome) => Ok(ActionResult::Applied {
                        applied: true,
                        saved: false,
                        revision: outcome.content_revision,
                    }),
                    AddressedEdit::Job(job) => self.schedule_file_job(&id, move || job.execute()),
                }
            }
            Action::Restore { id, version } => {
                if self.operations.writing() {
                    return Err(ActionError::Busy);
                }
                let path = self.resolve_target(&id)?;
                let expected = self.expected_version(&id, &version)?;
                let job = self.core.begin_addressed_restore(&path, expected)?;
                self.schedule_file_job(&id, move || job.execute())
            }
            Action::Create { title } => {
                if self.operations.writing() {
                    return Err(ActionError::Busy);
                }
                let timestamp = format_utc_timestamp(self.wall_time)?;
                let path = self.core.create_note_addressed(&title, &timestamp)?;
                let id = self.target_id(&path)?;
                Ok(ActionResult::Saved { id, saved: true })
            }
            Action::Rename { id, version, title } => {
                if self.operations.writing() {
                    return Err(ActionError::Busy);
                }
                let path = self.resolve_target(&id)?;
                let expected = self.expected_version(&id, &version)?;
                let timestamp = format_utc_timestamp(self.wall_time)?;
                let new = self
                    .core
                    .rename_note_addressed(&path, expected, &title, &timestamp)?;
                self.remap(&path, &new);
                Ok(ActionResult::Saved { id, saved: true })
            }
            Action::Metadata { id, version, edit } => {
                if self.operations.writing() {
                    return Err(ActionError::Busy);
                }
                let path = self.resolve_target(&id)?;
                let expected = self.expected_version(&id, &version)?;
                let timestamp = format_utc_timestamp(self.wall_time)?;
                self.core
                    .update_note_metadata_addressed(&path, expected, edit, &timestamp)?;
                Ok(ActionResult::Saved { id, saved: true })
            }
            Action::Open { id } => {
                if self.operations.writing() {
                    return Err(ActionError::Busy);
                }
                let path = self.resolve_target(&id)?;
                let index = self
                    .notes()
                    .iter()
                    .position(|n| n.path == path)
                    .ok_or(ActionError::NotFound)?;
                if self.notes()[index].protection == NoteProtection::Protected {
                    return Err(ActionError::RequiresUserInteraction);
                }
                self.open_note(index)?;
                Ok(ActionResult::Opened { opened: id })
            }
            Action::Search { query } => {
                if query.len() > 4096 {
                    return Err(ActionError::InvalidArguments);
                }
                if !self.operations.has_capacity() || self.operations.searching.is_some() {
                    return Err(ActionError::Busy);
                }
                let search = self.search_sender.as_ref().ok_or(ActionError::Busy)?;
                let (reply, response) = mpsc::sync_channel(1);
                search
                    .try_send(super::search::SearchCommand::ToolQuery { query, reply })
                    .map_err(|_| ActionError::Busy)?;
                self.operations.next += 1;
                let operation = format!(
                    "operations/{:x}/{:016x}",
                    self.session_id(),
                    self.operations.next
                );
                self.operations.entries.insert(
                    operation.clone(),
                    Operation {
                        progress: OperationProgress::default(),
                        state: OperationStatus::Running,
                        gate: Arc::new(AtomicU8::new(1)),
                        writes: false,
                    },
                );
                self.operations.searching = Some(operation.clone());
                self.operations.trim();
                let sender = self.operations.sender.clone();
                let operation_id = operation.clone();
                std::thread::spawn(move || {
                    let result = response
                        .recv_timeout(std::time::Duration::from_secs(30))
                        .map_err(|_| ActionError::Busy)
                        .and_then(|result| result.map_err(ActionError::Failed));
                    let _ = sender.send(Completion {
                        operation: operation_id,
                        started: false,
                        effect: None,
                        path: PathBuf::new(),
                        result: Err(ActionError::Cancelled),
                        search: Some(result),
                    });
                });
                Ok(ActionResult::Accepted {
                    operation,
                    saved: false,
                })
            }
            Action::RssList => Ok(ActionResult::Subscriptions {
                subscriptions: self
                    .rss_subscriptions()
                    .into_iter()
                    .map(|s| FeedItem {
                        id: s.subscription.id.to_string(),
                        title: s.display_title,
                    })
                    .collect(),
            }),
            Action::RssRefresh { id } => {
                let id =
                    stillus_core::ItemId::new(id).map_err(|_| ActionError::InvalidArguments)?;
                if !self
                    .rss_subscriptions()
                    .iter()
                    .any(|s| s.subscription.id == id)
                {
                    return Err(ActionError::NotFound);
                }
                let (reply, receiver) = mpsc::sync_channel(1);
                let operation = self.operations.register(self.session_id(), false)?;
                if let Err(error) = self.send_rss(super::rss::Command::Refresh(id, reply)) {
                    self.operations.finish(&operation, Err(error.clone()));
                    return Err(error);
                }
                let sender = self.operations.sender.clone();
                let tracked = operation.clone();
                std::thread::spawn(move || {
                    let result = receiver
                        .recv()
                        .map_err(|_| ActionError::Cancelled)
                        .and_then(|result| result)
                        .map(|()| OperationOutput::Effect {
                            changed: true,
                            saved: true,
                        });
                    let _ = sender.send(Completion {
                        operation: tracked,
                        started: false,
                        effect: Some(result),
                        path: PathBuf::new(),
                        result: Err(ActionError::Cancelled),
                        search: None,
                    });
                });
                Ok(ActionResult::Accepted {
                    operation,
                    saved: false,
                })
            }
            Action::Status { id } => self
                .operations
                .entries
                .get(&id)
                .map(|op| ActionResult::Status(op.state.clone()))
                .ok_or(ActionError::NotFound),
            Action::Cancel { id } => {
                let op = self
                    .operations
                    .entries
                    .get_mut(&id)
                    .ok_or(ActionError::NotFound)?;
                match op.state {
                    OperationStatus::Pending => {
                        op.gate
                            .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire)
                            .map_err(|_| ActionError::Busy)?;
                        self.operations.complete(&id, OperationStatus::Cancelled);
                        Ok(ActionResult::Cancelled { cancelled: true })
                    }
                    OperationStatus::Running => Err(ActionError::Busy),
                    _ => Ok(ActionResult::Cancelled { cancelled: false }),
                }
            }
            Action::UserInteraction => Err(ActionError::RequiresUserInteraction),
        };
        if mutates && result.is_ok() {
            self.actions_dirty = true;
        }
        self.operations.trim();
        result
    }
}

impl Operations {
    fn complete(&mut self, id: &str, status: OperationStatus) {
        if self.searching.as_deref() == Some(id) {
            self.searching = None;
        }
        let Some(operation) = self.entries.get_mut(id) else {
            return;
        };
        if matches!(operation.state, OperationStatus::Cancelled) {
            return;
        }
        operation.progress.phase = match &status {
            OperationStatus::Cancelled => "cancelled",
            OperationStatus::Failed(_) => "failed",
            _ => "finished",
        }
        .into();
        if matches!(
            &status,
            OperationStatus::Completed(_) | OperationStatus::Saved(_)
        ) {
            operation.progress.completed = operation.progress.total.unwrap_or(1);
            operation.progress.total = Some(operation.progress.completed);
        }
        operation.state = status;
        self.completed.retain(|completed| completed != id);
        self.completed.push_back(id.to_owned());
        self.trim();
    }
    fn trim(&mut self) {
        // Completion order, not submission order: a slow operation can finish last.
        while self.completed.len() > 64 {
            if let Some(id) = self.completed.pop_front() {
                self.entries.remove(&id);
            }
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct NoteItem {
    pub id: String,
    pub title: String,
    pub tags: Vec<String>,
    pub pinned: bool,
    pub favorited: bool,
    pub deleted: bool,
    pub protected: bool,
}
#[derive(Clone, Debug, Serialize)]
pub(crate) struct FeedItem {
    pub id: String,
    pub title: String,
}
#[derive(Clone, Debug, Serialize)]
pub(crate) struct SearchItem {
    pub id: String,
    pub path: String,
    pub title: String,
}
#[derive(Clone, Serialize)]
#[serde(untagged)]
pub(crate) enum OperationOutput {
    Saved {
        saved: bool,
        reconcile_error: Option<ActionError>,
    },
    Search {
        results: Vec<SearchItem>,
    },
    Effect {
        changed: bool,
        saved: bool,
    },
    Rss(super::rss::AddressedResult),
    Journal {
        rows: Vec<super::journal::Summary>,
        detail: Option<stillus_ai::journal::RequestRecord>,
        blocked: bool,
    },
}
#[derive(Clone, Serialize)]
#[serde(untagged)]
pub(crate) enum ActionResult {
    Notes {
        notes: Vec<NoteItem>,
    },
    Read {
        id: String,
        version: String,
        offset: usize,
        text: String,
        total_bytes: u64,
    },
    Applied {
        applied: bool,
        saved: bool,
        revision: u64,
    },
    Accepted {
        operation: String,
        saved: bool,
    },
    Saved {
        id: String,
        saved: bool,
    },
    Opened {
        opened: String,
    },
    Subscriptions {
        subscriptions: Vec<FeedItem>,
    },
    Scheduled {
        scheduled: bool,
    },
    Status(OperationStatus),
    Cancelled {
        cancelled: bool,
    },
}

#[cfg(test)]
impl Operations {
    pub(crate) fn test_pending(&mut self, id: &str) {
        self.entries.insert(
            id.to_owned(),
            Operation {
                progress: OperationProgress {
                    phase: "queued".into(),
                    ..Default::default()
                },
                state: OperationStatus::Pending,
                gate: Arc::new(AtomicU8::new(0)),
                writes: true,
            },
        );
    }
}

impl Operations {
    pub(super) fn progress(
        &self,
        id: &str,
    ) -> Result<(OperationStatus, OperationProgress), ActionError> {
        self.entries
            .get(id)
            .map(|operation| (operation.state.clone(), operation.progress.clone()))
            .ok_or(ActionError::NotFound)
    }
    pub(super) fn update_progress(&mut self, id: &str, progress: OperationProgress) -> bool {
        let Some(operation) = self.entries.get_mut(id) else {
            return false;
        };
        if operation.progress == progress {
            return false;
        }
        operation.progress = progress;
        true
    }
    pub(super) fn has_capacity(&self) -> bool {
        self.entries
            .values()
            .filter(|operation| {
                matches!(
                    operation.state,
                    OperationStatus::Pending | OperationStatus::Running
                )
            })
            .count()
            < 8
    }
    pub(super) fn register(&mut self, session: u64, writes: bool) -> Result<String, ActionError> {
        if writes && self.writing() {
            return Err(ActionError::Busy);
        }
        if self
            .entries
            .values()
            .filter(|operation| {
                matches!(
                    operation.state,
                    OperationStatus::Pending | OperationStatus::Running
                )
            })
            .count()
            >= 8
        {
            return Err(ActionError::Busy);
        }
        self.next = self.next.saturating_add(1);
        let id = format!("operations/{session:x}/{:016x}", self.next);
        self.entries.insert(
            id.clone(),
            Operation {
                progress: OperationProgress::default(),
                state: OperationStatus::Running,
                gate: Arc::new(AtomicU8::new(1)),
                writes,
            },
        );
        Ok(id)
    }
    pub(super) fn import_completion(
        &mut self,
        id: String,
        result: Result<OperationOutput, ActionError>,
    ) {
        self.entries.insert(
            id.clone(),
            Operation {
                state: OperationStatus::Running,
                progress: OperationProgress::default(),
                gate: Arc::new(AtomicU8::new(1)),
                writes: false,
            },
        );
        self.finish(&id, result);
    }
    pub(super) fn finish(&mut self, id: &str, result: Result<OperationOutput, ActionError>) {
        let status = match result {
            Ok(output) => OperationStatus::Completed(output),
            Err(ActionError::Cancelled) => OperationStatus::Cancelled,
            Err(error) => OperationStatus::Failed(error),
        };
        self.complete(id, status);
    }
}
