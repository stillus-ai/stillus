// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]

//! Typed entry point. A tool context captures a session, never a mutable selection.
use super::{
    Application,
    actions::{Action, ActionError, ActionResult, OperationOutput},
    global::JournalRequest,
    runtime::SidebarFilter,
};
use serde::Serialize;
use std::{collections::BTreeMap, path::PathBuf};
use stillus_core::{DocumentTarget, EditorCommand, SaveStatus};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct SessionId(u64);
#[derive(Clone, Copy, Debug)]
pub(crate) enum Caller {
    Ui,
    Tool(SessionId),
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct ToolContext {
    session: SessionId,
}
impl ToolContext {
    pub(crate) fn capture(app: &Application) -> Result<Self, ActionError> {
        Ok(Self {
            session: app.session_id().ok_or(ActionError::NotFound)?,
        })
    }
    pub(crate) fn caller(self) -> Caller {
        Caller::Tool(self.session)
    }
}

// Deliberately no Serialize/Deserialize: trusted commands can contain secrets.
pub(crate) enum TrustedCommand {
    OpenWorkspace(PathBuf),
    InitializeWorkspace(PathBuf),
    Unlock {
        index: usize,
        password: stillus_secure::MasterPassword,
        restore: bool,
    },
    Protect(Option<stillus_secure::MasterPassword>),
    DisableProtection,
    ChangePassword {
        current: stillus_secure::MasterPassword,
        new: stillus_secure::MasterPassword,
    },
    Integrity(stillus_core::IntegrityResolution),
    RetrySecurityRecovery,
    Connect(stillus_ai::AiSettings, zeroize::Zeroizing<String>),
}

pub(crate) enum Command {
    Chat(super::chat::Command),
    Notes(Action),
    Rss(super::rss::Addressed),
    ExternalOpen {
        path: PathBuf,
    },
    ExternalClose {
        id: String,
        version: String,
    },
    Editor {
        id: String,
        version: String,
        command: EditorCommand,
    },
    Save {
        id: String,
        version: String,
    },
    Restore {
        id: String,
        version: String,
    },
    Discard {
        id: String,
        version: String,
    },
    RebuildSearch,
    UiPreferences(super::settings::UiSettings),
    Sort {
        scope: SidebarFilter,
        field: Option<super::settings::NoteSortField>,
        direction: super::settings::SortDirection,
        version: String,
    },
    CategoryOrder {
        categories: Vec<String>,
        version: String,
    },
    Order {
        scope: SidebarFilter,
        items: Vec<String>,
        version: String,
    },
    ClearOrder {
        scope: SidebarFilter,
        version: String,
    },
    Ai {
        expected: stillus_ai::AiSettings,
        action: super::ai::Action,
    },
    Journal(JournalRequest),
    CheckUpdates,
    InstallUpdate,
    UpdateAutomatic {
        value: bool,
        version: String,
    },
    Locale {
        value: crate::i18n::Locale,
        version: String,
    },
    Lock {
        id: String,
        version: String,
    },
    DisableProtection {
        id: String,
    },
    Trusted(TrustedCommand),
}

pub(crate) enum Query {
    Chat(super::chat::Query),
    Workspace,
    Settings,
    Notes {
        offset: usize,
        limit: usize,
    },
    Read {
        id: String,
        offset: usize,
        limit: usize,
    },
    External {
        offset: usize,
        limit: usize,
    },
    Categories,
    Find {
        id: String,
        version: String,
        text: String,
        limit: usize,
    },
    Ai,
    Updates,
    Operation(String),
    OperationProgress(String),
}

#[derive(Clone, Serialize)]
#[serde(untagged)]
pub(crate) enum CommandResult {
    Action(ActionResult),
    Accepted { operation: String, saved: bool },
    Changed { changed: bool, saved: bool },
    Opened { id: String, opened: bool },
    Workspace { session: Option<SessionId> },
}
#[derive(Clone, Serialize)]
pub(crate) struct ExternalItem {
    pub id: String,
    pub path: PathBuf,
    pub title: String,
}
#[derive(Clone, Serialize)]
pub(crate) struct AiSnapshot {
    pub version: String,
    pub provider: Option<stillus_ai::AiProvider>,
    pub profiles: BTreeMap<String, stillus_ai::AiProfile>,
    pub models: Vec<stillus_ai::AiModel>,
    pub busy: bool,
}
#[derive(Clone, Serialize)]
#[serde(untagged)]
pub(crate) enum QueryResult {
    Chat(super::chat::Output),
    Pending {
        operation: String,
    },
    Action(ActionResult),
    Settings {
        version: String,
        settings: super::settings::PublicSettings,
    },
    Workspace {
        session: Option<SessionId>,
        path: Option<PathBuf>,
        saving: bool,
        security_busy: bool,
    },
    External {
        files: Vec<ExternalItem>,
    },
    Categories {
        version: String,
        categories: Vec<String>,
        sort: Vec<super::settings::CategoryNoteSortSettings>,
        order: Vec<String>,
    },
    Matches {
        matches: Vec<(usize, usize)>,
    },
    Ai(AiSnapshot),
    Operation {
        status: super::actions::OperationStatus,
        progress: super::actions::OperationProgress,
    },
    Updates {
        busy: bool,
        available: Option<String>,
        installed: bool,
    },
}

pub(super) enum Pending {
    Rss(std::sync::mpsc::Receiver<Result<super::rss::AddressedResult, ActionError>>),
    Save { id: String, revision: u64 },
    Open(DocumentTarget),
    Close(DocumentTarget),
    Search,
    Preferences(u64),
    Security,
    Ai(u64),
    Journal(u64),
    Update,
}
#[derive(Default)]
pub(crate) struct ApiState {
    pending: BTreeMap<String, Pending>,
}
impl Application {
    pub(crate) fn session_id(&self) -> Option<SessionId> {
        self.workspace
            .as_ref()
            .map(|workspace| SessionId(workspace.session_id()))
    }
    pub(crate) fn authorize(&self, caller: Caller) -> Result<(), ActionError> {
        if let Caller::Tool(expected) = caller {
            if self.session_id() != Some(expected) {
                return Err(ActionError::SessionChanged);
            }
        }
        Ok(())
    }
    fn selected_version(&mut self, id: &str, version: &str) -> Result<DocumentTarget, ActionError> {
        let workspace = self.workspace.as_mut().ok_or(ActionError::NotFound)?;
        let target = workspace.verify_read(id, version)?;
        if workspace.selected_target().as_ref() != Some(&target) {
            return Err(ActionError::RequiresUserInteraction);
        }
        Ok(target)
    }
    fn accepted(&mut self, pending: Pending) -> Result<CommandResult, ActionError> {
        let workspace = self.workspace.as_mut().ok_or(ActionError::NotFound)?;
        let id = workspace
            .operations
            .register(workspace.session_id(), false)?;
        self.api.pending.insert(id.clone(), pending);
        Ok(CommandResult::Accepted {
            operation: id,
            saved: false,
        })
    }
    pub(crate) fn ai_expected(&self, token: &str) -> Result<stillus_ai::AiSettings, ActionError> {
        self.workspace
            .as_ref()
            .ok_or(ActionError::NotFound)?
            .ai_expected(token)
    }
    pub(crate) fn dispatch(
        &mut self,
        caller: Caller,
        command: Command,
    ) -> Result<CommandResult, ActionError> {
        self.authorize(caller)?;
        self.update_action_clock();
        if matches!(caller, Caller::Tool(_))
            && matches!(
                &command,
                Command::Trusted(_)
                    | Command::UiPreferences(_)
                    | Command::Discard { .. }
                    | Command::Journal(JournalRequest { clear: true, .. })
                    | Command::Ai {
                        action: super::ai::Action::Connect(_),
                        ..
                    }
            )
        {
            return Err(ActionError::RequiresUserInteraction);
        }
        if matches!(
            &command,
            Command::Rss(_)
                | Command::Ai { .. }
                | Command::Journal(_)
                | Command::CheckUpdates
                | Command::InstallUpdate
                | Command::Save { .. }
                | Command::RebuildSearch
                | Command::Sort { .. }
                | Command::CategoryOrder { .. }
                | Command::UiPreferences(_)
        ) && self
            .workspace
            .as_ref()
            .is_some_and(|workspace| !workspace.operations.has_capacity())
        {
            return Err(ActionError::Busy);
        }
        let now = self.now_ms();
        let result = match command {
            Command::Chat(command) => return self.chat_command(caller, command),
            Command::Rss(action) => {
                let (reply, receiver) = std::sync::mpsc::sync_channel(1);
                self.workspace
                    .as_mut()
                    .ok_or(ActionError::NotFound)?
                    .send_rss(super::rss::Command::Addressed(action, reply))?;
                self.accepted(Pending::Rss(receiver))?
            }
            Command::Notes(Action::Open { id }) => {
                let workspace = self.workspace.as_ref().ok_or(ActionError::NotFound)?;
                let target = workspace.document_target(&id)?;
                // A protected buffer is never exposed even if UI has already unlocked it.
                if let DocumentTarget::WorkspaceNote(index) = target {
                    if workspace.notes()[index].protection
                        == stillus_core::NoteProtection::Protected
                    {
                        return Err(ActionError::RequiresUserInteraction);
                    }
                    self.open_note(index);
                } else {
                    self.open_external_target(target.clone());
                }
                if self.error.is_some() {
                    return Err(ActionError::Busy);
                }
                if self
                    .workspace
                    .as_ref()
                    .and_then(|workspace| workspace.selected_target())
                    != Some(target.clone())
                {
                    self.accepted(Pending::Open(target))?
                } else {
                    CommandResult::Opened { id, opened: true }
                }
            }
            Command::Notes(Action::Cancel { ref id })
                if self.workspace_load_status(id).is_some() =>
            {
                if matches!(caller, Caller::Tool(_)) {
                    return Err(ActionError::RequiresUserInteraction);
                }
                CommandResult::Action(ActionResult::Cancelled {
                    cancelled: self.cancel_workspace_load(id).unwrap_or(false),
                })
            }
            Command::Notes(Action::Cancel { ref id })
                if matches!(self.api.pending.get(id), Some(Pending::Ai(_))) =>
            {
                self.global
                    .as_ref()
                    .ok_or(ActionError::NotFound)?
                    .borrow_mut()
                    .cancel_ai();
                CommandResult::Action(ActionResult::Cancelled { cancelled: true })
            }
            Command::Notes(action) => CommandResult::Action(
                self.workspace
                    .as_mut()
                    .ok_or(ActionError::NotFound)?
                    .execute_action(action, now)?,
            ),
            Command::ExternalOpen { path } => {
                if !path.is_absolute() {
                    return Err(ActionError::InvalidArguments);
                }
                if !self.open_external_path(&path) {
                    return Err(ActionError::NotFound);
                }
                let path = path.canonicalize().map_err(|_| ActionError::NotFound)?;
                let workspace = self.workspace.as_mut().ok_or(ActionError::NotFound)?;
                let id = workspace.target_id(&path)?;
                let target = workspace.document_target(&id)?;
                if workspace.selected_target() != Some(target.clone()) {
                    self.accepted(Pending::Open(target))?
                } else {
                    CommandResult::Opened { id, opened: true }
                }
            }
            Command::ExternalClose { id, version } => {
                let target = self
                    .workspace
                    .as_mut()
                    .ok_or(ActionError::NotFound)?
                    .verify_read(&id, &version)?;
                let changed = self.close_external_target(target.clone());
                if self.pending_external_close.is_some() {
                    self.accepted(Pending::Close(target))?
                } else {
                    CommandResult::Changed {
                        changed,
                        saved: false,
                    }
                }
            }
            Command::Editor {
                id,
                version,
                command,
            } => {
                self.selected_version(&id, &version)?;
                // Clipboard is a trusted UI capability, not an assistant output channel.
                if matches!(caller, Caller::Tool(_))
                    && !matches!(command, EditorCommand::Undo | EditorCommand::Redo)
                {
                    return Err(ActionError::InvalidArguments);
                }
                self.apply(command);
                if self.error.is_some() {
                    return Err(ActionError::Busy);
                }
                CommandResult::Changed {
                    changed: true,
                    saved: false,
                }
            }
            Command::Save { id, version } => {
                self.selected_version(&id, &version)?;
                let document = self
                    .workspace
                    .as_ref()
                    .and_then(|workspace| workspace.document())
                    .ok_or(ActionError::NotFound)?;
                match document.save_status() {
                    SaveStatus::Conflict { .. } => return Err(ActionError::Conflict),
                    SaveStatus::Clean { .. } => {
                        return Ok(CommandResult::Changed {
                            changed: false,
                            saved: true,
                        });
                    }
                    _ => {}
                }
                let revision = document.content_revision();
                self.retry_save();
                self.accepted(Pending::Save { id, revision })?
            }
            Command::Restore { id, version } => {
                let workspace = self.workspace.as_ref().ok_or(ActionError::NotFound)?;
                if workspace.selected_target() != Some(workspace.document_target(&id)?) {
                    return self.dispatch(caller, Command::Notes(Action::Restore { id, version }));
                }
                self.selected_version(&id, &version)?;
                self.restore_selected_recovery()?;
                CommandResult::Changed {
                    changed: true,
                    saved: false,
                }
            }
            Command::Discard { id, version } => {
                self.selected_version(&id, &version)?;
                self.discard_local_and_reload()?;
                CommandResult::Changed {
                    changed: true,
                    saved: false,
                }
            }
            Command::RebuildSearch => {
                self.rebuild_search();
                if self.search_error.is_some() {
                    return Err(ActionError::Busy);
                }
                self.accepted(Pending::Search)?
            }
            Command::UiPreferences(settings) => self.stage_preferences(settings)?,
            Command::Sort {
                scope,
                field,
                direction,
                version,
            } => {
                if self.catalogue_version()? != version {
                    return Err(ActionError::Conflict);
                }
                let category = super::runtime::sidebar_note_order_key(&scope)
                    .ok_or(ActionError::InvalidArguments)?
                    .to_owned();
                let mut settings = self
                    .preferences
                    .as_ref()
                    .ok_or(ActionError::NotFound)?
                    .borrow()
                    .snapshot()
                    .clone();
                settings
                    .sidebar
                    .note_sort
                    .retain(|sort| sort.category != category);
                if let Some(field) = field {
                    settings
                        .sidebar
                        .note_sort
                        .push(super::settings::CategoryNoteSortSettings {
                            category,
                            field,
                            direction,
                        });
                }
                self.preferences_projection = Some(settings.sidebar.clone());
                self.stage_preferences(settings)?
            }
            Command::CategoryOrder {
                categories,
                version,
            } => {
                if self.catalogue_version()? != version {
                    return Err(ActionError::Conflict);
                }
                let actual = self
                    .workspace
                    .as_ref()
                    .ok_or(ActionError::NotFound)?
                    .categories();
                let mut seen = std::collections::BTreeSet::new();
                if categories.len() > 100
                    || categories.iter().any(|category| {
                        !actual.iter().any(|item| {
                            super::runtime::category_path_is_same_or_descendant(
                                &item.name, category,
                            )
                        }) || !seen.insert(category.clone())
                    })
                {
                    return Err(ActionError::InvalidArguments);
                }
                let mut settings = self
                    .preferences
                    .as_ref()
                    .ok_or(ActionError::NotFound)?
                    .borrow()
                    .snapshot()
                    .clone();
                settings.sidebar.category_order = categories;
                self.preferences_projection = Some(settings.sidebar.clone());
                self.stage_preferences(settings)?
            }
            Command::Order {
                scope,
                items,
                version,
            } => {
                if self.catalogue_version()? != version {
                    return Err(ActionError::Conflict);
                }
                let workspace = self.workspace.as_ref().ok_or(ActionError::NotFound)?;
                let ordered = items
                    .into_iter()
                    .map(|id| {
                        if let Some(item) = workspace
                            .non_document_items()
                            .iter()
                            .find(|item| item.item_id.as_str() == id)
                        {
                            Ok(stillus_core::CatalogOrderItem::Engine(
                                item.engine_id.clone(),
                                item.item_id.clone(),
                            ))
                        } else {
                            workspace
                                .resolve_target(&id)
                                .map(stillus_core::CatalogOrderItem::Note)
                        }
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let changed = self
                    .set_sidebar_catalog_order(&scope, &ordered)
                    .ok_or(ActionError::Busy)?;
                CommandResult::Changed {
                    changed,
                    saved: true,
                }
            }
            Command::ClearOrder { scope, version } => {
                if self.catalogue_version()? != version {
                    return Err(ActionError::Conflict);
                }
                let changed = self
                    .clear_sidebar_note_order(&scope)
                    .ok_or(ActionError::Busy)?;
                CommandResult::Changed {
                    changed,
                    saved: true,
                }
            }
            Command::Ai { expected, action } => {
                let global = self.global.as_ref().ok_or(ActionError::NotFound)?;
                let id = global
                    .borrow_mut()
                    .start_ai(expected, action)
                    .map_err(|_| ActionError::Busy)?;
                self.accepted(Pending::Ai(id))?
            }
            Command::Journal(request) => {
                let id = self
                    .global
                    .as_ref()
                    .ok_or(ActionError::NotFound)?
                    .borrow_mut()
                    .start_journal(request)
                    .map_err(|_| ActionError::Busy)?;
                self.accepted(Pending::Journal(id))?
            }
            Command::CheckUpdates | Command::InstallUpdate => {
                let global = self.global.as_ref().ok_or(ActionError::NotFound)?;
                {
                    let mut global = global.borrow_mut();
                    if global.update.stage.busy() {
                        return Err(ActionError::Busy);
                    }
                    if matches!(command, Command::InstallUpdate) {
                        global.update.install();
                    } else {
                        global.update.check(stillus_update::CheckMode::Manual);
                    }
                    if !global.update.stage.busy() {
                        return Err(ActionError::RequiresUserInteraction);
                    }
                }
                self.accepted(Pending::Update)?
            }
            Command::UpdateAutomatic { value, version } => {
                let expected = self
                    .workspace
                    .as_ref()
                    .ok_or(ActionError::NotFound)?
                    .settings_expected(&version)?;
                self.change_public_settings(super::settings::PublicEdit::Automatic {
                    expected: expected.automatic,
                    wanted: value,
                })?
            }
            Command::Locale { value, version } => {
                let expected = self
                    .workspace
                    .as_ref()
                    .ok_or(ActionError::NotFound)?
                    .settings_expected(&version)?;
                self.change_public_settings(super::settings::PublicEdit::Locale {
                    expected: expected.locale,
                    wanted: value,
                })?
            }
            Command::DisableProtection { id: _ } => {
                return Err(ActionError::RequiresUserInteraction);
            }
            Command::Lock { id, version } => {
                self.selected_version(&id, &version)?;
                self.lock_selected();
                CommandResult::Changed {
                    changed: true,
                    saved: false,
                }
            }
            Command::Trusted(command) => return self.dispatch_trusted(command),
        };
        self.state_dirty = true;
        self.poll_persistence();
        Ok(result)
    }
    pub(crate) fn query(
        &mut self,
        caller: Caller,
        query: Query,
    ) -> Result<QueryResult, ActionError> {
        self.authorize(caller)?;
        match query {
            Query::Chat(query) => return self.chat_query(caller, query),
            Query::Settings => {
                let settings = self
                    .global
                    .as_ref()
                    .ok_or(ActionError::NotFound)?
                    .borrow()
                    .public_settings();
                let version = self
                    .workspace
                    .as_mut()
                    .ok_or(ActionError::NotFound)?
                    .remember_settings(settings.clone());
                Ok(QueryResult::Settings { version, settings })
            }
            Query::Workspace => Ok(QueryResult::Workspace {
                session: self.session_id(),
                path: self
                    .workspace
                    .as_ref()
                    .map(|workspace| workspace.root().to_owned()),
                saving: self.save_worker_active,
                security_busy: self.secure_worker_active,
            }),
            Query::Notes { offset, limit } => Ok(QueryResult::Action(
                self.workspace
                    .as_mut()
                    .ok_or(ActionError::NotFound)?
                    .execute_action(Action::List { offset, limit }, 0)?,
            )),
            Query::Read { id, offset, limit } => Ok(QueryResult::Action(
                self.workspace
                    .as_mut()
                    .ok_or(ActionError::NotFound)?
                    .execute_action(Action::Read { id, offset, limit }, 0)?,
            )),
            Query::OperationProgress(id) => {
                self.poll();
                self.authorize(caller)?;
                if let Some(status) = self.workspace_load_status(&id) {
                    return Ok(QueryResult::Operation {
                        status: status.clone(),
                        progress: super::actions::OperationProgress {
                            phase: if matches!(status, super::actions::OperationStatus::Running) {
                                "workspace/loading"
                            } else {
                                "completed"
                            }
                            .into(),
                            ..Default::default()
                        },
                    });
                }
                let (status, progress) = self
                    .workspace
                    .as_ref()
                    .ok_or(ActionError::NotFound)?
                    .operations
                    .progress(&id)?;
                Ok(QueryResult::Operation { status, progress })
            }
            Query::Operation(id) => {
                self.poll();
                self.authorize(caller)?;
                if let Some(status) = self.workspace_load_status(&id) {
                    return Ok(QueryResult::Action(ActionResult::Status(status)));
                }
                Ok(QueryResult::Action(
                    self.workspace
                        .as_mut()
                        .ok_or(ActionError::NotFound)?
                        .execute_action(Action::Status { id }, 0)?,
                ))
            }
            Query::External { offset, limit } => {
                let workspace = self.workspace.as_mut().ok_or(ActionError::NotFound)?;
                let files = workspace
                    .external_files()
                    .iter()
                    .skip(offset)
                    .take(limit.min(100))
                    .cloned()
                    .collect::<Vec<_>>();
                Ok(QueryResult::External {
                    files: files
                        .into_iter()
                        .map(|file| {
                            Ok(ExternalItem {
                                id: workspace.target_id(&file.path)?,
                                path: file.path,
                                title: file.title,
                            })
                        })
                        .collect::<Result<Vec<_>, ActionError>>()?,
                })
            }
            Query::Categories => Ok(QueryResult::Categories {
                version: self.catalogue_version()?,
                sort: self
                    .preferences
                    .as_ref()
                    .map(|preferences| preferences.borrow().snapshot().sidebar.note_sort.clone())
                    .unwrap_or_default(),
                order: self
                    .preferences
                    .as_ref()
                    .map(|preferences| {
                        preferences
                            .borrow()
                            .snapshot()
                            .sidebar
                            .category_order
                            .clone()
                    })
                    .unwrap_or_default(),
                categories: self
                    .workspace
                    .as_ref()
                    .ok_or(ActionError::NotFound)?
                    .categories()
                    .iter()
                    .take(100)
                    .map(|category| category.name.clone())
                    .collect(),
            }),
            Query::Find {
                id,
                version,
                text,
                limit,
            } => {
                self.selected_version(&id, &version)?;
                if text.len() > 4096 {
                    return Err(ActionError::InvalidArguments);
                }
                let matches = self
                    .workspace
                    .as_ref()
                    .ok_or(ActionError::NotFound)?
                    .search_selected_document(&text, limit.min(100))?;
                Ok(QueryResult::Matches {
                    matches: matches
                        .into_iter()
                        .map(|range| (range.start().get(), range.end().get()))
                        .collect(),
                })
            }
            Query::Ai => {
                let global = self.global.as_ref().ok_or(ActionError::NotFound)?.borrow();
                let settings = global.ai();
                let version = self
                    .workspace
                    .as_mut()
                    .ok_or(ActionError::NotFound)?
                    .remember_ai(settings.clone());
                Ok(QueryResult::Ai(AiSnapshot {
                    version,
                    provider: settings
                        .connection
                        .as_ref()
                        .map(|connection| connection.provider.clone()),
                    models: settings
                        .connection
                        .as_ref()
                        .map(|connection| connection.models.clone())
                        .unwrap_or_default(),
                    profiles: settings.aliases,
                    busy: global.ai_busy(),
                }))
            }
            Query::Updates => {
                let global = self.global.as_ref().ok_or(ActionError::NotFound)?.borrow();
                Ok(QueryResult::Updates {
                    busy: global.update.stage.busy(),
                    available: global
                        .update
                        .stage
                        .release()
                        .map(|release| release.version.to_string()),
                    installed: matches!(global.update.stage, super::updates::Stage::Installed(_)),
                })
            }
        }
    }
    fn catalogue_version(&self) -> Result<String, ActionError> {
        use std::hash::{Hash, Hasher};
        let workspace = self.workspace.as_ref().ok_or(ActionError::NotFound)?;
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        for note in workspace.notes() {
            note.path.hash(&mut hash);
            note.title.hash(&mut hash);
            note.tags.hash(&mut hash);
            note.order.hash(&mut hash);
            note.pinned.hash(&mut hash);
            note.favorited.hash(&mut hash);
            note.deleted.hash(&mut hash);
        }
        if let Some(preferences) = &self.preferences {
            let preferences = preferences.borrow();
            preferences
                .snapshot()
                .sidebar
                .category_order
                .hash(&mut hash);
            for sort in &preferences.snapshot().sidebar.note_sort {
                sort.category.hash(&mut hash);
                (sort.field as u8).hash(&mut hash);
                (sort.direction as u8).hash(&mut hash);
            }
        }
        for item in workspace.non_document_items() {
            item.engine_id.hash(&mut hash);
            item.item_id.hash(&mut hash);
            item.metadata_version.hash(&mut hash);
            item.metadata.order.hash(&mut hash);
        }
        Ok(format!(
            "catalogue/{:x}/{:x}",
            workspace.session_id(),
            hash.finish()
        ))
    }
    fn change_public_settings(
        &mut self,
        edit: super::settings::PublicEdit,
    ) -> Result<CommandResult, ActionError> {
        self.global
            .as_ref()
            .ok_or(ActionError::NotFound)?
            .borrow_mut()
            .change_public(edit)
            .map_err(|error| match error {
                super::settings::SettingsError::Conflict => ActionError::Conflict,
                _ => ActionError::Failed("settings save failed".into()),
            })?;
        Ok(CommandResult::Changed {
            changed: true,
            saved: true,
        })
    }
    fn stage_preferences(
        &mut self,
        settings: super::settings::UiSettings,
    ) -> Result<CommandResult, ActionError> {
        let revision = {
            let mut preferences = self
                .preferences
                .as_ref()
                .ok_or(ActionError::NotFound)?
                .borrow_mut();
            if !preferences.stage(settings) {
                return Ok(CommandResult::Changed {
                    changed: false,
                    saved: preferences
                        .completion(preferences.revision())
                        .is_some_and(|result| result.is_ok()),
                });
            }
            preferences.revision()
        };
        self.accepted(Pending::Preferences(revision))
    }
    fn dispatch_trusted(&mut self, command: TrustedCommand) -> Result<CommandResult, ActionError> {
        let initialize = matches!(command, TrustedCommand::InitializeWorkspace(_));
        match command {
            TrustedCommand::OpenWorkspace(path) | TrustedCommand::InitializeWorkspace(path) => {
                let operation = self
                    .begin_workspace_switch(&path, initialize)
                    .map_err(|error| error.reason)?;
                Ok(CommandResult::Accepted {
                    operation,
                    saved: false,
                })
            }
            TrustedCommand::Unlock {
                index,
                password,
                restore,
            } => {
                self.unlock_note(index, password, restore);
                self.accepted(Pending::Security)
            }
            TrustedCommand::Protect(password) => {
                self.protect_selected(password);
                self.accepted(Pending::Security)
            }
            TrustedCommand::DisableProtection => {
                self.disable_protection_selected();
                self.accepted(Pending::Security)
            }
            TrustedCommand::ChangePassword { current, new } => {
                if !self.request_master_password_change(current, new) {
                    return Err(ActionError::Busy);
                }
                self.accepted(Pending::Security)
            }
            TrustedCommand::Integrity(resolution) => {
                if !self.start_integrity_resolution(resolution) {
                    return Err(ActionError::Busy);
                }
                self.accepted(Pending::Security)
            }
            TrustedCommand::RetrySecurityRecovery => Ok(CommandResult::Changed {
                changed: self.retry_password_change_recovery(),
                saved: false,
            }),
            TrustedCommand::Connect(expected, key) => self.dispatch(
                Caller::Ui,
                Command::Ai {
                    expected,
                    action: super::ai::Action::Connect(key),
                },
            ),
        }
    }
    pub(super) fn poll_api(&mut self) -> bool {
        let mut completed = Vec::new();
        let mut progress_changed = false;
        for (id, pending) in &self.api.pending {
            use super::actions::OperationProgress;
            let progress =
                match pending {
                    Pending::Security => {
                        self.secure_progress
                            .as_ref()
                            .map(|progress| OperationProgress {
                                phase: format!("{:?}", progress.phase),
                                completed: progress.completed as u64,
                                total: Some(progress.total as u64),
                            })
                    }
                    Pending::Update => self.global.as_ref().and_then(|global| {
                        match &global.borrow().update.stage {
                            super::updates::Stage::Downloading {
                                received, total, ..
                            } => Some(OperationProgress {
                                phase: "downloading".into(),
                                completed: *received,
                                total: *total,
                            }),
                            _ => None,
                        }
                    }),
                    _ => None,
                };
            if let (Some(progress), Some(workspace)) = (progress, self.workspace.as_mut()) {
                progress_changed |= workspace.operations.update_progress(id, progress);
            }
            let result = match pending {
                Pending::Rss(receiver) => match receiver.try_recv() {
                    Ok(result) => Some(result.map(OperationOutput::Rss)),
                    Err(std::sync::mpsc::TryRecvError::Empty) => None,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        Some(Err(ActionError::Cancelled))
                    }
                },
                Pending::Save { id, revision } => self
                    .workspace
                    .as_ref()
                    .and_then(|workspace| {
                        let target = workspace.document_target(id).ok()?;
                        workspace
                            .document()
                            .filter(|document| document.target() == &target)
                    })
                    .and_then(|document| match document.save_status() {
                        SaveStatus::Clean {
                            revision: saved, ..
                        } if saved >= *revision => Some(Ok(OperationOutput::Effect {
                            changed: true,
                            saved: true,
                        })),
                        SaveStatus::Conflict { .. } => Some(Err(ActionError::Conflict)),
                        SaveStatus::Error { .. } => {
                            Some(Err(ActionError::Failed("save failed".into())))
                        }
                        _ => None,
                    }),
                Pending::Close(target) => self
                    .workspace
                    .as_ref()
                    .filter(|workspace| match target {
                        DocumentTarget::ExternalFile { engine_id, item_id } => !workspace
                            .external_files()
                            .iter()
                            .any(|file| file.engine_id == *engine_id && file.item_id == *item_id),
                        _ => true,
                    })
                    .map(|_| {
                        Ok(OperationOutput::Effect {
                            changed: true,
                            saved: false,
                        })
                    }),
                Pending::Search => (!self.search_indexing
                    && (self.search_ready || self.search_error.is_some()))
                .then_some(if self.search_error.is_some() {
                    Err(ActionError::Failed("search failed".into()))
                } else {
                    Ok(OperationOutput::Effect {
                        changed: true,
                        saved: true,
                    })
                }),
                Pending::Open(target) => self
                    .workspace
                    .as_ref()
                    .filter(|workspace| workspace.selected_target().as_ref() == Some(target))
                    .map(|_| {
                        Ok(OperationOutput::Effect {
                            changed: true,
                            saved: false,
                        })
                    }),
                Pending::Preferences(revision) => self
                    .preferences
                    .as_ref()
                    .and_then(|preferences| preferences.borrow().completion(*revision))
                    .map(|result| {
                        result.map(|()| OperationOutput::Effect {
                            changed: true,
                            saved: true,
                        })
                    }),
                Pending::Security => (!self.secure_worker_active
                    && self.pending_security_action.is_none()
                    && self.pending_password_change.is_none()
                    && self.search_security_operation.is_none())
                .then_some(
                    if self.error.is_some() || self.password_change_error.is_some() {
                        Err(ActionError::RequiresUserInteraction)
                    } else {
                        Ok(OperationOutput::Effect {
                            changed: true,
                            saved: true,
                        })
                    },
                ),
                Pending::Ai(operation) => self
                    .global
                    .as_ref()
                    .and_then(|global| global.borrow().ai_result(*operation))
                    .map(|result| {
                        result
                            .map(|_| OperationOutput::Effect {
                                changed: true,
                                saved: true,
                            })
                            .map_err(|error| match error {
                                super::ai::Failure::Cancelled => ActionError::Cancelled,
                                super::ai::Failure::Settings => ActionError::Conflict,
                                _ => ActionError::Failed("AI request failed".into()),
                            })
                    }),
                Pending::Journal(operation) => self
                    .global
                    .as_ref()
                    .and_then(|global| global.borrow().journal_result(*operation))
                    .map(|result| {
                        result
                            .map(|page| OperationOutput::Journal {
                                rows: page.rows,
                                detail: page.detail,
                                blocked: page.blocked,
                            })
                            .map_err(|_| ActionError::Failed("journal request failed".into()))
                    }),
                Pending::Update => self.global.as_ref().and_then(|global| {
                    let global = global.borrow();
                    (!global.update.stage.busy()).then_some(match &global.update.stage {
                        super::updates::Stage::Failed(_)
                        | super::updates::Stage::Unsupported(_) => {
                            Err(ActionError::Failed("update failed".into()))
                        }
                        _ => Ok(OperationOutput::Effect {
                            changed: true,
                            saved: matches!(
                                global.update.stage,
                                super::updates::Stage::Installed(_)
                            ),
                        }),
                    })
                }),
            };
            if let Some(result) = result {
                completed.push((id.clone(), result));
            }
        }
        let changed = progress_changed || !completed.is_empty();
        for (id, result) in completed {
            self.api.pending.remove(&id);
            if let Some(workspace) = self.workspace.as_mut() {
                workspace.operations.finish(&id, result);
            }
        }
        changed
    }
}
