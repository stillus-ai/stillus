// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]

//! Owner-thread application state. Workers return completions; views consume effects.
use super::search::{SearchCommand, SearchEvent, SearchWorkerParts, spawn_search_worker};
use super::security::{
    PendingPasswordChange, PendingPasswordChangeState, PendingSecurityAction, RestoreCompletion,
    SearchSecurityOperation, SecureUiOperation,
};
use super::settings::PersistedExternalFile;
use super::{persistence, rss as rss_service, search, workspace::Workspace as WorkspaceSession};
use crate::application;
use crate::i18n::{UiText, msg};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, SyncSender},
    thread,
    time::{Instant, SystemTime},
};
use stillus_core::*;
use stillus_search::SearchResult;
use stillus_secure::MasterPassword;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ApplicationEvent {
    ResetEditor,
    ClosePassword,
    CloseProtectionPassword,
    AuthenticationFailed,
    ProtectionAuthenticationFailed,
    PasswordFinished,
}

/// Read-only UI projection. The slot cannot expose a mutable session outside this module.
pub(crate) struct WorkspaceSlot(Option<WorkspaceSession>);
impl WorkspaceSlot {
    pub(crate) fn as_ref(&self) -> Option<&WorkspaceSession> {
        self.0.as_ref()
    }
    pub(crate) fn is_some(&self) -> bool {
        self.0.is_some()
    }
    pub(crate) fn is_none(&self) -> bool {
        self.0.is_none()
    }
    pub(super) fn as_mut(&mut self) -> Option<&mut WorkspaceSession> {
        self.0.as_mut()
    }
}
static RSS_UI_SESSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SecurityActionOutcome {
    Completed,
    Pending,
    AuthenticationFailed,
    OperationFailed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UnlockOutcome {
    Pending,
    AuthenticationFailed,
    OperationFailed,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum SidebarFilter {
    All,
    Favorites,
    Tag(String),
    Trash,
}

pub(crate) fn secure_phase_order(phase: SecurePhase) -> u8 {
    match phase {
        SecurePhase::Validating => 0,
        SecurePhase::PreparingVerifier => 1,
        SecurePhase::PreparingSecrets => 2,
        SecurePhase::PreparingNotes => 3,
        SecurePhase::PreparingRecovery => 4,
        SecurePhase::BackingUpNotes => 5,
        SecurePhase::BackingUpSecrets => 6,
        SecurePhase::ReplacingRecovery => 7,
        SecurePhase::ReplacingSecrets => 8,
        SecurePhase::ReplacingNotes => 9,
        SecurePhase::ReplacingVerifier => 10,
        SecurePhase::Verifying => 11,
        SecurePhase::RollingBack => 12,
    }
}
pub(crate) fn note_matches_filter(
    tags: &[String],
    favorited: bool,
    deleted: bool,
    filter: &SidebarFilter,
) -> bool {
    match filter {
        SidebarFilter::All => !deleted,
        SidebarFilter::Favorites => !deleted && favorited,
        SidebarFilter::Tag(selected) => {
            !deleted
                && tags
                    .iter()
                    .any(|tag| category_path_is_same_or_descendant(tag, selected))
        }
        SidebarFilter::Trash => deleted,
    }
}
pub(crate) fn sidebar_note_order_key(filter: &SidebarFilter) -> Option<&str> {
    match filter {
        SidebarFilter::Favorites => Some(FAVORITED_ORDER_KEY),
        SidebarFilter::Tag(category) if category != FAVORITED_ORDER_KEY => Some(category),
        SidebarFilter::Tag(_) => None,
        SidebarFilter::All | SidebarFilter::Trash => None,
    }
}
pub(crate) fn is_current_search_generation(current: u64, incoming: u64) -> bool {
    current == incoming
}

pub(crate) struct Application {
    pub(crate) rss_session: u64,
    pub(crate) workspace: WorkspaceSlot,
    pub(crate) error: Option<UiText>,
    clock: std::sync::Arc<dyn Clock>,
    pub(crate) effects: Vec<ApplicationEvent>,
    pub(super) state_dirty: bool,
    pub(super) preferences_projection: Option<super::settings::SidebarSettings>,
    workspace_loader: Option<WorkspaceLoadTask>,
    workspace_executor: WorkspaceExecutor,
    unloaded_operation: Option<(String, super::actions::OperationStatus)>,
    workspace_loaded: Option<Result<WorkspaceChanged, UiText>>,
    external_deadline: u64,
    pub(crate) preferences:
        Option<std::rc::Rc<std::cell::RefCell<super::preferences::Preferences>>>,
    pub(super) api: super::api::ApiState,
    pub(crate) global: Option<std::rc::Rc<std::cell::RefCell<super::global::GlobalApplication>>>,
    #[cfg(not(test))]
    save_sender: SyncSender<PersistenceCompletion>,
    #[cfg(test)]
    pub(crate) save_sender: SyncSender<PersistenceCompletion>,
    #[cfg(not(test))]
    save_receiver: Receiver<PersistenceCompletion>,
    #[cfg(test)]
    pub(crate) save_receiver: Receiver<PersistenceCompletion>,
    pub(crate) save_worker_active: bool,
    #[cfg(not(test))]
    secure_sender: SyncSender<SecureWorkerEvent>,
    #[cfg(test)]
    pub(crate) secure_sender: SyncSender<SecureWorkerEvent>,
    #[cfg(not(test))]
    secure_receiver: Receiver<SecureWorkerEvent>,
    #[cfg(test)]
    pub(crate) secure_receiver: Receiver<SecureWorkerEvent>,
    pub(crate) secure_worker_active: bool,
    pub(crate) secure_operation_id: Option<u64>,
    pub(crate) secure_progress: Option<SecureProgress>,
    pub(crate) pending_password_change: Option<PendingPasswordChange>,
    pub(crate) password_change_error: Option<UiText>,
    pub(crate) password_change_result: Option<(usize, usize, usize)>,
    pub(crate) blocked_password_change_workspace: Option<PathBuf>,
    pub(crate) secure_ui_operation: Option<SecureUiOperation>,
    pub(crate) pending_note_path: Option<PathBuf>,
    pub(crate) pending_note_creation: Option<(SidebarFilter, String)>,
    pub(crate) note_creation_focus_pending: bool,
    pub(crate) pending_external_target: Option<DocumentTarget>,
    pub(crate) pending_external_close: Option<DocumentTarget>,
    pub(crate) pending_security_action: Option<PendingSecurityAction>,
    pub(crate) unlock_request: Option<usize>,
    #[cfg(not(test))]
    search_sender: SyncSender<SearchCommand>,
    #[cfg(test)]
    pub(crate) search_sender: SyncSender<SearchCommand>,
    #[cfg(not(test))]
    search_worker: Option<thread::JoinHandle<()>>,
    #[cfg(test)]
    pub(crate) search_worker: Option<thread::JoinHandle<()>>,
    #[cfg(not(test))]
    search_receiver: Receiver<SearchEvent>,
    #[cfg(test)]
    pub(crate) search_receiver: Receiver<SearchEvent>,
    pub(crate) search_security_operation: Option<SearchSecurityOperation>,
    pub(crate) search_operation_generation: u64,
    pub(crate) search_ready: bool,
    pub(crate) search_indexing: bool,
    pub(crate) search_error: Option<UiText>,
    pub(crate) search_query_generation: u64,
    pub(crate) search_query: String,
    pub(crate) search_results_generation: Option<u64>,
    pub(crate) search_results: Vec<SearchResult>,
    pub(crate) rss_status: BTreeMap<String, rss_service::Status>,
    pub(crate) rss_saves: BTreeMap<String, (u64, bool)>,
    pub(crate) rss_save_sequence: u64,
    pub(crate) expanded_rss_entry: Option<String>,
    pub(crate) rss_refreshing: BTreeSet<String>,
    pub(crate) selected_rss_entry: Option<String>,
}

impl Application {
    pub(crate) fn unloaded() -> Self {
        let (save_sender, save_receiver) = mpsc::sync_channel(1);
        let (secure_sender, secure_receiver) = mpsc::sync_channel(32);
        let (search_sender, _search_commands) = mpsc::sync_channel(64);
        let (_search_events, search_receiver) = mpsc::sync_channel(64);
        Self {
            workspace: WorkspaceSlot(None),

            error: None,
            clock: std::sync::Arc::new(SystemClock(Instant::now())),
            effects: Vec::new(),
            state_dirty: false,
            preferences_projection: None,
            workspace_loader: None,
            workspace_executor: std::sync::Arc::new(prepare_workspace_load),
            unloaded_operation: None,
            workspace_loaded: None,
            external_deadline: 1000,
            preferences: None,
            api: super::api::ApiState::default(),
            global: None,
            save_sender,
            save_receiver,
            save_worker_active: false,
            secure_sender,
            secure_receiver,
            secure_worker_active: false,
            secure_operation_id: None,
            secure_progress: None,
            pending_password_change: None,
            password_change_error: None,
            password_change_result: None,
            blocked_password_change_workspace: None,
            secure_ui_operation: None,

            pending_note_path: None,
            pending_note_creation: None,
            note_creation_focus_pending: false,
            pending_external_target: None,
            pending_external_close: None,
            pending_security_action: None,
            unlock_request: None,
            search_sender,
            search_worker: None,
            search_receiver,
            search_security_operation: None,
            search_operation_generation: 0,
            search_ready: false,
            search_indexing: false,
            search_error: None,
            search_query_generation: 0,
            search_query: String::new(),
            search_results_generation: None,
            search_results: Vec::new(),
            rss_session: RSS_UI_SESSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed),

            rss_status: BTreeMap::new(),
            rss_saves: BTreeMap::new(),
            rss_save_sequence: 0,
            expanded_rss_entry: None,
            rss_refreshing: BTreeSet::new(),
            selected_rss_entry: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn load(path: &Path) -> Self {
        Self::load_restoring(path, None)
    }

    pub(crate) fn load_restoring(path: &Path, restored_note: Option<&Path>) -> Self {
        Self::load_restoring_state(path, restored_note, &[], None, None)
    }

    pub(crate) fn load_restoring_state(
        path: &Path,
        restored_note: Option<&Path>,
        restored_external_files: &[PersistedExternalFile],
        restored_external: Option<&Path>,
        restored_rss: Option<&str>,
    ) -> Self {
        let (save_sender, save_receiver) = mpsc::sync_channel(1);
        let (secure_sender, secure_receiver) = mpsc::sync_channel(32);
        let workspace_result = WorkspaceSession::open(path);
        let password_change_blocked = match &workspace_result {
            Err(CoreError::PasswordChange(_)) => true,
            Err(CoreError::Security(error)) => error.blocks_workspace(),
            _ => false,
        };
        let search_suspended = password_change_blocked
            || workspace_result
                .as_ref()
                .ok()
                .is_some_and(|workspace| workspace.integrity_failure().is_some());
        let SearchWorkerParts {
            sender: search_sender,
            receiver: search_receiver,
            worker: search_worker,
        } = spawn_search_worker(path.to_path_buf(), search_suspended);
        let clock: std::sync::Arc<dyn Clock> = std::sync::Arc::new(SystemClock(Instant::now()));
        match workspace_result {
            Ok(mut workspace) => {
                workspace.bind_search(search_sender.clone());
                let mut restore_diagnostics = Vec::new();
                for persisted in restored_external_files {
                    let external_path = Path::new(&persisted.absolute_path);
                    match workspace.attach_external_file(external_path) {
                        Ok(DocumentTarget::ExternalFile { engine_id, .. })
                            if engine_id.as_str() != persisted.engine_id =>
                        {
                            restore_diagnostics.push(format!(
                                "external file {} belongs to engine {}, not {}",
                                external_path.display(),
                                engine_id,
                                persisted.engine_id
                            ));
                        }
                        Ok(_) => {}
                        Err(error) => restore_diagnostics.push(format!(
                            "external file {}: {error}",
                            external_path.display()
                        )),
                    }
                }
                let first_ready = workspace.notes().iter().position(|note| {
                    note.availability.is_ready()
                        && note.protection == NoteProtection::Plain
                        && !note.deleted
                });
                let restored_index = restored_note.and_then(|path| {
                    let canonical = path.canonicalize().ok()?;
                    workspace.notes().iter().position(|note| {
                        note.availability.is_ready()
                            && note.path.canonicalize().is_ok_and(|path| path == canonical)
                    })
                });
                let restored_external_target = restored_external.and_then(|path| {
                    let canonical = path.canonicalize().ok()?;
                    workspace
                        .external_files()
                        .iter()
                        .find(|file| {
                            matches!(file.availability, stillus_core::ItemAvailability::Ready)
                                && file.path.canonicalize().is_ok_and(|path| path == canonical)
                        })
                        .map(|file| (file.engine_id.clone(), file.item_id.clone()))
                });
                let mut unlock_request = None;
                let restored_rss_target = restored_rss
                    .and_then(|value| ItemId::new(value.to_owned()).ok())
                    .filter(|item_id| {
                        workspace
                            .rss_subscriptions()
                            .iter()
                            .any(|subscription| &subscription.subscription.id == item_id)
                    });
                let mut error = if let Some(item_id) = restored_rss_target {
                    workspace.open_rss(&item_id).err()
                } else if let Some((engine_id, item_id)) = restored_external_target {
                    workspace.open_external_item(&engine_id, &item_id).err()
                } else {
                    restored_index.or(first_ready).and_then(|index| {
                        if workspace.notes()[index].protection == NoteProtection::Protected {
                            unlock_request = Some(index);
                            workspace.select_protected_note(index).err()
                        } else {
                            workspace.open_note(index).err()
                        }
                    })
                }
                .map(|error| error.to_string());
                if error.is_none() && !restore_diagnostics.is_empty() {
                    error = Some(restore_diagnostics.join("; "));
                }
                if error.is_none() && !workspace.recovery_diagnostics().is_empty() {
                    error = Some(format!(
                        "recovery diagnostics: {}",
                        workspace.recovery_diagnostics().join("; ")
                    ));
                }
                Self {
                    workspace: WorkspaceSlot(Some(workspace)),

                    error: error.map(|details| UiText::Failure { details }),
                    clock,
                    effects: Vec::new(),
                    state_dirty: false,
                    preferences_projection: None,
                    workspace_loader: None,
                    workspace_executor: std::sync::Arc::new(prepare_workspace_load),
                    unloaded_operation: None,
                    workspace_loaded: None,
                    external_deadline: 1000,
                    preferences: None,
                    api: super::api::ApiState::default(),
                    global: None,
                    save_sender,
                    save_receiver,
                    save_worker_active: false,
                    secure_sender,
                    secure_receiver,
                    secure_worker_active: false,
                    secure_operation_id: None,
                    secure_progress: None,
                    pending_password_change: None,
                    password_change_error: None,
                    password_change_result: None,
                    blocked_password_change_workspace: None,
                    secure_ui_operation: None,

                    pending_note_path: None,
                    pending_note_creation: None,
                    note_creation_focus_pending: false,
                    pending_external_target: None,
                    pending_external_close: None,
                    pending_security_action: None,
                    unlock_request,
                    search_sender,
                    search_worker: Some(search_worker),
                    search_receiver,
                    search_security_operation: None,
                    search_operation_generation: 0,
                    search_ready: false,
                    search_indexing: true,
                    search_error: None,
                    search_query_generation: 0,
                    search_query: String::new(),
                    search_results_generation: None,
                    search_results: Vec::new(),
                    rss_session: RSS_UI_SESSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed),

                    rss_status: BTreeMap::new(),
                    rss_saves: BTreeMap::new(),
                    rss_save_sequence: 0,
                    expanded_rss_entry: None,
                    rss_refreshing: BTreeSet::new(),
                    selected_rss_entry: None,
                }
            }
            Err(error) => Self {
                workspace: WorkspaceSlot(None),

                error: Some(UiText::Failure {
                    details: error.to_string(),
                }),
                clock,
                effects: Vec::new(),
                state_dirty: false,
                preferences_projection: None,
                workspace_loader: None,
                workspace_executor: std::sync::Arc::new(prepare_workspace_load),
                unloaded_operation: None,
                workspace_loaded: None,
                external_deadline: 1000,
                preferences: None,
                api: super::api::ApiState::default(),
                global: None,
                save_sender,
                save_receiver,
                save_worker_active: false,
                secure_sender,
                secure_receiver,
                secure_worker_active: false,
                secure_operation_id: None,
                secure_progress: None,
                pending_password_change: None,
                password_change_error: None,
                password_change_result: None,
                blocked_password_change_workspace: password_change_blocked
                    .then(|| path.to_path_buf()),
                secure_ui_operation: None,

                pending_note_path: None,
                pending_note_creation: None,
                note_creation_focus_pending: false,
                pending_external_target: None,
                pending_external_close: None,
                pending_security_action: None,
                unlock_request: None,
                search_sender,
                search_worker: Some(search_worker),
                search_receiver,
                search_security_operation: None,
                search_operation_generation: 0,
                search_ready: false,
                search_indexing: true,
                search_error: None,
                search_query_generation: 0,
                search_query: String::new(),
                search_results_generation: None,
                search_results: Vec::new(),
                rss_session: RSS_UI_SESSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed),

                rss_status: BTreeMap::new(),
                rss_saves: BTreeMap::new(),
                rss_save_sequence: 0,
                expanded_rss_entry: None,
                rss_refreshing: BTreeSet::new(),
                selected_rss_entry: None,
            },
        }
    }

    pub(crate) fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    pub(crate) fn open_rss(&mut self, item_id: &ItemId) -> bool {
        let result = self
            .workspace
            .as_mut()
            .ok_or_else(|| CoreError::Workspace("workspace is not open".to_owned()))
            .and_then(|workspace| workspace.open_rss(item_id));
        match result {
            Ok(()) => {
                self.selected_rss_entry = None;
                self.expanded_rss_entry = None;
                self.error = None;
                true
            }
            Err(error) => {
                self.error = Some(UiText::Failure {
                    details: error.to_string(),
                });
                false
            }
        }
    }

    pub(crate) fn create_rss(&mut self, url: &str, active: &SidebarFilter) -> Option<ItemId> {
        let categories = match active {
            SidebarFilter::Tag(category) => vec![category.clone()],
            _ => Vec::new(),
        };
        let favorited = matches!(active, SidebarFilter::Favorites);
        let result = format_utc_timestamp(self.clock.wall_time()).and_then(|timestamp| {
            self.workspace
                .as_mut()
                .ok_or_else(|| CoreError::Workspace("workspace is not open".to_owned()))?
                .create_rss(url, categories, favorited, &timestamp)
        });
        match result {
            Ok(item_id) => {
                self.selected_rss_entry = None;
                self.expanded_rss_entry = None;
                self.error = None;
                Some(item_id)
            }
            Err(error) => {
                let message = error.to_string();
                self.error = Some(if message.contains("source/url") {
                    UiText::from(msg!(FeedUrlRequired))
                } else if matches!(error, CoreError::Workspace(ref value) if value.contains("conflict"))
                {
                    UiText::from(msg!(FeedAlreadyExists))
                } else {
                    UiText::Failure { details: message }
                });
                None
            }
        }
    }

    pub(crate) fn rss_command(&mut self, command: rss_service::Command) -> bool {
        match self
            .workspace
            .as_mut()
            .ok_or(application::tools::ActionError::NotFound)
            .and_then(|workspace| workspace.send_rss(command))
        {
            Ok(()) => true,
            Err(_) => {
                self.error = Some(msg!(RssFilterConflict).into());
                false
            }
        }
    }

    pub(crate) fn start_rss_refresh(&mut self, item_id: ItemId) -> bool {
        self.workspace.as_mut().is_some_and(|workspace| {
            workspace
                .execute_action(
                    super::actions::Action::RssRefresh {
                        id: item_id.to_string(),
                    },
                    0,
                )
                .is_ok()
        })
    }

    pub(crate) fn poll_rss(&mut self) -> bool {
        let mut changed = false;
        if let Some(workspace) = self.workspace.as_mut() {
            while let Some(snapshot) = workspace.receive_rss() {
                workspace.accept_rss_snapshot(snapshot.engine);
                self.rss_refreshing = snapshot.refreshing;
                self.rss_status = snapshot.status;
                self.rss_saves = snapshot.saves;
                changed = true;
            }
        }
        changed
    }

    pub(crate) fn select_rss_entry(&mut self, entry_id: &str) -> bool {
        let result = format_utc_timestamp(self.clock.wall_time()).and_then(|timestamp| {
            let id = self
                .workspace
                .as_ref()
                .and_then(WorkspaceSession::selected_rss)
                .cloned()
                .ok_or_else(|| CoreError::Workspace("workspace is not open".into()))?;
            if self.rss_command(rss_service::Command::Read(id, entry_id.into(), timestamp)) {
                Ok(true)
            } else {
                Err(CoreError::Workspace("RSS queue unavailable".into()))
            }
        });
        match result {
            Ok(_) => {
                let hidden = self
                    .workspace
                    .as_ref()
                    .and_then(|w| {
                        w.selected_rss()
                            .and_then(|id| w.rss_feed(id).ok().map(|(_, s)| s.hidden(entry_id)))
                    })
                    .unwrap_or(false);
                self.expanded_rss_entry = hidden.then(|| entry_id.to_owned());
                self.selected_rss_entry = Some(entry_id.to_owned());
                self.error = None;
                true
            }
            Err(error) => {
                self.error = Some(UiText::Failure {
                    details: error.to_string(),
                });
                false
            }
        }
    }

    pub(crate) fn move_rss_selection(&mut self, direction: i32) -> bool {
        let Some(workspace) = self.workspace.as_ref() else {
            return false;
        };
        let Some(item_id) = workspace.selected_rss().cloned() else {
            return false;
        };
        let Ok((feed, state)) = workspace.rss_feed(&item_id) else {
            return false;
        };
        let target = match self.selected_rss_entry.as_deref() {
            Some(selected) => {
                let Some(current) = feed.entries.iter().position(|entry| entry.id == selected)
                else {
                    return false;
                };
                let indices: Box<dyn Iterator<Item = usize>> = if direction > 0 {
                    Box::new(current + 1..feed.entries.len())
                } else {
                    Box::new((0..current).rev())
                };
                indices
                    .filter_map(|i| feed.entries.get(i))
                    .find(|entry| !state.hidden(&entry.id))
                    .map(|e| e.id.clone())
                    .unwrap_or_default()
            }
            None => feed
                .entries
                .iter()
                .filter(|e| !state.hidden(&e.id))
                .find(|e| !state.read_entry_ids.contains(&e.id))
                .or_else(|| feed.entries.iter().find(|e| !state.hidden(&e.id)))
                .map(|e| e.id.clone())
                .unwrap_or_default(),
        };
        !target.is_empty() && self.select_rss_entry(&target)
    }

    pub(crate) fn rename_selected_rss(&mut self, title: &str) -> bool {
        self.run_workspace_action(|workspace, timestamp| workspace.rename_rss(title, timestamp))
            .is_some()
    }

    pub(crate) fn toggle_selected_rss_pinned(&mut self) -> bool {
        self.run_workspace_action(|workspace, timestamp| {
            workspace.update_selected_rss_metadata(timestamp, |item| item.pinned = !item.pinned)
        })
        .is_some()
    }

    pub(crate) fn toggle_selected_rss_favorited(&mut self) -> bool {
        self.run_workspace_action(|workspace, timestamp| {
            workspace
                .update_selected_rss_metadata(timestamp, |item| item.favorited = !item.favorited)
        })
        .is_some()
    }

    pub(crate) fn set_selected_rss_deleted(&mut self, deleted: bool) -> bool {
        self.run_workspace_action(|workspace, timestamp| {
            workspace.update_selected_rss_metadata(timestamp, |item| item.deleted = deleted)
        })
        .is_some()
    }

    pub(crate) fn set_selected_rss_categories(&mut self, categories: Vec<String>) -> bool {
        self.run_workspace_action(|workspace, timestamp| {
            workspace.set_selected_rss_categories(&categories, timestamp)
        })
        .is_some()
    }

    pub(crate) fn retry_password_change_recovery(&mut self) -> bool {
        let Some(path) = self.blocked_password_change_workspace.clone() else {
            return false;
        };
        let mut replacement = Self::load_restoring(&path, None);
        if replacement.workspace.is_none() {
            self.error = replacement.error.take();
            replacement.request_search_worker_shutdown();
            return false;
        }
        self.request_search_worker_shutdown();
        replacement.global = self.global.clone();
        replacement.preferences = self.preferences.clone();
        *self = replacement;
        true
    }

    pub(crate) fn start_secure_job(
        &mut self,
        job: SecureJob,
        operation: SecureUiOperation,
    ) -> bool {
        if self.secure_worker_active || self.secure_ui_operation.is_some() {
            self.error = Some((msg!(SecureBusy)).into());
            return false;
        }
        self.secure_worker_active = true;
        self.secure_operation_id = Some(job.operation_id());
        self.secure_progress = None;
        self.secure_ui_operation = Some(operation);
        self.error = None;
        let sender = self.secure_sender.clone();
        application::security::start(job, sender);
        true
    }

    pub(crate) fn start_integrity_resolution(&mut self, resolution: IntegrityResolution) -> bool {
        let result = self
            .workspace
            .as_mut()
            .ok_or_else(|| CoreError::Workspace("workspace is not open".to_owned()))
            .and_then(|workspace| workspace.begin_integrity_resolution(resolution));
        match result {
            Ok(job) => self.start_secure_job(job, SecureUiOperation::Integrity),
            Err(error) => {
                self.error = Some(UiText::Failure {
                    details: error.to_string(),
                });
                false
            }
        }
    }

    pub(crate) fn request_master_password_change(
        &mut self,
        current: MasterPassword,
        new: MasterPassword,
    ) -> bool {
        if self.pending_password_change.is_some() || self.secure_worker_active {
            self.password_change_error = Some((msg!(PasswordChangeBusy)).into());
            return false;
        }
        if self
            .workspace
            .as_ref()
            .and_then(WorkspaceSession::document)
            .is_some_and(|document| {
                matches!(
                    document.save_status(),
                    SaveStatus::Error { .. } | SaveStatus::Conflict { .. }
                )
            })
        {
            self.password_change_error = Some((msg!(ResolveSaveFirst)).into());
            return false;
        }
        self.password_change_error = None;
        self.password_change_result = None;
        self.pending_password_change = Some(PendingPasswordChange {
            current,
            new,
            state: PendingPasswordChangeState::WaitingPersistence,
        });
        self.retry_pending_password_change();
        true
    }

    pub(crate) fn retry_pending_password_change(&mut self) -> bool {
        let Some(request) = self.pending_password_change.as_ref() else {
            return false;
        };
        if !matches!(
            request.state,
            PendingPasswordChangeState::WaitingPersistence
        ) {
            return false;
        }
        if self.save_worker_active {
            return false;
        }
        if let Some(document) = self.workspace.as_ref().and_then(WorkspaceSession::document) {
            match document.save_status() {
                SaveStatus::Clean { .. } => {}
                SaveStatus::Dirty { .. } | SaveStatus::Saving { .. } => return false,
                SaveStatus::Error { .. } | SaveStatus::Conflict { .. } => {
                    self.pending_password_change = None;
                    self.password_change_error = Some((msg!(PasswordChangeCancelled)).into());
                    return true;
                }
            }
            if matches!(document.recovery_status(), RecoveryStatus::Saving { .. }) {
                return false;
            }
        }
        let operation_id = self.next_search_operation_id();
        let paths = self
            .workspace
            .as_ref()
            .map(|workspace| {
                workspace
                    .notes()
                    .iter()
                    .filter(|note| note.protection == NoteProtection::Protected)
                    .map(|note| note.path.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if self
            .search_sender
            .try_send(SearchCommand::SuspendPasswordChange {
                operation_id,
                paths,
            })
            .is_err()
        {
            self.pending_password_change = None;
            self.password_change_error = Some((msg!(PauseSearchFailed)).into());
            return true;
        }
        if let Some(request) = self.pending_password_change.as_mut() {
            request.state = PendingPasswordChangeState::WaitingSearch { operation_id };
        }
        true
    }

    pub(crate) fn finish_password_change_search_suspend(&mut self, operation_id: u64) -> bool {
        let matches = self
            .pending_password_change
            .as_ref()
            .is_some_and(|request| {
                matches!(
                    request.state,
                    PendingPasswordChangeState::WaitingSearch {
                        operation_id: expected
                    } if expected == operation_id
                )
            });
        if !matches {
            return false;
        }
        let request = self
            .pending_password_change
            .take()
            .expect("matched password change request exists");
        let result = self
            .workspace
            .as_mut()
            .ok_or_else(|| CoreError::Workspace("workspace is not open".to_owned()))
            .and_then(|workspace| {
                workspace.begin_change_master_password(request.current, request.new)
            });
        match result {
            Ok(job) => self.start_secure_job(job, SecureUiOperation::ChangeMasterPassword),
            Err(error) => {
                self.password_change_error = Some(UiText::Failure {
                    details: error.to_string(),
                });
                let _ = self.search_sender.try_send(SearchCommand::Resume);
                true
            }
        }
    }

    pub(crate) fn finish_secure_completion(&mut self, completion: SecureCompletion) -> bool {
        if self.secure_operation_id != Some(completion.operation_id()) {
            return false;
        }
        self.secure_worker_active = false;
        self.secure_operation_id = None;
        let Some(operation) = self.secure_ui_operation.take() else {
            self.error = Some((msg!(UnknownSecureResult)).into());
            return true;
        };
        let password_dialog_operation = matches!(&operation, SecureUiOperation::Unlock { .. });
        let result = self
            .workspace
            .as_mut()
            .ok_or_else(|| CoreError::Workspace("workspace is not open".to_owned()))
            .and_then(|workspace| workspace.finish_secure_operation(completion));

        match (operation, result) {
            (_, Ok(SecureOutcome::IntegrityFailure)) => {
                self.error = None;
                self.suspend_search_for_integrity();
            }
            (
                SecureUiOperation::Integrity,
                Ok(
                    SecureOutcome::IntegrityRetried
                    | SecureOutcome::IntegrityRestored(_)
                    | SecureOutcome::MetadataChanged
                    | SecureOutcome::ProtectionDisabled(_),
                ),
            ) => {
                self.secure_progress = None;
                self.reset_editor();
                self.error = None;
                self.resume_search_after_integrity();
            }
            (
                SecureUiOperation::ChangeMasterPassword,
                Ok(SecureOutcome::MasterPasswordChanged {
                    notes,
                    recovery,
                    secrets,
                }),
            ) => {
                self.secure_progress = None;
                self.password_change_error = None;
                self.password_change_result = Some((notes, recovery, secrets));
                self.error = None;
                let _ = self.search_sender.try_send(SearchCommand::Resume);
            }
            (SecureUiOperation::ChangeMasterPassword, Err(error)) => {
                self.secure_progress = None;
                self.password_change_error = Some(UiText::Failure {
                    details: error.to_string(),
                });
                self.error = None;
                let _ = self.search_sender.try_send(SearchCommand::Resume);
            }
            (SecureUiOperation::Unlock { restore_recovery }, Ok(SecureOutcome::Unlocked)) => {
                self.emit(ApplicationEvent::ClosePassword);
                self.pending_note_path = None;
                self.reset_editor();
                self.error = None;
                if restore_recovery {
                    let now_ms = self.now_ms();
                    let next = self.workspace.as_mut().and_then(|workspace| {
                        workspace.selected_note().map(|note_index| {
                            workspace.begin_restore_protected_recovery(note_index, now_ms)
                        })
                    });
                    match next {
                        Some(Ok(job)) => {
                            self.start_secure_job(job, SecureUiOperation::RestoreRecovery);
                        }
                        Some(Err(error)) => {
                            self.error = Some(UiText::Failure {
                                details: error.to_string(),
                            })
                        }
                        None => self.error = Some((msg!(RecoveryNoteMissing)).into()),
                    }
                }
            }
            (SecureUiOperation::OpenProtected, Ok(SecureOutcome::Unlocked)) => {
                self.unlock_request = None;
                self.pending_note_path = None;
                self.reset_editor();
                self.error = None;
            }
            (
                SecureUiOperation::Protect {
                    action: _,
                    note_path,
                },
                Ok(SecureOutcome::Protected(_)),
            ) => {
                self.emit(ApplicationEvent::CloseProtectionPassword);
                self.reset_editor();
                self.error = None;
                self.begin_search_restore(note_path, RestoreCompletion::Protected);
            }
            (SecureUiOperation::Protect { action, note_path }, Err(error)) => {
                let completion = if error == CoreError::UnsavedChanges {
                    RestoreCompletion::RetryProtect(action)
                } else if error.is_master_password_authentication_failure() {
                    RestoreCompletion::AuthenticationFailed
                } else {
                    RestoreCompletion::ProtectFailed
                };
                self.begin_search_restore(note_path, completion);
            }
            (SecureUiOperation::DisableProtection, Ok(SecureOutcome::ProtectionDisabled(_))) => {
                self.reset_editor();
                self.error = None;
                self.request_search_reconcile();
            }
            (SecureUiOperation::Metadata, Ok(SecureOutcome::MetadataChanged)) => {
                self.error = None;
                self.request_search_reconcile();
            }
            (SecureUiOperation::ExternalPoll, Ok(SecureOutcome::ExternalPoll(poll))) => {
                if matches!(poll, ExternalPoll::Reloaded | ExternalPoll::Conflict) {
                    self.request_search_reconcile();
                }
                self.error = None;
            }
            (SecureUiOperation::DiscardReload, Ok(SecureOutcome::DiscardedAndReloaded)) => {
                self.error = None;
                self.request_search_reconcile();
            }
            (SecureUiOperation::RestoreRecovery, Ok(SecureOutcome::RecoveryRestored)) => {
                self.reset_editor();
                self.error = None;
            }
            (SecureUiOperation::Unlock { .. }, Err(error))
                if error.is_master_password_authentication_failure() =>
            {
                self.error = Some((msg!(AuthenticationFailed)).into());
                self.emit(ApplicationEvent::AuthenticationFailed);
            }
            (SecureUiOperation::OpenProtected, Err(error))
                if error.is_master_password_authentication_failure() =>
            {
                self.error = Some((msg!(AuthenticationFailed)).into());
            }
            (_, Err(error)) => {
                self.error = Some(UiText::Failure {
                    details: error.to_string(),
                })
            }
            (_, Ok(_)) => {
                self.error = Some((msg!(SecureUnexpectedEnd)).into());
            }
        }
        if password_dialog_operation {
            self.emit(ApplicationEvent::PasswordFinished);
        }
        true
    }

    pub(crate) fn finish_secure_progress(&mut self, progress: SecureProgress) -> bool {
        if self.secure_operation_id != Some(progress.operation_id) {
            return false;
        }
        if let Some(previous) = self.secure_progress {
            let previous_phase = secure_phase_order(previous.phase);
            let next_phase = secure_phase_order(progress.phase);
            let rollback_started = progress.phase == SecurePhase::RollingBack
                && previous.phase != SecurePhase::RollingBack;
            let percent_regressed = !rollback_started
                && match (previous.percent, progress.percent) {
                    (Some(_), None) => true,
                    (Some(previous), Some(next)) => next < previous,
                    (None, None | Some(_)) => false,
                };
            if next_phase < previous_phase
                || (next_phase == previous_phase && progress.completed < previous.completed)
                || percent_regressed
            {
                return false;
            }
        }
        self.secure_progress = Some(progress);
        true
    }

    #[cfg(test)]
    pub(crate) fn shutdown_search_worker(&mut self) {
        if let Some(worker) = self.search_worker.take() {
            application::search::shutdown(&self.search_sender, &self.search_receiver, worker)
                .expect("search worker exits cleanly");
        }
    }

    pub(crate) fn request_search_worker_shutdown(&mut self) {
        if let Some(worker) = self.search_worker.take() {
            if super::search::shutdown(&self.search_sender, &self.search_receiver, worker).is_err()
            {
                self.error = Some(UiText::Failure {
                    details: "search worker failed".into(),
                });
            }
        }
    }

    pub(crate) fn open_note(&mut self, index: usize) {
        self.pending_external_target = None;
        let now_ms = self.now_ms();
        let Some(workspace) = self.workspace.as_mut() else {
            self.error = Some(("workspace is not open".to_owned()).into());
            return;
        };
        let Some(target_path) = workspace.notes().get(index).map(|note| note.path.clone()) else {
            self.error = Some((format!("unknown note index {index}")).into());
            return;
        };
        let protected_requires_prompt = workspace.notes().get(index).is_some_and(|note| {
            note.protection == NoteProtection::Protected
                && !workspace.has_master_password()
                && workspace.document().is_none_or(|document| {
                    document.note_index() != index && !document.has_unsaved_work()
                })
        });
        if protected_requires_prompt {
            if let Err(error) = workspace.select_protected_note(index) {
                self.error = Some(UiText::Failure {
                    details: error.to_string(),
                });
                return;
            }
            self.unlock_request = Some(index);
            self.pending_note_path = None;
            self.error = None;
            return;
        }
        if workspace
            .notes()
            .get(index)
            .is_some_and(|note| note.protection == NoteProtection::Protected)
        {
            match workspace.begin_open_protected_note(index) {
                Ok(job) => {
                    self.unlock_request = None;
                    self.pending_note_path = None;
                    self.start_secure_job(job, SecureUiOperation::OpenProtected);
                }
                Err(CoreError::UnsavedChanges) => {
                    self.pending_note_path = Some(target_path);
                    workspace.retry_autosave(now_ms);
                    self.error = None;
                }
                Err(CoreError::MasterPasswordRequired) => {
                    self.unlock_request = Some(index);
                    self.pending_note_path = None;
                    self.error = None;
                }
                Err(error) => {
                    self.error = Some(UiText::Failure {
                        details: error.to_string(),
                    })
                }
            }
            return;
        }
        let result = workspace.open_note(index);
        match result {
            Ok(()) => {
                self.unlock_request = None;
                self.pending_note_path = None;
                self.reset_editor();
                self.error = None;
            }
            Err(CoreError::UnsavedChanges) => {
                self.pending_note_path = Some(target_path);
                workspace.retry_autosave(now_ms);
                self.error = None;
            }
            Err(CoreError::MasterPasswordRequired) => {
                self.unlock_request = Some(index);
                self.pending_note_path = None;
                self.error = None;
            }
            Err(error) => {
                self.error = Some(UiText::Failure {
                    details: error.to_string(),
                })
            }
        }
    }

    pub(crate) fn open_external_path(&mut self, path: &Path) -> bool {
        let now_ms = self.now_ms();
        let Some(workspace) = self.workspace.as_mut() else {
            self.error = Some(("workspace is not open".to_owned()).into());
            return false;
        };
        let known = workspace
            .external_files()
            .iter()
            .map(|file| (file.engine_id.clone(), file.item_id.clone()))
            .collect::<HashSet<_>>();
        let target = match workspace.attach_external_file(path) {
            Ok(target) => target,
            Err(error) => {
                self.error = Some(UiText::Failure {
                    details: error.to_string(),
                });
                return false;
            }
        };
        if let DocumentTarget::WorkspaceNote(index) = target {
            self.open_note(index);
            return self.error.is_none();
        }
        let DocumentTarget::ExternalFile { engine_id, item_id } = &target else {
            unreachable!()
        };
        match workspace.open_external_item(engine_id, item_id) {
            Ok(()) => {
                self.pending_note_path = None;
                self.pending_external_target = None;
                self.reset_editor();
                self.error = None;
                true
            }
            Err(CoreError::UnsavedChanges) => {
                self.pending_note_path = None;
                self.pending_external_target = Some(target);
                workspace.retry_autosave(now_ms);
                self.error = None;
                true
            }
            Err(error) => {
                if !known.contains(&(engine_id.clone(), item_id.clone())) {
                    let _ = workspace.close_external_file(engine_id, item_id);
                }
                self.error = Some(UiText::Failure {
                    details: error.to_string(),
                });
                false
            }
        }
    }

    pub(crate) fn accept_external_paths(&mut self, paths: &[PathBuf]) -> bool {
        if let [path] = paths {
            return self.open_external_path(path);
        }
        let mut first_target = None;
        let mut diagnostics = Vec::new();
        let mut changed = false;
        {
            let Some(workspace) = self.workspace.as_mut() else {
                self.error = Some(("workspace is not open".to_owned()).into());
                return false;
            };
            for path in paths {
                let known = workspace
                    .external_files()
                    .iter()
                    .map(|file| (file.engine_id.clone(), file.item_id.clone()))
                    .collect::<HashSet<_>>();
                match workspace.attach_external_file(path) {
                    Ok(target @ DocumentTarget::WorkspaceNote(_)) => {
                        first_target.get_or_insert(target);
                    }
                    Ok(target @ DocumentTarget::ExternalFile { .. }) => {
                        let DocumentTarget::ExternalFile { engine_id, item_id } = &target else {
                            unreachable!()
                        };
                        let ready = workspace.external_files().iter().any(|file| {
                            file.engine_id == *engine_id
                                && file.item_id == *item_id
                                && matches!(
                                    file.availability,
                                    stillus_core::ItemAvailability::Ready
                                )
                        });
                        if ready {
                            changed |= !known.contains(&(engine_id.clone(), item_id.clone()));
                            first_target.get_or_insert(target);
                        } else {
                            if !known.contains(&(engine_id.clone(), item_id.clone())) {
                                let _ = workspace.close_external_file(engine_id, item_id);
                            }
                            diagnostics.push(format!("cannot open {}", path.display()));
                        }
                    }
                    Err(error) => diagnostics.push(format!("{}: {error}", path.display())),
                }
            }
        }
        let opened = match first_target {
            Some(DocumentTarget::WorkspaceNote(index)) => {
                self.open_note(index);
                self.error.is_none()
            }
            Some(target @ DocumentTarget::ExternalFile { .. }) => self.open_external_target(target),
            None => false,
        };
        if !diagnostics.is_empty() {
            self.error = Some((diagnostics.join("; ")).into());
        }
        changed || opened
    }

    pub(crate) fn open_external_target(&mut self, target: DocumentTarget) -> bool {
        let DocumentTarget::ExternalFile { engine_id, item_id } = &target else {
            return false;
        };
        let now_ms = self.now_ms();
        let Some(workspace) = self.workspace.as_mut() else {
            self.error = Some(("workspace is not open".to_owned()).into());
            return false;
        };
        match workspace.open_external_item(engine_id, item_id) {
            Ok(()) => {
                self.pending_note_path = None;
                self.pending_external_target = None;
                self.reset_editor();
                self.error = None;
                true
            }
            Err(CoreError::UnsavedChanges) => {
                self.pending_note_path = None;
                self.pending_external_target = Some(target);
                workspace.retry_autosave(now_ms);
                self.error = None;
                true
            }
            Err(error) => {
                self.pending_external_target = None;
                self.error = Some(UiText::Failure {
                    details: error.to_string(),
                });
                false
            }
        }
    }

    pub(crate) fn open_pending_external(&mut self) -> bool {
        let Some(target) = self.pending_external_target.clone() else {
            return false;
        };
        self.open_external_target(target)
    }

    pub(crate) fn close_external_target(&mut self, target: DocumentTarget) -> bool {
        let DocumentTarget::ExternalFile { engine_id, item_id } = &target else {
            return false;
        };
        let now_ms = self.now_ms();
        let Some(workspace) = self.workspace.as_mut() else {
            self.error = Some(("workspace is not open".to_owned()).into());
            return false;
        };
        let selected = workspace.selected_target().as_ref() == Some(&target);
        let has_recovery = workspace.external_files().iter().any(|file| {
            file.engine_id == *engine_id && file.item_id == *item_id && file.recovery_available
        });
        if selected && has_recovery {
            self.pending_external_close = Some(target);
            workspace.retry_autosave(now_ms);
            self.error = Some((msg!(ExternalRecoveryKept)).into());
            return false;
        }
        match workspace.close_external_file(engine_id, item_id) {
            Ok(true) => {
                self.pending_external_close = None;
                if selected {
                    self.open_fallback_document();
                }
                self.error = None;
                true
            }
            Ok(false) => {
                self.pending_external_close = None;
                false
            }
            Err(CoreError::UnsavedChanges) => {
                self.pending_external_close = Some(target);
                workspace.retry_autosave(now_ms);
                self.error = None;
                false
            }
            Err(error) => {
                self.error = Some(UiText::Failure {
                    details: error.to_string(),
                });
                false
            }
        }
    }

    pub(crate) fn finish_pending_external_close(&mut self) -> bool {
        let Some(target) = self.pending_external_close.clone() else {
            return false;
        };
        let status = self
            .workspace
            .as_ref()
            .and_then(WorkspaceSession::document)
            .map(|document| document.save_status().clone());
        match status {
            Some(SaveStatus::Clean { .. }) | None => self.close_external_target(target),
            Some(SaveStatus::Dirty { .. }) => {
                let now_ms = self.now_ms();
                if let Some(workspace) = self.workspace.as_mut() {
                    workspace.retry_autosave(now_ms);
                }
                false
            }
            Some(SaveStatus::Saving { .. })
            | Some(SaveStatus::Error { .. })
            | Some(SaveStatus::Conflict { .. }) => false,
        }
    }

    pub(crate) fn open_fallback_document(&mut self) {
        let note_indices = self
            .workspace
            .as_ref()
            .map(|workspace| {
                workspace
                    .notes()
                    .iter()
                    .enumerate()
                    .filter(|(_, note)| note.availability.is_ready() && !note.deleted)
                    .map(|(index, _)| index)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for index in note_indices {
            self.open_note(index);
            if self.workspace.as_ref().is_some_and(|workspace| {
                workspace.selected_target() == Some(DocumentTarget::WorkspaceNote(index))
            }) {
                return;
            }
        }
        let external = self.workspace.as_ref().and_then(|workspace| {
            workspace
                .external_files()
                .iter()
                .find(|file| matches!(file.availability, stillus_core::ItemAvailability::Ready))
                .map(|file| DocumentTarget::ExternalFile {
                    engine_id: file.engine_id.clone(),
                    item_id: file.item_id.clone(),
                })
        });
        if let Some(target) = external {
            let _ = self.open_external_target(target);
        }
    }

    pub(crate) fn open_first_matching_note_if_unselected(
        &mut self,
        filter: &SidebarFilter,
    ) -> bool {
        let (note_index, rss_item) = self.workspace.as_ref().map_or((None, None), |workspace| {
            if workspace.selected_item().is_some() {
                return (None, None);
            }
            let note_index = workspace.notes().iter().position(|note| {
                note_matches_filter(&note.tags, note.favorited, note.deleted, filter)
            });
            let rss_item = (note_index.is_none())
                .then(|| {
                    workspace.rss_subscriptions().into_iter().find(|summary| {
                        let item = &summary.subscription;
                        note_matches_filter(&item.categories, item.favorited, item.deleted, filter)
                    })
                })
                .flatten()
                .map(|summary| summary.subscription.id);
            (note_index, rss_item)
        });
        if let Some(note_index) = note_index {
            self.open_note(note_index);
        } else if let Some(item_id) = rss_item
            && self.open_rss(&item_id)
        {
            return self.start_rss_refresh(item_id);
        }
        false
    }

    pub(crate) fn open_pending_note(&mut self) -> bool {
        let Some(target_path) = self.pending_note_path.clone() else {
            return false;
        };
        let now_ms = self.now_ms();
        let Some(workspace) = self.workspace.as_mut() else {
            self.pending_note_path = None;
            self.error = Some(("workspace is not open".to_owned()).into());
            return true;
        };
        let Some(index) = workspace
            .notes()
            .iter()
            .position(|note| note.path == target_path)
        else {
            self.pending_note_path = None;
            self.error = Some(("queued note no longer exists".to_owned()).into());
            return true;
        };
        let protected_requires_prompt = workspace.notes().get(index).is_some_and(|note| {
            note.protection == NoteProtection::Protected
                && !workspace.has_master_password()
                && workspace.document().is_none_or(|document| {
                    document.note_index() != index && !document.has_unsaved_work()
                })
        });
        if protected_requires_prompt {
            if let Err(error) = workspace.select_protected_note(index) {
                self.pending_note_path = None;
                self.error = Some(UiText::Failure {
                    details: error.to_string(),
                });
                return true;
            }
            self.unlock_request = Some(index);
            self.pending_note_path = None;
            self.error = None;
            return true;
        }
        if workspace
            .notes()
            .get(index)
            .is_some_and(|note| note.protection == NoteProtection::Protected)
        {
            return match workspace.begin_open_protected_note(index) {
                Ok(job) => {
                    self.unlock_request = None;
                    self.pending_note_path = None;
                    self.start_secure_job(job, SecureUiOperation::OpenProtected);
                    true
                }
                Err(CoreError::UnsavedChanges) => {
                    let should_accelerate = workspace.document().is_some_and(|document| {
                        matches!(document.save_status(), SaveStatus::Dirty { .. })
                    });
                    if should_accelerate {
                        workspace.retry_autosave(now_ms);
                    }
                    false
                }
                Err(CoreError::MasterPasswordRequired) => {
                    self.unlock_request = Some(index);
                    self.pending_note_path = None;
                    self.error = None;
                    true
                }
                Err(error) => {
                    self.pending_note_path = None;
                    self.error = Some(UiText::Failure {
                        details: error.to_string(),
                    });
                    true
                }
            };
        }
        match workspace.open_note(index) {
            Ok(()) => {
                self.unlock_request = None;
                self.pending_note_path = None;
                self.reset_editor();
                self.error = None;
                true
            }
            Err(CoreError::UnsavedChanges) => {
                let should_accelerate = workspace.document().is_some_and(|document| {
                    matches!(document.save_status(), SaveStatus::Dirty { .. })
                });
                if should_accelerate {
                    workspace.retry_autosave(now_ms);
                }
                false
            }
            Err(CoreError::MasterPasswordRequired) => {
                self.unlock_request = Some(index);
                self.pending_note_path = None;
                self.error = None;
                true
            }
            Err(error) => {
                self.pending_note_path = None;
                self.error = Some(UiText::Failure {
                    details: error.to_string(),
                });
                true
            }
        }
    }

    fn run_workspace_action<T>(
        &mut self,
        action: impl FnOnce(&mut WorkspaceSession, &str) -> Result<T, stillus_core::CoreError>,
    ) -> Option<T> {
        let result = format_utc_timestamp(self.clock.wall_time()).and_then(|timestamp| {
            self.workspace
                .as_mut()
                .ok_or_else(|| {
                    stillus_core::CoreError::Workspace("workspace is not open".to_owned())
                })
                .and_then(|workspace| action(workspace, &timestamp))
        });
        match result {
            Ok(value) => {
                self.reset_editor();
                self.error = None;
                self.request_search_reconcile();
                Some(value)
            }
            Err(error) => {
                self.error = Some(UiText::Failure {
                    details: error.to_string(),
                });
                None
            }
        }
    }

    pub(crate) fn request_note_creation(&mut self, active: SidebarFilter, title: String) -> bool {
        let title = title.as_str();
        let result = format_utc_timestamp(self.clock.wall_time()).and_then(|timestamp| {
            self.workspace
                .as_mut()
                .ok_or_else(|| CoreError::Workspace("workspace is not open".to_owned()))?
                .create_note(title, &timestamp)
        });
        match result {
            Ok(_) => {
                self.pending_note_creation = None;
                self.reset_editor();
                self.error = None;
                self.request_search_reconcile();
                match active {
                    SidebarFilter::All => {}
                    SidebarFilter::Favorites => {
                        self.toggle_favorited_selected();
                    }
                    SidebarFilter::Tag(tag) => {
                        self.add_tag_selected(&tag);
                    }
                    SidebarFilter::Trash => {}
                }
                self.apply(EditorCommand::SetSelection {
                    anchor: 2,
                    focus: 2 + title.len(),
                });
                self.note_creation_focus_pending = true;
                true
            }
            Err(CoreError::UnsavedChanges) => {
                self.pending_note_creation = Some((active, title.to_owned()));
                let now_ms = self.now_ms();
                if let Some(workspace) = self.workspace.as_mut() {
                    workspace.retry_autosave(now_ms);
                }
                self.error = None;
                false
            }
            Err(error) => {
                self.pending_note_creation = None;
                self.error = Some(UiText::Failure {
                    details: error.to_string(),
                });
                false
            }
        }
    }

    pub(crate) fn retry_pending_note_creation(&mut self) -> bool {
        let Some((active, title)) = self.pending_note_creation.clone() else {
            return false;
        };
        let created = self.request_note_creation(active, title);
        created || self.pending_note_creation.is_none()
    }

    #[cfg(test)]
    pub(crate) fn clear_category_note_order(&mut self, category: &str) -> Option<bool> {
        let result = self
            .workspace
            .as_mut()
            .ok_or_else(|| CoreError::Workspace("workspace is not open".to_owned()))
            .and_then(|workspace| workspace.clear_category_note_order(category));
        match result {
            Ok(changed) => {
                self.error = None;
                Some(changed)
            }
            Err(error) => {
                self.error = Some(UiText::Failure {
                    details: error.to_string(),
                });
                None
            }
        }
    }

    pub(crate) fn set_sidebar_catalog_order(
        &mut self,
        scope: &SidebarFilter,
        ordered: &[CatalogOrderItem],
    ) -> Option<bool> {
        let order_key = sidebar_note_order_key(scope)?;
        let result = self
            .workspace
            .as_mut()
            .ok_or_else(|| CoreError::Workspace("workspace is not open".to_owned()))
            .and_then(|workspace| workspace.set_catalog_order(order_key, ordered));
        match result {
            Ok(changed) => {
                self.error = None;
                Some(changed)
            }
            Err(error) => {
                self.error = Some(UiText::Failure {
                    details: error.to_string(),
                });
                None
            }
        }
    }

    pub(crate) fn clear_sidebar_note_order(&mut self, scope: &SidebarFilter) -> Option<bool> {
        let order_key = sidebar_note_order_key(scope)?;
        let result = self
            .workspace
            .as_mut()
            .ok_or_else(|| CoreError::Workspace("workspace is not open".to_owned()))
            .and_then(|workspace| workspace.clear_catalog_order(order_key));
        match result {
            Ok(changed) => {
                self.error = None;
                Some(changed)
            }
            Err(error) => {
                self.error = Some(UiText::Failure {
                    details: error.to_string(),
                });
                None
            }
        }
    }

    pub(crate) fn add_tag_selected(&mut self, tag: &str) -> bool {
        let protected = self
            .workspace
            .as_ref()
            .is_some_and(WorkspaceSession::selected_is_protected);
        if !protected {
            return self
                .run_workspace_action(|workspace, timestamp| {
                    workspace.add_tag_selected(tag, timestamp)
                })
                .unwrap_or(false);
        }
        let result = format_utc_timestamp(self.clock.wall_time()).and_then(|timestamp| {
            self.workspace
                .as_mut()
                .ok_or_else(|| CoreError::Workspace("workspace is not open".to_owned()))
                .and_then(|workspace| workspace.begin_add_tag_protected_selected(tag, &timestamp))
        });
        self.start_optional_metadata_job(result)
    }

    pub(crate) fn remove_tag_selected(&mut self, tag: &str) -> bool {
        let protected = self
            .workspace
            .as_ref()
            .is_some_and(WorkspaceSession::selected_is_protected);
        if !protected {
            return self
                .run_workspace_action(|workspace, timestamp| {
                    workspace.remove_tag_selected(tag, timestamp)
                })
                .unwrap_or(false);
        }
        let result = format_utc_timestamp(self.clock.wall_time()).and_then(|timestamp| {
            self.workspace
                .as_mut()
                .ok_or_else(|| CoreError::Workspace("workspace is not open".to_owned()))
                .and_then(|workspace| {
                    workspace.begin_remove_tag_protected_selected(tag, &timestamp)
                })
        });
        self.start_optional_metadata_job(result)
    }

    pub(crate) fn toggle_pinned_selected(&mut self) -> bool {
        let protected = self
            .workspace
            .as_ref()
            .is_some_and(WorkspaceSession::selected_is_protected);
        if !protected {
            return self
                .run_workspace_action(WorkspaceSession::toggle_pinned_selected)
                .is_some();
        }
        let result = format_utc_timestamp(self.clock.wall_time()).and_then(|timestamp| {
            self.workspace
                .as_mut()
                .ok_or_else(|| CoreError::Workspace("workspace is not open".to_owned()))
                .and_then(|workspace| {
                    workspace
                        .begin_toggle_pinned_protected_selected(&timestamp)
                        .map(|(_, job)| job)
                })
        });
        self.start_metadata_job(result)
    }

    pub(crate) fn toggle_favorited_selected(&mut self) -> bool {
        let protected = self
            .workspace
            .as_ref()
            .is_some_and(WorkspaceSession::selected_is_protected);
        if !protected {
            return self
                .run_workspace_action(WorkspaceSession::toggle_favorited_selected)
                .is_some();
        }
        let result = format_utc_timestamp(self.clock.wall_time()).and_then(|timestamp| {
            self.workspace
                .as_mut()
                .ok_or_else(|| CoreError::Workspace("workspace is not open".to_owned()))
                .and_then(|workspace| {
                    workspace
                        .begin_toggle_favorited_protected_selected(&timestamp)
                        .map(|(_, job)| job)
                })
        });
        self.start_metadata_job(result)
    }

    pub(crate) fn start_optional_metadata_job(
        &mut self,
        result: Result<Option<SecureJob>, CoreError>,
    ) -> bool {
        match result {
            Ok(Some(job)) => self.start_secure_job(job, SecureUiOperation::Metadata),
            Ok(None) => {
                self.error = None;
                false
            }
            Err(error) => {
                self.error = Some(UiText::Failure {
                    details: error.to_string(),
                });
                false
            }
        }
    }

    pub(crate) fn start_metadata_job(&mut self, result: Result<SecureJob, CoreError>) -> bool {
        match result {
            Ok(job) => self.start_secure_job(job, SecureUiOperation::Metadata),
            Err(error) => {
                self.error = Some(UiText::Failure {
                    details: error.to_string(),
                });
                false
            }
        }
    }

    pub(crate) fn set_deleted_selected(&mut self, deleted: bool) -> bool {
        let protected = self
            .workspace
            .as_ref()
            .is_some_and(WorkspaceSession::selected_is_protected);
        if !protected {
            return self
                .run_workspace_action(|workspace, timestamp| {
                    workspace.set_deleted_selected(deleted, timestamp)
                })
                .is_some();
        }
        let result = format_utc_timestamp(self.clock.wall_time()).and_then(|timestamp| {
            self.workspace
                .as_mut()
                .ok_or_else(|| CoreError::Workspace("workspace is not open".to_owned()))
                .and_then(|workspace| {
                    workspace.begin_set_deleted_protected_selected(deleted, &timestamp)
                })
        });
        self.start_metadata_job(result)
    }

    pub(crate) fn submit_search(&mut self, query: String) {
        self.invalidate_search_projection();
        self.search_query.clone_from(&query);
        let generation = self.search_query_generation;
        if self
            .search_sender
            .try_send(SearchCommand::Query { generation, query })
            .is_err()
        {
            self.search_error = Some((msg!(SearchStopped)).into());
        }
    }

    pub(crate) fn request_search_reconcile(&mut self) {
        if self
            .search_sender
            .try_send(SearchCommand::Reconcile)
            .is_err()
        {
            self.search_error = Some((msg!(SearchStopped)).into());
        }
    }

    pub(crate) fn suspend_search_for_integrity(&mut self) {
        self.invalidate_search_projection();
        let paths = self
            .workspace
            .as_ref()
            .and_then(WorkspaceSession::integrity_failure)
            .map(|failure| {
                vec![
                    failure.backup.source_path.clone(),
                    failure.commit.path.clone(),
                ]
            })
            .unwrap_or_default();
        if self
            .search_sender
            .try_send(SearchCommand::SuspendAndPurge { paths })
            .is_err()
        {
            self.search_error = Some((msg!(SearchStopped)).into());
        }
    }

    pub(crate) fn resume_search_after_integrity(&mut self) {
        if self.search_sender.try_send(SearchCommand::Resume).is_err() {
            self.search_error = Some((msg!(SearchStopped)).into());
        }
    }

    pub(crate) fn next_search_operation_id(&mut self) -> u64 {
        self.search_operation_generation = self.search_operation_generation.saturating_add(1);
        self.search_operation_generation
    }

    pub(crate) fn begin_search_purge(
        &mut self,
        action: PendingSecurityAction,
        note_path: PathBuf,
    ) -> SecurityActionOutcome {
        let operation_id = self.next_search_operation_id();
        if self
            .search_sender
            .try_send(SearchCommand::Purge {
                operation_id,
                note_path,
            })
            .is_err()
        {
            self.search_error = Some((msg!(SearchStopped)).into());
            self.error = Some((msg!(ExcludeSearchFailed)).into());
            return SecurityActionOutcome::OperationFailed;
        }
        self.pending_security_action = Some(action);
        self.search_security_operation = Some(SearchSecurityOperation::Purging { operation_id });
        self.error = None;
        SecurityActionOutcome::Pending
    }

    pub(crate) fn begin_search_restore(
        &mut self,
        note_path: PathBuf,
        completion: RestoreCompletion,
    ) {
        let operation_id = self.next_search_operation_id();
        if self
            .search_sender
            .try_send(SearchCommand::RestoreAfterFailedPurge {
                operation_id,
                note_path,
            })
            .is_err()
        {
            self.search_error = Some((msg!(SearchStopped)).into());
            self.finish_restore_completion(completion, Err("search worker stopped".to_owned()));
            return;
        }
        self.search_security_operation = Some(SearchSecurityOperation::Restoring {
            operation_id,
            completion,
        });
    }

    pub(crate) fn finish_search_purge(
        &mut self,
        operation_id: u64,
        result: Result<(), String>,
    ) -> bool {
        let Some(SearchSecurityOperation::Purging {
            operation_id: expected,
        }) = self.search_security_operation.as_ref()
        else {
            return false;
        };
        if *expected != operation_id {
            return false;
        }
        self.search_security_operation = None;
        let Some(action) = self.pending_security_action.take() else {
            self.error = Some((msg!(SecureActionMissing)).into());
            return true;
        };
        let note_path = action.note_path().to_path_buf();
        match result {
            Ok(()) => self.finish_protect_after_purge(action, note_path),
            Err(error) => {
                self.begin_search_restore(note_path, RestoreCompletion::PurgeFailed(error));
            }
        }
        true
    }

    pub(crate) fn finish_search_restore(
        &mut self,
        operation_id: u64,
        result: Result<(), String>,
    ) -> bool {
        let operation = self.search_security_operation.take();
        let Some(SearchSecurityOperation::Restoring {
            operation_id: expected,
            completion,
        }) = operation
        else {
            self.search_security_operation = operation;
            return false;
        };
        if expected != operation_id {
            self.search_security_operation = Some(SearchSecurityOperation::Restoring {
                operation_id: expected,
                completion,
            });
            return false;
        }
        self.finish_restore_completion(completion, result);
        true
    }

    pub(crate) fn finish_restore_completion(
        &mut self,
        completion: RestoreCompletion,
        restore_result: Result<(), String>,
    ) {
        if let Err(error) = &restore_result {
            self.search_error =
                Some((msg!(RestoreSearchFailed , "error" => error.to_string())).into());
        }
        match completion {
            RestoreCompletion::Protected => {
                if restore_result.is_ok() {
                    self.error = None;
                } else {
                    self.error = Some((msg!(ProtectedMetadataPending).to_owned()).into());
                }
            }
            RestoreCompletion::PurgeFailed(purge_error) => {
                self.search_error = Some(match restore_result {
                    Ok(()) => UiText::from(purge_error),
                    Err(restore_error) => {
                        msg!(PurgeRestoreFailed , "purge_error" => purge_error, "restore_error" => restore_error).into()
                    }
                });
                self.error = Some((msg!(ExcludeSearchFailed)).into());
            }
            RestoreCompletion::RetryProtect(action) if restore_result.is_ok() => {
                self.pending_security_action = Some(action);
                let now_ms = self.now_ms();
                if let Some(workspace) = self.workspace.as_mut() {
                    workspace.retry_autosave(now_ms);
                }
                self.error = None;
            }
            RestoreCompletion::RetryProtect(_) | RestoreCompletion::ProtectFailed => {
                self.error = Some((msg!(ProtectFailed)).into());
            }
            RestoreCompletion::AuthenticationFailed => {
                self.error = Some((msg!(AuthenticationFailed)).into());
                self.emit(ApplicationEvent::ProtectionAuthenticationFailed);
            }
        }
    }

    pub(crate) fn protect_selected(
        &mut self,
        password: Option<MasterPassword>,
    ) -> SecurityActionOutcome {
        let note_path = self.workspace.as_ref().and_then(|workspace| {
            workspace
                .selected_note()
                .and_then(|index| workspace.notes().get(index))
                .map(|note| note.path.clone())
        });
        let Some(note_path) = note_path else {
            self.error = Some((msg!(NoSelection)).into());
            return SecurityActionOutcome::OperationFailed;
        };

        self.request_security_action(PendingSecurityAction::Protect {
            note_path,
            password,
        })
    }

    pub(crate) fn request_security_action(
        &mut self,
        action: PendingSecurityAction,
    ) -> SecurityActionOutcome {
        match self.security_action_is_ready(&action) {
            Ok(true) => self.execute_security_action(action),
            Ok(false) => {
                self.pending_note_path = None;
                self.pending_security_action = Some(action);
                let now_ms = self.now_ms();
                if let Some(workspace) = self.workspace.as_mut() {
                    workspace.retry_autosave(now_ms);
                }
                self.error = None;
                SecurityActionOutcome::Pending
            }
            Err(error) => {
                self.error = Some(error);
                SecurityActionOutcome::OperationFailed
            }
        }
    }

    pub(crate) fn security_action_is_ready(
        &self,
        action: &PendingSecurityAction,
    ) -> Result<bool, UiText> {
        let workspace = self
            .workspace
            .as_ref()
            .ok_or_else(|| "workspace is not open".to_owned())?;
        if self.secure_worker_active || workspace.secure_operation_pending() {
            return Ok(false);
        }
        let note_index = workspace.selected_note().ok_or_else(|| msg!(NoSelection))?;
        let note = workspace
            .notes()
            .get(note_index)
            .ok_or_else(|| msg!(SelectionUnavailable))?;
        if note.path != action.note_path() {
            return Err(msg!(SelectionChanged).into());
        }
        let document = workspace
            .document()
            .filter(|document| document.note_index() == note_index)
            .ok_or_else(|| msg!(SelectionNotOpen))?;
        let canonical_clean = matches!(document.save_status(), SaveStatus::Clean { .. });
        if canonical_clean
            && note.recovery_available
            && matches!(action, PendingSecurityAction::Protect { .. })
        {
            return Err(msg!(ResolveRecoveryFirst).into());
        }
        let recovery_write_active =
            matches!(document.recovery_status(), RecoveryStatus::Saving { .. });
        Ok(canonical_clean && !recovery_write_active)
    }

    pub(crate) fn retry_pending_security_action(&mut self) -> bool {
        if self.search_security_operation.is_some() || self.secure_worker_active {
            return false;
        }
        let Some(action) = self.pending_security_action.take() else {
            return false;
        };
        match self.security_action_is_ready(&action) {
            Ok(true) => {
                let outcome = self.execute_security_action(action);
                if outcome == SecurityActionOutcome::AuthenticationFailed {
                    self.error = Some((msg!(AuthenticationFailed)).into());
                }
                true
            }
            Ok(false) => {
                self.pending_security_action = Some(action);
                false
            }
            Err(error) => {
                self.error = Some(error);
                true
            }
        }
    }

    pub(crate) fn execute_security_action(
        &mut self,
        action: PendingSecurityAction,
    ) -> SecurityActionOutcome {
        match &action {
            PendingSecurityAction::Protect {
                note_path,
                password: _,
            } => {
                let note_path = note_path.clone();
                self.execute_protect_action(action, note_path)
            }
            PendingSecurityAction::Lock { .. } => {
                let result = self
                    .workspace
                    .as_mut()
                    .ok_or(CoreError::Workspace("workspace is not open".to_owned()))
                    .and_then(WorkspaceSession::lock_selected);
                match result {
                    Ok(()) => {
                        self.reset_editor();
                        self.error = None;
                        SecurityActionOutcome::Completed
                    }
                    Err(CoreError::UnsavedChanges) => {
                        self.pending_security_action = Some(action);
                        let now_ms = self.now_ms();
                        if let Some(workspace) = self.workspace.as_mut() {
                            workspace.retry_autosave(now_ms);
                        }
                        self.error = None;
                        SecurityActionOutcome::Pending
                    }
                    Err(error) => {
                        self.error = Some(UiText::Failure {
                            details: error.to_string(),
                        });
                        SecurityActionOutcome::OperationFailed
                    }
                }
            }
            PendingSecurityAction::DisableProtection { .. } => {
                let result = self
                    .workspace
                    .as_mut()
                    .ok_or(CoreError::Workspace("workspace is not open".to_owned()))
                    .and_then(WorkspaceSession::begin_disable_protection_selected);
                match result {
                    Ok(job) => {
                        if self.start_secure_job(job, SecureUiOperation::DisableProtection) {
                            SecurityActionOutcome::Pending
                        } else {
                            SecurityActionOutcome::OperationFailed
                        }
                    }
                    Err(CoreError::UnsavedChanges) => {
                        self.pending_security_action = Some(action);
                        let now_ms = self.now_ms();
                        if let Some(workspace) = self.workspace.as_mut() {
                            workspace.retry_autosave(now_ms);
                        }
                        self.error = None;
                        SecurityActionOutcome::Pending
                    }
                    Err(error) => {
                        self.error = Some(UiText::Failure {
                            details: error.to_string(),
                        });
                        SecurityActionOutcome::OperationFailed
                    }
                }
            }
        }
    }

    pub(crate) fn execute_protect_action(
        &mut self,
        action: PendingSecurityAction,
        note_path: PathBuf,
    ) -> SecurityActionOutcome {
        self.invalidate_search_projection();
        self.begin_search_purge(action, note_path)
    }

    pub(crate) fn finish_protect_after_purge(
        &mut self,
        action: PendingSecurityAction,
        note_path: PathBuf,
    ) {
        let password = match &action {
            PendingSecurityAction::Protect { password, .. } => password.clone(),
            PendingSecurityAction::Lock { .. }
            | PendingSecurityAction::DisableProtection { .. } => {
                self.error = Some((msg!(InvalidSecureAction)).into());
                return;
            }
        };
        let result = self
            .workspace
            .as_mut()
            .ok_or(CoreError::Workspace("workspace is not open".to_owned()))
            .and_then(|workspace| workspace.begin_protect_selected(password));
        match result {
            Ok(job) => {
                if !self.start_secure_job(
                    job,
                    SecureUiOperation::Protect {
                        action,
                        note_path: note_path.clone(),
                    },
                ) {
                    self.begin_search_restore(note_path, RestoreCompletion::ProtectFailed);
                }
            }
            Err(error) => {
                let completion = if error == CoreError::UnsavedChanges {
                    RestoreCompletion::RetryProtect(action)
                } else if error.is_master_password_authentication_failure() {
                    RestoreCompletion::AuthenticationFailed
                } else {
                    RestoreCompletion::ProtectFailed
                };
                self.begin_search_restore(note_path, completion);
            }
        }
    }

    pub(crate) fn invalidate_search_projection(&mut self) {
        self.search_query_generation = self.search_query_generation.saturating_add(1);
        self.search_results_generation = None;
        self.search_results.clear();
    }

    pub(crate) fn accept_search_results(
        &mut self,
        generation: u64,
        results: Vec<SearchResult>,
    ) -> bool {
        if !is_current_search_generation(self.search_query_generation, generation)
            || self.search_indexing
            || self.search_error.is_some()
        {
            return false;
        }
        self.search_results = results;
        self.search_results_generation = Some(generation);
        true
    }

    pub(crate) fn search_result_generation(&self, query: &str) -> Option<u64> {
        self.search_results_generation.filter(|generation| {
            *generation == self.search_query_generation
                && self.search_query == query
                && !query.trim().is_empty()
                && !self.search_indexing
                && self.search_error.is_none()
        })
    }

    pub(crate) fn unlock_note(
        &mut self,
        note_index: usize,
        password: MasterPassword,
        restore_recovery: bool,
    ) -> UnlockOutcome {
        let result = self
            .workspace
            .as_mut()
            .ok_or(CoreError::Workspace("workspace is not open".to_owned()))
            .and_then(|workspace| workspace.begin_unlock_note(note_index, password));
        match result {
            Ok(job) => {
                if self.start_secure_job(job, SecureUiOperation::Unlock { restore_recovery }) {
                    UnlockOutcome::Pending
                } else {
                    UnlockOutcome::OperationFailed
                }
            }
            Err(CoreError::Secure(_) | CoreError::MasterPasswordRequired) => {
                self.error = None;
                UnlockOutcome::AuthenticationFailed
            }
            Err(error) => {
                self.error = Some(UiText::Failure {
                    details: error.to_string(),
                });
                UnlockOutcome::OperationFailed
            }
        }
    }

    pub(crate) fn lock_selected(&mut self) -> SecurityActionOutcome {
        let note_path = self.workspace.as_ref().and_then(|workspace| {
            workspace
                .selected_note()
                .and_then(|index| workspace.notes().get(index))
                .map(|note| note.path.clone())
        });
        let Some(note_path) = note_path else {
            self.error = Some((msg!(NoSelection)).into());
            return SecurityActionOutcome::OperationFailed;
        };
        self.request_security_action(PendingSecurityAction::Lock { note_path })
    }

    pub(crate) fn disable_protection_selected(&mut self) -> SecurityActionOutcome {
        let note_path = self.workspace.as_ref().and_then(|workspace| {
            workspace
                .selected_note()
                .and_then(|index| workspace.notes().get(index))
                .map(|note| note.path.clone())
        });
        let Some(note_path) = note_path else {
            self.error = Some((msg!(NoSelection)).into());
            return SecurityActionOutcome::OperationFailed;
        };
        self.request_security_action(PendingSecurityAction::DisableProtection { note_path })
    }

    pub(crate) fn restore_recovery_note(&mut self, note_index: usize) -> Result<(), CoreError> {
        let now_ms = self.now_ms();
        let workspace = self
            .workspace
            .as_mut()
            .ok_or_else(|| CoreError::Workspace("workspace is not open".to_owned()))?;
        let protected = workspace
            .notes()
            .get(note_index)
            .is_some_and(|note| note.protection == NoteProtection::Protected);
        if protected {
            let job = workspace.begin_restore_protected_recovery(note_index, now_ms)?;
            if self.start_secure_job(job, SecureUiOperation::RestoreRecovery) {
                Ok(())
            } else {
                Err(CoreError::UnsavedChanges)
            }
        } else {
            workspace.restore_recovery(note_index, now_ms)?;
            self.reset_editor();
            self.error = None;
            Ok(())
        }
    }

    pub(crate) fn restore_selected_recovery(&mut self) -> Result<Option<usize>, CoreError> {
        let target = self
            .workspace
            .as_ref()
            .and_then(WorkspaceSession::selected_target)
            .ok_or_else(|| CoreError::NoteUnavailable("document is not selected".to_owned()))?;
        match target {
            DocumentTarget::WorkspaceNote(note_index) => {
                self.restore_recovery_note(note_index)?;
                Ok(Some(note_index))
            }
            DocumentTarget::ExternalFile { engine_id, item_id } => {
                let now_ms = self.now_ms();
                self.workspace
                    .as_mut()
                    .ok_or_else(|| CoreError::Workspace("workspace is not open".to_owned()))?
                    .restore_external_recovery(&engine_id, &item_id, now_ms)?;
                self.reset_editor();
                self.error = None;
                Ok(None)
            }
        }
    }

    pub(crate) fn discard_local_and_reload(&mut self) -> Result<(), CoreError> {
        let protected = self
            .workspace
            .as_ref()
            .is_some_and(WorkspaceSession::selected_is_protected);
        if protected {
            let job = self
                .workspace
                .as_mut()
                .ok_or_else(|| CoreError::Workspace("workspace is not open".to_owned()))?
                .begin_discard_protected_local_and_reload()?;
            if self.start_secure_job(job, SecureUiOperation::DiscardReload) {
                Ok(())
            } else {
                Err(CoreError::UnsavedChanges)
            }
        } else {
            self.workspace
                .as_mut()
                .ok_or_else(|| CoreError::Workspace("workspace is not open".to_owned()))?
                .discard_local_and_reload()?;
            self.request_search_reconcile();
            self.retry_pending_security_action();
            Ok(())
        }
    }

    pub(crate) fn open_search_result(
        &mut self,
        generation: u64,
        query: &str,
        relative_path: &str,
    ) -> bool {
        // A pointer event can outlive the row that produced it; keyboard events
        // can arrive before the query effect. Validate both against the input.
        if self.search_result_generation(query) != Some(generation)
            || !self
                .search_results
                .iter()
                .any(|result| result.relative_path == relative_path)
        {
            return false;
        }
        let Some(workspace) = self.workspace.as_ref() else {
            self.error = Some(("workspace is not open".to_owned()).into());
            return false;
        };
        let absolute_path = workspace.root().join(relative_path);
        let Some(index) = workspace
            .notes()
            .iter()
            .position(|note| note.path == absolute_path)
        else {
            self.error = Some(("search result is no longer present".to_owned()).into());
            self.request_search_reconcile();
            return false;
        };
        self.open_note(index);
        self.error.is_none()
    }
    pub(crate) fn apply(&mut self, command: EditorCommand) -> Option<String> {
        let now_ms = self.now_ms();
        match self.workspace.as_mut()?.apply_selected_at(command, now_ms) {
            Ok(outcome) => {
                self.error = None;
                outcome.clipboard
            }
            Err(error) => {
                self.error = Some(UiText::Failure {
                    details: error.to_string(),
                });
                None
            }
        }
    }
    fn reset_editor(&mut self) {
        self.emit(ApplicationEvent::ResetEditor);
    }
    pub(crate) fn retry_save(&mut self) -> bool {
        let now = self.now_ms();
        self.workspace
            .as_mut()
            .is_some_and(|workspace| workspace.retry_autosave(now))
    }
    pub(crate) fn rebuild_search(&mut self) {
        if self.search_sender.try_send(SearchCommand::Rebuild).is_ok() {
            self.invalidate_search_projection();
            self.search_indexing = true;
            self.search_error = None;
        } else {
            self.search_error = Some(msg!(SearchStopped).into());
        }
    }

    pub(crate) fn poll_persistence(&mut self) -> bool {
        let mut job = None;
        let mut changed = false;
        let completions = self.save_receiver.try_iter().collect::<Vec<_>>();
        for completion in completions {
            self.save_worker_active = false;
            let canonical_saved = completion.canonical_verified();
            let old_selected_path = self.workspace.as_ref().and_then(|workspace| {
                workspace
                    .selected_note()
                    .and_then(|index| workspace.notes().get(index))
                    .map(|note| note.path.clone())
            });
            let result = self
                .workspace
                .as_mut()
                .ok_or_else(|| "workspace is not open".to_owned())
                .and_then(|workspace| {
                    workspace
                        .finish_persistence(completion)
                        .map_err(|error| error.to_string())
                });
            if let Err(error) = result {
                self.error = Some(UiText::Failure {
                    details: error.to_string(),
                });
            } else if self
                .workspace
                .as_ref()
                .is_some_and(|workspace| workspace.integrity_failure().is_some())
            {
                self.suspend_search_for_integrity();
            } else if canonical_saved {
                let new_selected_path = self.workspace.as_ref().and_then(|workspace| {
                    workspace
                        .selected_note()
                        .and_then(|index| workspace.notes().get(index))
                        .map(|note| note.path.clone())
                });
                if let (Some(old_path), Some(new_path)) =
                    (old_selected_path.as_deref(), new_selected_path.as_deref())
                    && old_path != new_path
                {
                    if let Some(action) = self.pending_security_action.as_mut() {
                        action.replace_note_path(old_path, new_path);
                    }
                    if self.pending_note_path.as_deref() == Some(old_path) {
                        self.pending_note_path = Some(new_path.to_path_buf());
                    }
                }
                self.request_search_reconcile();
            }
            changed = true;
        }
        let secure_events = self.secure_receiver.try_iter().collect::<Vec<_>>();
        for event in secure_events {
            match event {
                SecureWorkerEvent::Progress(progress) => {
                    changed |= self.finish_secure_progress(progress);
                }
                SecureWorkerEvent::Completed(completion) => {
                    changed |= self.finish_secure_completion(*completion);
                }
            }
        }
        changed |= self.poll_api();
        changed |= self.retry_pending_security_action();
        changed |= self.retry_pending_password_change();
        changed |= self.retry_pending_note_creation();
        changed |= self.open_pending_note();
        changed |= self.open_pending_external();
        changed |= self.finish_pending_external_close();
        let persistence_allowed = self.pending_password_change.as_ref().is_none_or(|request| {
            matches!(
                request.state,
                PendingPasswordChangeState::WaitingPersistence
            )
        });
        if persistence_allowed && !self.save_worker_active && !self.secure_worker_active {
            let now_ms = self.now_ms();
            match format_utc_timestamp(self.clock.wall_time()) {
                Ok(modified) => {
                    let result = self
                        .workspace
                        .as_mut()
                        .map(|workspace| workspace.begin_persistence(now_ms, modified))
                        .transpose();
                    match result {
                        Ok(Some(Some(save_job))) => {
                            self.save_worker_active = true;
                            job = Some(save_job);
                            changed = true;
                        }
                        Ok(Some(None)) | Ok(None) => {}
                        Err(error) => {
                            self.error = Some(UiText::Failure {
                                details: error.to_string(),
                            });
                            changed = true;
                        }
                    }
                }
                Err(error) => {
                    self.error = Some(UiText::Failure {
                        details: error.to_string(),
                    });
                    changed = true;
                }
            }
        }
        if let Some(job) = job {
            persistence::start(job, self.save_sender.clone());
        }
        changed
    }

    pub(crate) fn poll_search(&mut self) -> bool {
        let mut changed = false;
        let mut rerun = false;
        if self
            .workspace
            .as_mut()
            .is_some_and(WorkspaceSession::poll_actions)
        {
            changed = true;
            self.request_search_reconcile();
        }
        let events = self.search_receiver.try_iter().collect::<Vec<_>>();
        for event in events {
            match event {
                SearchEvent::Indexing => {
                    self.invalidate_search_projection();
                    self.search_indexing = true;
                    self.search_error = None;
                    changed = true;
                }
                SearchEvent::Ready => {
                    rerun = true;
                    self.search_ready = true;
                    self.search_indexing = false;
                    self.search_error = None;
                    changed = true;
                }
                SearchEvent::Changed => {
                    rerun = true;
                    changed = true;
                }
                SearchEvent::Results {
                    generation,
                    results,
                } => {
                    if !self.accept_search_results(generation, results) {
                        continue;
                    }
                    changed = true;
                }
                SearchEvent::PurgeFinished {
                    operation_id,
                    result,
                } => {
                    changed |= self.finish_search_purge(operation_id, result);
                }
                SearchEvent::RestoreFinished {
                    operation_id,
                    result,
                } => {
                    changed |= self.finish_search_restore(operation_id, result);
                }
                SearchEvent::PasswordChangeSuspended { operation_id } => {
                    changed |= self.finish_password_change_search_suspend(operation_id);
                }
                SearchEvent::Error(error) => {
                    self.invalidate_search_projection();
                    self.search_indexing = false;
                    self.search_error = Some(UiText::Failure {
                        details: error.to_string(),
                    });
                    changed = true;
                }
            }
        }
        if rerun && !self.search_query.trim().is_empty() {
            self.submit_search(self.search_query.clone());
        }
        changed
    }

    fn poll_external(&mut self) -> bool {
        if self.pending_password_change.is_some()
            || matches!(
                self.secure_ui_operation,
                Some(SecureUiOperation::ChangeMasterPassword)
            )
        {
            return false;
        }
        let now = self.now_ms();
        match self
            .workspace
            .as_mut()
            .map(|workspace| workspace.begin_poll_external(now))
            .transpose()
        {
            Ok(Some(ExternalPollStart::Immediate(
                ExternalPoll::Reloaded | ExternalPoll::Conflict,
            ))) => {
                self.request_search_reconcile();
                true
            }
            Ok(Some(ExternalPollStart::Secure(job))) => {
                self.start_secure_job(job, SecureUiOperation::ExternalPoll)
            }
            Ok(_) => false,
            Err(error) => {
                self.error = Some(UiText::Failure {
                    details: error.to_string(),
                });
                true
            }
        }
    }

    /// Pump on the owner thread, including when no particular UI page is open.
    pub(crate) fn poll(&mut self) -> bool {
        let mut changed = self.poll_workspace_loader();
        changed |= std::mem::take(&mut self.state_dirty);
        changed |= self
            .global
            .as_ref()
            .is_some_and(|global| global.borrow_mut().poll(self.now_ms()));
        if let Some(preferences) = &self.preferences {
            let mut preferences = preferences.borrow_mut();
            changed |= preferences.poll(self.now_ms());
            if let Some(details) = preferences.take_error() {
                self.error = Some(UiText::Failure { details });
            }
        }
        changed |= self.poll_search();
        changed |= self.poll_rss();
        changed |= self.poll_persistence();
        changed |= self.poll_api();
        let now = self.now_ms();
        if now >= self.external_deadline {
            self.external_deadline = now.saturating_add(1000);
            changed |= self.poll_external();
        }
        changed
    }

    /// Absolute monotonic deadline, also used by headless callers.
    pub(crate) fn next_deadline(&self) -> u64 {
        let next = self.now_ms().saturating_add(25);
        if self.save_worker_active || self.secure_worker_active {
            return next;
        }
        self.workspace
            .as_ref()
            .and_then(WorkspaceSession::next_persistence_deadline)
            .map_or(next, |deadline| deadline.min(next))
    }

    pub(crate) fn take_preferences_projection(
        &mut self,
    ) -> Option<super::settings::SidebarSettings> {
        self.preferences_projection.take()
    }

    pub(crate) fn shutdown(&mut self) -> Result<(), String> {
        if let Some(task) = self.workspace_loader.take() {
            task.gate.store(2, std::sync::atomic::Ordering::Release);
            drop(task.receiver);
            task.worker
                .join()
                .map_err(|_| "workspace worker failed".to_owned())?;
        }
        // Writers already past their cancellation point must finish before exit.
        // Keep consuming security/search completions so bounded queues cannot stall them.
        while self.save_worker_active
            || self.secure_worker_active
            || self.search_security_operation.is_some()
            || self
                .workspace
                .as_ref()
                .is_some_and(WorkspaceSession::actions_busy)
        {
            self.poll();
            thread::sleep(std::time::Duration::from_millis(1));
        }
        if let Some(worker) = self.search_worker.take() {
            search::shutdown(&self.search_sender, &self.search_receiver, worker)
                .map_err(|_| "search worker failed".to_owned())?;
        }
        self.workspace.0.take();
        Ok(())
    }
}

pub(crate) fn category_path_is_same_or_descendant(candidate: &str, ancestor: &str) -> bool {
    if candidate == ancestor {
        return true;
    }
    let candidate = category_path_segments(candidate);
    let ancestor = category_path_segments(ancestor);
    candidate.len() > ancestor.len() && candidate.starts_with(&ancestor)
}

impl Application {
    pub(crate) fn rename_selected(&mut self, title: &str) -> bool {
        self.run_workspace_action(|workspace, timestamp| {
            workspace.rename_selected(title, timestamp)
        })
        .is_some()
    }
    #[cfg(test)]
    pub(crate) fn test_set_workspace(&mut self, workspace: WorkspaceSession) {
        self.workspace.0 = Some(workspace);
    }
    #[cfg(test)]
    pub(crate) fn test_workspace_mut(&mut self) -> Option<&mut WorkspaceSession> {
        self.workspace.as_mut()
    }
}

pub(crate) fn category_path_segments(category: &str) -> Vec<&str> {
    let segments = category.split('/').collect::<Vec<_>>();
    if segments.iter().any(|segment| segment.is_empty()) {
        vec![category]
    } else {
        segments
    }
}

pub(crate) trait Clock {
    fn now_ms(&self) -> u64;
    fn wall_time(&self) -> SystemTime;
}
struct SystemClock(Instant);
impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.0.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
    fn wall_time(&self) -> SystemTime {
        SystemTime::now()
    }
}
impl Application {
    pub(crate) fn set_clock(&mut self, clock: std::sync::Arc<dyn Clock>) {
        self.clock = clock;
        self.update_action_clock();
    }
    pub(super) fn update_action_clock(&mut self) {
        if let Some(workspace) = self.workspace.as_mut() {
            workspace.wall_time = self.clock.wall_time();
        }
    }
    fn emit(&mut self, event: ApplicationEvent) {
        // These are idempotent view effects, never worker completions.
        if !self.effects.contains(&event) {
            self.effects.push(event);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkspaceSwitchBlocker {
    Persistence,
    Security,
    Unsaved,
    SaveFailure,
}
pub(crate) fn workspace_switch_blocker(model: &Application) -> Option<WorkspaceSwitchBlocker> {
    if model
        .workspace
        .as_ref()
        .is_some_and(WorkspaceSession::actions_busy)
    {
        return Some(WorkspaceSwitchBlocker::Persistence);
    }
    if model.save_worker_active
        || model.pending_note_path.is_some()
        || model.pending_external_target.is_some()
        || model.pending_external_close.is_some()
    {
        return Some(WorkspaceSwitchBlocker::Persistence);
    }
    if model.secure_worker_active
        || model.secure_ui_operation.is_some()
        || model.pending_security_action.is_some()
        || model.pending_password_change.is_some()
        || model.search_security_operation.is_some()
        || model.unlock_request.is_some()
        || model.workspace.as_ref().is_some_and(|workspace| {
            workspace.secure_operation_pending() || workspace.integrity_failure().is_some()
        })
    {
        return Some(WorkspaceSwitchBlocker::Security);
    }
    model
        .workspace
        .as_ref()
        .and_then(WorkspaceSession::document)
        .and_then(|document| match document.save_status() {
            SaveStatus::Clean { .. } => None,
            SaveStatus::Dirty { .. } | SaveStatus::Saving { .. } => {
                Some(WorkspaceSwitchBlocker::Unsaved)
            }
            SaveStatus::Error { .. } | SaveStatus::Conflict { .. } => {
                Some(WorkspaceSwitchBlocker::SaveFailure)
            }
        })
}
pub(crate) struct PreparedWorkspaceSwitch {
    pub(crate) canonical_path: PathBuf,
    pub(crate) model: Application,
    pub(crate) store: super::preferences::Preferences,
    pub(crate) settings: super::settings::UiSettings,
    pub(crate) diagnostic: Option<String>,
}

pub(crate) fn prepare_workspace_switch(path: &Path) -> Result<PreparedWorkspaceSwitch, UiText> {
    if !path.is_absolute() {
        return Err(msg!(EnterAbsoluteWorkspace).into());
    }
    let canonical_path = path
        .canonicalize()
        .map_err(|error| msg!(OpenFolderFailed , "error" => error.to_string()))?;
    if !canonical_path.is_dir() {
        return Err(msg!(SelectedNotFolder).into());
    }
    let application::preferences::Load {
        store,
        settings,
        diagnostic,
    } = super::preferences::Preferences::load(&canonical_path);
    let restored_note = settings
        .selected_note
        .as_deref()
        .and_then(|path| super::settings::resolve_note_path(&canonical_path, path));
    let selected_external = settings.selected_external.as_deref().map(Path::new);
    let mut model = Application::load_restoring_state(
        &canonical_path,
        restored_note.as_deref(),
        &settings.external_files,
        selected_external,
        settings.selected_rss.as_deref(),
    );
    if model.workspace.is_none() {
        let error = model
            .error
            .clone()
            .unwrap_or_else(|| msg!(OpenWorkspaceFailed).into());
        model.request_search_worker_shutdown();
        return Err(error);
    }
    Ok(PreparedWorkspaceSwitch {
        canonical_path,
        model,
        store,
        settings,
        diagnostic,
    })
}

pub(crate) struct WorkspaceChanged {
    pub canonical_path: PathBuf,
    pub settings: super::settings::UiSettings,
    pub diagnostic: Option<UiText>,
    pub changed: bool,
}
pub(crate) fn initialize_workspace(path: &Path) -> Result<(), CoreError> {
    stillus_core::initialize_workspace(path)
}
impl Application {
    fn apply_workspace_switch(
        &mut self,
        canonical: PathBuf,
        mut prepared: PreparedWorkspaceSwitch,
    ) -> Result<WorkspaceChanged, UiText> {
        if let Some(preferences) = &self.preferences {
            let result = preferences.borrow_mut().flush();
            if let Err(error) = result {
                prepared.model.request_search_worker_shutdown();
                return Err(msg!(SaveSettingsFailed, "error" => error.to_string()).into());
            }
            *preferences.borrow_mut() = prepared.store;
        } else {
            self.preferences = Some(std::rc::Rc::new(std::cell::RefCell::new(prepared.store)));
        }
        prepared.model.set_clock(self.clock.clone());
        prepared.model.workspace_executor = self.workspace_executor.clone();
        prepared.model.preferences = self.preferences.clone();
        prepared.model.global = self.global.clone();
        let warning = self
            .global
            .as_ref()
            .and_then(|global| {
                global
                    .borrow_mut()
                    .remember_workspace_from_ui(&canonical)
                    .err()
            })
            .map(|error| UiText::from(msg!(RememberWorkspaceFailed, "error" => error.to_string())));
        let diagnostic = match (prepared.diagnostic, warning) {
            (Some(settings), Some(global)) => Some(UiText::Joined(vec![settings.into(), global])),
            (Some(settings), None) => Some(settings.into()),
            (None, warning) => warning,
        };
        self.request_search_worker_shutdown();
        *self = prepared.model;
        self.reset_editor();
        if let Some(id) = self
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.selected_rss())
            .cloned()
        {
            self.start_rss_refresh(id);
        }
        Ok(WorkspaceChanged {
            canonical_path: canonical,
            settings: prepared.settings,
            diagnostic,
            changed: true,
        })
    }
}

impl Application {
    #[cfg(test)]
    pub(crate) fn set_rss_executor(&mut self, executor: super::rss::Executor) {
        if let Some(workspace) = self.workspace.as_mut() {
            workspace.set_rss_executor(executor);
        }
    }
}

/// A worker owns this draft until the owner accepts it. Dropping a cancelled
/// or undelivered draft drains and joins its search worker before releasing it.
pub(super) struct LoadedWorkspace {
    canonical_path: PathBuf,
    workspace: Option<WorkspaceSession>,
    error: Option<UiText>,
    unlock_request: Option<usize>,
    search: Option<SearchWorkerParts>,
    rss_session: u64,
    store: Option<super::preferences::Preferences>,
    settings: super::settings::UiSettings,
    diagnostic: Option<String>,
}
impl Drop for LoadedWorkspace {
    fn drop(&mut self) {
        if let Some(search) = self.search.take() {
            // Dropping the draft must not leave an index worker using this directory.
            let _ = search::shutdown(&search.sender, &search.receiver, search.worker);
        }
    }
}
impl LoadedWorkspace {
    fn into_prepared(mut self) -> PreparedWorkspaceSwitch {
        let mut model = Application::unloaded();
        model.workspace = WorkspaceSlot(self.workspace.take());
        model.error = self.error.take();
        model.unlock_request = self.unlock_request;
        let search = self
            .search
            .take()
            .expect("loaded workspace owns its search worker");
        model.search_sender = search.sender;
        model.search_receiver = search.receiver;
        model.search_worker = Some(search.worker);
        model.search_indexing = true;
        model.rss_session = self.rss_session;
        PreparedWorkspaceSwitch {
            canonical_path: std::mem::take(&mut self.canonical_path),
            model,
            store: self
                .store
                .take()
                .expect("loaded workspace owns its preferences"),
            settings: std::mem::take(&mut self.settings),
            diagnostic: self.diagnostic.take(),
        }
    }
}
#[derive(Debug)]
pub(crate) struct WorkspaceLoadError {
    pub(crate) reason: super::actions::ActionError,
    pub(crate) message: UiText,
}
impl WorkspaceLoadError {
    fn cancelled() -> Self {
        Self {
            reason: super::actions::ActionError::Cancelled,
            message: msg!(OpenWorkspaceFailed).into(),
        }
    }
    fn blocked(blocker: WorkspaceSwitchBlocker) -> Self {
        let message = match blocker {
            WorkspaceSwitchBlocker::Persistence => msg!(WaitSave),
            WorkspaceSwitchBlocker::Security => msg!(WaitSecure),
            WorkspaceSwitchBlocker::Unsaved => msg!(WaitAutosave),
            WorkspaceSwitchBlocker::SaveFailure => msg!(ResolveSaveFirst),
        };
        let reason = match blocker {
            WorkspaceSwitchBlocker::SaveFailure => {
                super::actions::ActionError::RequiresUserInteraction
            }
            _ => super::actions::ActionError::Busy,
        };
        Self {
            reason,
            message: message.into(),
        }
    }
}
pub(super) type WorkspaceExecutor = std::sync::Arc<
    dyn Fn(PathBuf, bool) -> Result<LoadedWorkspace, WorkspaceLoadError> + Send + Sync,
>;
enum WorkspaceLoadOutput {
    Loaded(LoadedWorkspace),
    Unchanged(PathBuf),
}
struct WorkspaceLoadTask {
    id: String,
    receiver: Receiver<Result<WorkspaceLoadOutput, WorkspaceLoadError>>,
    worker: thread::JoinHandle<()>,
    gate: std::sync::Arc<std::sync::atomic::AtomicU8>,
    initialize: bool,
}
pub(super) fn prepare_workspace_load(
    path: PathBuf,
    initialize: bool,
) -> Result<LoadedWorkspace, WorkspaceLoadError> {
    if initialize {
        initialize_workspace(&path).map_err(|error| WorkspaceLoadError {
            message: msg!(CreateWorkspaceFailed, "error" => error.to_string()).into(),
            reason: error.into(),
        })?;
    }
    let PreparedWorkspaceSwitch {
        canonical_path,
        model,
        store,
        settings,
        diagnostic,
    } = prepare_workspace_switch(&path).map_err(|message| WorkspaceLoadError {
        reason: super::actions::ActionError::Failed("workspace load failed".into()),
        message,
    })?;
    let Application {
        workspace,
        error,
        unlock_request,
        search_sender,
        search_receiver,
        search_worker,
        rss_session,
        ..
    } = model;
    Ok(LoadedWorkspace {
        canonical_path,
        workspace: workspace.0,
        error,
        unlock_request,
        search: search_worker.map(|worker| SearchWorkerParts {
            sender: search_sender,
            receiver: search_receiver,
            worker,
        }),
        rss_session,
        store: Some(store),
        settings,
        diagnostic,
    })
}
impl Application {
    #[cfg(test)]
    pub(super) fn set_workspace_executor(&mut self, executor: WorkspaceExecutor) {
        self.workspace_executor = executor;
    }
    pub(crate) fn begin_workspace_switch(
        &mut self,
        path: &Path,
        initialize: bool,
    ) -> Result<String, WorkspaceLoadError> {
        if self.workspace_loader.is_some() || self.workspace_loaded.is_some() {
            return Err(WorkspaceLoadError::blocked(
                WorkspaceSwitchBlocker::Persistence,
            ));
        }
        if let Some(blocker) = workspace_switch_blocker(self) {
            return Err(WorkspaceLoadError::blocked(blocker));
        }
        if !path.is_absolute() {
            return Err(WorkspaceLoadError {
                reason: super::actions::ActionError::InvalidArguments,
                message: msg!(EnterAbsoluteWorkspace).into(),
            });
        }
        let id = if let Some(workspace) = self.workspace.as_mut() {
            workspace
                .operations
                .register(workspace.session_id(), false)
                .map_err(|_| WorkspaceLoadError::blocked(WorkspaceSwitchBlocker::Persistence))?
        } else {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            format!(
                "operations/workspace/{:016x}",
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            )
        };
        let path = path.to_owned();
        let (sender, receiver) = mpsc::sync_channel(1);
        let gate = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0));
        let worker_gate = gate.clone();
        let executor = self.workspace_executor.clone();
        let current = self
            .workspace
            .as_ref()
            .map(|workspace| workspace.root().to_owned());
        let worker = thread::spawn(move || {
            use std::sync::atomic::Ordering;
            let result = if worker_gate
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                let same_path = path
                    .canonicalize()
                    .ok()
                    .filter(|path| Some(path) == current.as_ref());
                let result = if let Some(path) = same_path {
                    Ok(WorkspaceLoadOutput::Unchanged(path))
                } else {
                    executor(path, initialize).map(WorkspaceLoadOutput::Loaded)
                };
                if worker_gate.load(Ordering::Acquire) == 2 {
                    drop(result);
                    Err(WorkspaceLoadError::cancelled())
                } else {
                    result
                }
            } else {
                Err(WorkspaceLoadError::cancelled())
            };
            // Exactly one completion fits the bounded channel; a closed owner drops the draft.
            let _ = sender.send(result);
        });
        self.workspace_loaded = None;
        self.unloaded_operation = None;
        self.workspace_loader = Some(WorkspaceLoadTask {
            id: id.clone(),
            receiver,
            worker,
            gate,
            initialize,
        });
        self.state_dirty = true;
        Ok(id)
    }
    pub(crate) fn workspace_load_status(
        &self,
        id: &str,
    ) -> Option<super::actions::OperationStatus> {
        self.workspace_loader
            .as_ref()
            .filter(|task| task.id == id)
            .map(|_| super::actions::OperationStatus::Running)
            .or_else(|| {
                self.unloaded_operation
                    .as_ref()
                    .filter(|(pending, _)| pending == id)
                    .map(|(_, status)| status.clone())
            })
    }
    pub(super) fn cancel_workspace_load(&mut self, id: &str) -> Option<bool> {
        use std::sync::atomic::Ordering;
        self.workspace_loader
            .as_ref()
            .filter(|task| task.id == id)
            .map(|task| {
                if task.initialize {
                    // Initialization may already have begun an irreversible write.
                    task.gate
                        .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                } else {
                    task.gate.store(2, Ordering::Release);
                    true
                }
            })
    }
    pub(crate) fn workspace_projection_pending(&self) -> bool {
        matches!(
            self.workspace_loaded,
            Some(Ok(WorkspaceChanged { changed: true, .. }))
        )
    }
    pub(crate) fn take_workspace_switch_result(
        &mut self,
    ) -> Option<Result<WorkspaceChanged, UiText>> {
        self.workspace_loaded.take()
    }
    fn poll_workspace_loader(&mut self) -> bool {
        let completion =
            self.workspace_loader
                .as_ref()
                .and_then(|task| match task.receiver.try_recv() {
                    Ok(result) => Some(result),
                    Err(mpsc::TryRecvError::Empty) => None,
                    Err(mpsc::TryRecvError::Disconnected) => Some(Err(WorkspaceLoadError {
                        reason: super::actions::ActionError::Failed(
                            "workspace worker failed".into(),
                        ),
                        message: msg!(OpenWorkspaceFailed).into(),
                    })),
                });
        let Some(completion) = completion else {
            return false;
        };
        let task = self
            .workspace_loader
            .take()
            .expect("workspace completion belongs to loader");
        let _ = task.worker.join();
        let completion = if task.gate.load(std::sync::atomic::Ordering::Acquire) == 2 {
            drop(completion);
            Err(WorkspaceLoadError::cancelled())
        } else {
            completion
        };
        let result = completion.and_then(|output| {
            if let Some(blocker) = workspace_switch_blocker(self) {
                return Err(WorkspaceLoadError::blocked(blocker));
            }
            let loaded = match output {
                WorkspaceLoadOutput::Unchanged(canonical_path) => {
                    return Ok(WorkspaceChanged {
                        canonical_path,
                        settings: Default::default(),
                        diagnostic: Some(msg!(WorkspaceAlreadyOpen).into()),
                        changed: false,
                    });
                }
                WorkspaceLoadOutput::Loaded(loaded) => loaded,
            };
            let prepared = loaded.into_prepared();
            self.apply_workspace_switch(prepared.canonical_path.clone(), prepared)
                .map_err(|message| WorkspaceLoadError {
                    reason: super::actions::ActionError::RequiresUserInteraction,
                    message,
                })
        });
        let outcome = match &result {
            Ok(changed) => Ok(super::actions::OperationOutput::Effect {
                changed: changed.changed,
                saved: true,
            }),
            Err(error) => Err(error.reason.clone()),
        };
        if let Some(workspace) = self.workspace.as_mut() {
            workspace.operations.import_completion(task.id, outcome);
        } else {
            let status = match outcome {
                Ok(output) => super::actions::OperationStatus::Completed(output),
                Err(super::actions::ActionError::Cancelled) => {
                    super::actions::OperationStatus::Cancelled
                }
                Err(error) => super::actions::OperationStatus::Failed(error),
            };
            self.unloaded_operation = Some((task.id, status));
        }
        self.workspace_loaded = Some(result.map_err(|error| error.message));
        true
    }
}
