// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]
//! Owner-thread coordinator. File and provider workers exchange bounded messages;
//! only Application executes tools and applies completions to the current session.
use super::{
    Application,
    actions::{ActionError, OperationOutput, OperationStatus},
    api::{Caller, ToolContext},
    tools,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::Duration,
};
use stillus_ai::{AiProvider, ApiKey, journal::now_ms, provider::*};
use stillus_chat::*;
use stillus_engine::{CommonMetadataPatch, ItemId};
use stillus_platform::credentials::CredentialStore;

pub(crate) enum Command {
    Create {
        title: String,
        categories: Vec<String>,
        favorited: bool,
        open: bool,
    },
    Open {
        id: String,
    },
    Metadata {
        id: String,
        version: String,
        patch: CommonMetadataPatch,
        alias: Option<String>,
    },
    #[allow(dead_code)] // Immediate, versioned draft persistence for trusted API callers.
    Draft {
        id: String,
        version: String,
        text: String,
    },
    Compose {
        id: String,
        version: String,
        text: String,
    },
    Send {
        id: String,
        version: String,
    },
    Stop {
        id: String,
    },
    Continue {
        id: String,
    },
    Seen {
        id: String,
    },
    Acknowledge {
        id: String,
        message: String,
        version: String,
    },
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum GenerationReadiness {
    Ready,
    Disconnected,
    Unavailable,
    Unsupported,
}
pub(crate) enum Query {
    List {
        offset: usize,
        limit: usize,
    },
    Read {
        id: String,
        before: Option<String>,
        limit: usize,
    },
    State {
        id: String,
    },
}
#[derive(Clone, Serialize)]
pub(crate) struct ChatView {
    pub id: String,
    #[serde(skip)]
    pub for_display: bool,
    pub before: Option<String>,
    pub diagnostics: Vec<String>,
    pub metadata: Versioned<Metadata>,
    pub draft: Versioned<Draft>,
    pub history: HistoryPage,
    pub run: Option<Versioned<Run>>,
}
#[derive(Clone, Serialize)]
#[serde(untagged)]
pub(crate) enum Output {
    List {
        chats: Vec<ChatSnapshot>,
    },
    View(ChatView),
    Metadata {
        id: String,
        metadata: Versioned<Metadata>,
    },
    Draft {
        id: String,
        draft: Versioned<Draft>,
    },
    Created {
        id: String,
    },
    State {
        run: Option<Run>,
    },
}
impl From<ChatError> for ActionError {
    fn from(e: ChatError) -> Self {
        match e {
            ChatError::Conflict => Self::Conflict,
            ChatError::Missing => Self::NotFound,
            _ => Self::Failed(format!("chat: {e:?}")),
        }
    }
}
fn ai_error(e: stillus_ai::AiError) -> ActionError {
    match e {
        stillus_ai::AiError::Cancelled => ActionError::Cancelled,
        stillus_ai::AiError::Incomplete | stillus_ai::AiError::Unsupported => {
            ActionError::RequiresUserInteraction
        }
        _ => ActionError::Failed(format!("AI: {e:?}")),
    }
}
#[derive(Clone)]
struct Start {
    id: ItemId,
    run: Versioned<Run>,
    credential: String,
    effort: Option<stillus_ai::AiEffort>,
}
struct DraftBuffer {
    version: String,
    text: String,
    due: u64,
    pending: Option<String>,
    failed: bool,
}
struct IoCompletion {
    operation: String,
    result: Result<Output, ActionError>,
    items: Option<Result<Vec<ChatSnapshot>, ActionError>>,
    start: Option<Start>,
    open: Option<ItemId>,
}
enum WorkerUpdate {
    Message(Message),
    Run(Run),
}
struct Active {
    start: Start,
    cancel: Cancellation,
    worker: thread::JoinHandle<()>,
    completion: Receiver<Result<Versioned<Run>, ActionError>>,
    progress: Arc<Mutex<Option<(String, String)>>>,
    updates: Receiver<WorkerUpdate>,
    operation: String,
}
struct ToolRequest {
    context: ToolContext,
    call: ToolInvocation,
    reply: SyncSender<Result<Value, ActionError>>,
    cancel: Cancellation,
}
struct WaitingTool {
    operation: String,
    reply: SyncSender<Result<Value, ActionError>>,
}
pub(crate) struct Coordinator {
    store: ChatStore,
    pub(crate) items: Vec<ChatSnapshot>,
    pub(crate) view: Option<ChatView>,
    io_send: SyncSender<IoCompletion>,
    io_receive: Receiver<IoCompletion>,
    io: usize,
    drafts: BTreeMap<ItemId, DraftBuffer>,
    pending_send: BTreeMap<ItemId, String>,
    deferred_metadata: BTreeMap<String, (ItemId, String, CommonMetadataPatch, Option<String>)>,
    cancelled_preparing: std::collections::BTreeSet<ItemId>,
    queue: VecDeque<Start>,
    active: BTreeMap<String, Active>,
    preparing: BTreeMap<String, ItemId>,
    tools_send: SyncSender<ToolRequest>,
    tools_receive: Receiver<ToolRequest>,
    waiting: Vec<WaitingTool>,
    provider_registry: ProviderRegistry,
    credentials: Arc<dyn CredentialStore>,
    pending_open: Option<ItemId>,
    refresh: Option<(ItemId, Option<String>)>,
    stopping: bool,
}
impl Coordinator {
    fn new(root: PathBuf, items: Vec<stillus_engine::ItemSummary>) -> Result<Self, ActionError> {
        let store = ChatStore::open(&root)?;
        let (io_send, io_receive) = mpsc::sync_channel(8);
        let (tools_send, tools_receive) = mpsc::sync_channel(2);
        Ok(Self {
            store,
            items: items
                .into_iter()
                .filter(|i| i.engine_id == engine_id())
                .map(|item| ChatSnapshot {
                    item,
                    metadata: None,
                    run: None,
                })
                .collect(),
            view: None,
            io_send,
            io_receive,
            io: 0,
            drafts: BTreeMap::new(),
            pending_send: BTreeMap::new(),
            deferred_metadata: BTreeMap::new(),
            cancelled_preparing: Default::default(),
            queue: VecDeque::new(),
            active: BTreeMap::new(),
            preparing: BTreeMap::new(),
            tools_send,
            tools_receive,
            waiting: Vec::new(),
            provider_registry: registry(),
            credentials: credentials(),
            pending_open: None,
            refresh: None,
            stopping: false,
        })
    }
    fn running(&self, id: &ItemId) -> bool {
        self.pending_send.contains_key(id)
            || self.preparing.values().any(|i| i == id)
            || self.queue.iter().any(|s| &s.id == id)
            || self.active.values().any(|a| &a.start.id == id)
    }
    pub(crate) fn busy(&self) -> bool {
        self.io > 0
            || !self.deferred_metadata.is_empty()
            || self.drafts.values().any(|d| !d.failed)
            || !self.queue.is_empty()
            || !self.active.is_empty()
    }
    fn stop(&mut self, id: Option<&ItemId>) {
        self.pending_send
            .retain(|target, _| id.is_some_and(|id| id != target));
        for target in self.preparing.values() {
            if id.is_none_or(|id| id == target) {
                self.cancelled_preparing.insert(target.clone());
            }
        }
        for start in &mut self.queue {
            if id.is_none_or(|id| id == &start.id) {
                start.run.value.status = RunStatus::Stopped;
            }
        }
        for active in self.active.values() {
            if id.is_none_or(|id| id == &active.start.id) {
                active.cancel.cancel();
            }
        }
        if id.is_none() {
            self.stopping = true;
        }
    }
}
impl Application {
    pub(super) fn chat_coordinator(&mut self) -> Result<&mut Coordinator, ActionError> {
        if self.chats.is_none() {
            let w = self.workspace.as_ref().ok_or(ActionError::NotFound)?;
            self.chats = Some(Coordinator::new(
                w.root().to_owned(),
                w.non_document_items(),
            )?);
        }
        Ok(self.chats.as_mut().expect("initialized"))
    }
    pub(crate) fn restore_chat_selection(&mut self, id: Option<&str>) {
        if let Some(id) = id.and_then(|id| ItemId::new(id).ok()) {
            if let Some(w) = self.workspace.as_mut() {
                let _ = w.open_engine_item(&engine_id(), &id);
            }
        }
    }
    pub(crate) fn sync_chat_catalog(&mut self) {
        if let (Some(w), Some(c)) = (self.workspace.as_ref(), self.chats.as_mut()) {
            for item in w
                .non_document_items()
                .into_iter()
                .filter(|i| i.engine_id == engine_id())
            {
                if let Some(snapshot) = c.items.iter_mut().find(|s| s.item.item_id == item.item_id)
                {
                    snapshot.item = item;
                }
            }
        }
    }
    pub(crate) fn chat_view(&self) -> Option<&ChatView> {
        self.chats.as_ref().and_then(|c| c.view.as_ref())
    }
    pub(crate) fn chat_generation_readiness(&self, id: &ItemId) -> GenerationReadiness {
        let Some(global) = self.global.as_ref() else {
            return GenerationReadiness::Disconnected;
        };
        let settings = global.borrow().ai();
        let Some(connection) = settings.connection.as_ref() else {
            return GenerationReadiness::Disconnected;
        };
        let Some(c) = self.chats.as_ref() else {
            return GenerationReadiness::Unavailable;
        };
        let alias = c
            .view
            .as_ref()
            .filter(|v| v.id == id.as_str())
            .map(|v| v.metadata.value.alias.as_str())
            .unwrap_or("default");
        let Ok(profile) = settings.resolve(alias) else {
            return GenerationReadiness::Unavailable;
        };
        let Some(provider) = c.provider_registry.get(&connection.provider) else {
            return GenerationReadiness::Unsupported;
        };
        match provider.capabilities(&profile.model) {
            Ok(c) if c.generation => GenerationReadiness::Ready,
            Ok(_) => GenerationReadiness::Unsupported,
            Err(_) => GenerationReadiness::Unavailable,
        }
    }
    pub(crate) fn chat_running(&self, id: &ItemId) -> bool {
        self.chats.as_ref().is_some_and(|c| c.running(id))
    }
    pub(crate) fn chat_draft_pending(&self, id: &ItemId) -> bool {
        self.chats
            .as_ref()
            .is_some_and(|c| c.drafts.contains_key(id))
    }
    pub(crate) fn chat_items(&self) -> Vec<ChatSnapshot> {
        self.chats
            .as_ref()
            .map(|c| c.items.clone())
            .unwrap_or_default()
    }
    fn chat_id(&self, id: String) -> Result<ItemId, ActionError> {
        ItemId::new(id).map_err(|_| ActionError::InvalidArguments)
    }
    fn chat_io(
        &mut self,
        writing: bool,
        job: impl FnOnce(ChatStore) -> (Result<Output, ActionError>, Option<Start>, Option<ItemId>)
        + Send
        + 'static,
    ) -> Result<String, ActionError> {
        self.chat_coordinator()?;
        let w = self.workspace.as_mut().ok_or(ActionError::NotFound)?;
        let operation = w.operations.register(w.session_id(), writing)?;
        self.launch_chat_io(operation.clone(), writing, job);
        Ok(operation)
    }
    fn launch_chat_io(
        &mut self,
        operation: String,
        writing: bool,
        job: impl FnOnce(ChatStore) -> (Result<Output, ActionError>, Option<Start>, Option<ItemId>)
        + Send
        + 'static,
    ) {
        let c = self.chats.as_mut().expect("initialized");
        let sender = c.io_send.clone();
        let store = c.store.clone();
        let id = operation.clone();
        c.io += 1;
        thread::spawn(move || {
            let (result, start, open) =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(store.clone())))
                    .unwrap_or_else(|_| {
                        (
                            Err(ActionError::Failed("chat file worker stopped".into())),
                            None,
                            None,
                        )
                    });
            let items = writing.then(|| store.list().map_err(ActionError::from));
            let _ = sender.send(IoCompletion {
                operation: id,
                result,
                items,
                start,
                open,
            });
        });
    }
    pub(crate) fn chat_command(
        &mut self,
        caller: Caller,
        command: Command,
    ) -> Result<super::api::CommandResult, ActionError> {
        self.authorize(caller)?;
        if matches!(caller, Caller::Tool(_))
            && matches!(
                &command,
                Command::Open { .. }
                    | Command::Send { .. }
                    | Command::Stop { .. }
                    | Command::Continue { .. }
                    | Command::Draft { .. }
                    | Command::Compose { .. }
                    | Command::Seen { .. }
                    | Command::Acknowledge { .. }
            )
        {
            return Err(ActionError::RequiresUserInteraction);
        }
        let accepted = |operation| super::api::CommandResult::Accepted {
            operation,
            saved: false,
        };
        match command {
            Command::Create {
                title,
                categories,
                favorited,
                open,
            } => {
                let categories = stillus_core::normalize_rss_categories(&categories)?;
                let timestamp = stillus_core::format_utc_timestamp(std::time::SystemTime::now())?;
                let operation = self.chat_io(true, move |store| {
                    let result = store
                        .create_chat(Metadata {
                            common: stillus_engine::CommonMetadata {
                                title,
                                categories,
                                favorited,
                                created: Some(timestamp.clone()),
                                modified: Some(timestamp),
                                ..Default::default()
                            },
                            automatic_title: true,
                            alias: "default".into(),
                        })
                        .map_err(ActionError::from);
                    match result {
                        Ok(id) => (
                            Ok(Output::Created { id: id.to_string() }),
                            None,
                            open.then_some(id),
                        ),
                        Err(e) => (Err(e), None, None),
                    }
                })?;
                Ok(accepted(operation))
            }
            Command::Open { id } => {
                let id = self.chat_id(id)?;
                self.chat_coordinator()?.pending_open = Some(id);
                self.state_dirty = true;
                Ok(super::api::CommandResult::Changed {
                    changed: true,
                    saved: false,
                })
            }
            Command::Metadata {
                id,
                version,
                mut patch,
                alias,
            } => {
                if let Some(categories) = &patch.categories {
                    patch.categories = Some(stillus_core::normalize_rss_categories(categories)?);
                }
                let id = self.chat_id(id)?;
                if patch.deleted == Some(true) && self.chat_running(&id) {
                    // A task cannot wait for its own termination, or another task waiting on it.
                    if matches!(caller, Caller::Tool(_)) {
                        return Err(ActionError::RequiresUserInteraction);
                    }
                    let w = self.workspace.as_mut().ok_or(ActionError::NotFound)?;
                    let operation = w.operations.register(w.session_id(), false)?;
                    let c = self.chat_coordinator()?;
                    c.stop(Some(&id));
                    c.deferred_metadata
                        .insert(operation.clone(), (id, version, patch, alias));
                    return Ok(accepted(operation));
                }
                let operation = self.chat_io(true, move |store| {
                    (
                        store
                            .patch_metadata(&id, &version, patch, alias, true)
                            .map(|metadata| Output::Metadata {
                                id: id.to_string(),
                                metadata,
                            })
                            .map_err(Into::into),
                        None,
                        None,
                    )
                })?;
                Ok(accepted(operation))
            }
            Command::Compose { id, version, text } => {
                if text.len() > 65536 {
                    return Err(ActionError::InvalidArguments);
                }
                let id = self.chat_id(id)?;
                let due = self.now_ms().saturating_add(250);
                let c = self.chat_coordinator()?;
                if let Some(draft) = c.drafts.get_mut(&id) {
                    if draft.failed {
                        return Err(ActionError::Conflict);
                    }
                    draft.text = text;
                    draft.due = due;
                } else {
                    let view = c
                        .view
                        .as_ref()
                        .filter(|v| v.id == id.as_str())
                        .ok_or(ActionError::NotFound)?;
                    if view.draft.revision != version {
                        return Err(ActionError::Conflict);
                    }
                    c.drafts.insert(
                        id,
                        DraftBuffer {
                            version,
                            text,
                            due,
                            pending: None,
                            failed: false,
                        },
                    );
                }
                Ok(super::api::CommandResult::Changed {
                    changed: true,
                    saved: false,
                })
            }
            Command::Draft { id, version, text } => {
                if text.len() > 65536 {
                    return Err(ActionError::InvalidArguments);
                }
                let id = self.chat_id(id)?;
                let operation = self.chat_io(false, move |store| {
                    (
                        store
                            .save_draft(&id, &version, &Draft { text })
                            .map(|draft| Output::Draft {
                                id: id.to_string(),
                                draft,
                            })
                            .map_err(Into::into),
                        None,
                        None,
                    )
                })?;
                Ok(accepted(operation))
            }
            Command::Stop { id } => {
                let id = self.chat_id(id)?;
                let c = self.chat_coordinator()?;
                c.stop(Some(&id));
                if let Some(pos) = c.queue.iter().position(|s| s.id == id) {
                    let start = c.queue.remove(pos).expect("queued");
                    let op = self.chat_io(false, move |store| {
                        let mut run = start.run.value;
                        run.status = RunStatus::Stopped;
                        (
                            store
                                .save_run(&id, Some(&start.run.revision), &run)
                                .map(|r| Output::State { run: Some(r.value) })
                                .map_err(Into::into),
                            None,
                            None,
                        )
                    })?;
                    return Ok(accepted(op));
                }
                Ok(super::api::CommandResult::Changed {
                    changed: true,
                    saved: false,
                })
            }
            Command::Acknowledge {
                id,
                message,
                version,
            } => {
                let id = self.chat_id(id)?;
                if self.chat_running(&id) {
                    return Err(ActionError::Busy);
                }
                let operation = self.chat_io(false, move |store| {
                    let result = (|| {
                        let mut cursor = None;
                        loop {
                            let page = store.history(&id,cursor.as_deref(),64)?;
                            if let Some(record) = page.entries.into_iter().find(|e|e.id == message).and_then(|e|e.message) {
                                if record.revision != version { return Err(ActionError::Conflict); }
                                let mut value = record.value;
                                let tool = value.tool.as_mut().ok_or(ActionError::InvalidArguments)?;
                                if tool.state != ToolState::Unknown { return Err(ActionError::Conflict); }
                                tool.state = ToolState::Failed;
                                tool.result = Some(json!({"outcome":"unknown","user_acknowledged":true,"instruction":"The user chose to continue without repeating this action. Its effect remains unconfirmed."}));
                                store.save_message(&id,Some(&version),&value)?;
                                return read_view(&store,&id,None,32).map(Output::View);
                            }
                            if page.next.is_none() { return Err(ActionError::NotFound); }
                            cursor = page.next;
                        }
                    })();
                    (result,None,None)
                })?;
                Ok(accepted(operation))
            }
            Command::Seen { id } => {
                let id = self.chat_id(id)?;
                let c = self.chat_coordinator()?;
                if c.running(&id) {
                    return Ok(super::api::CommandResult::Changed {
                        changed: false,
                        saved: false,
                    });
                }
                let operation = self.chat_io(false, move |store| {
                    let result = (|| {
                        let Some(mut run) = store.run(&id)? else {
                            return Ok(Output::State { run: None });
                        };
                        run.value.unread = false;
                        let run = store.save_run(&id, Some(&run.revision), &run.value)?;
                        Ok(Output::State {
                            run: Some(run.value),
                        })
                    })();
                    (result, None, None)
                })?;
                Ok(accepted(operation))
            }
            Command::Send { id, version } => {
                let target = self.chat_id(id.clone())?;
                let c = self.chat_coordinator()?;
                if c.drafts.contains_key(&target) {
                    if c.running(&target)
                        || c.active.len() + c.queue.len() + c.preparing.len() + c.pending_send.len()
                            >= 8
                    {
                        return Err(ActionError::Busy);
                    }
                    let draft = c.drafts.get_mut(&target).expect("pending draft");
                    if draft.failed || draft.version != version {
                        return Err(ActionError::Conflict);
                    }
                    draft.due = 0;
                    c.pending_send.insert(target, version);
                    return Ok(super::api::CommandResult::Changed {
                        changed: true,
                        saved: false,
                    });
                }
                self.enqueue_chat(id, Some(version), false).map(accepted)
            }
            Command::Continue { id } => self.enqueue_chat(id, None, true).map(accepted),
        }
    }
    fn enqueue_chat(
        &mut self,
        id: String,
        version: Option<String>,
        resume: bool,
    ) -> Result<String, ActionError> {
        let id = self.chat_id(id)?;
        let c = self.chat_coordinator()?;
        if c.stopping
            || c.drafts.contains_key(&id)
            || c.running(&id)
            || c.active.len() + c.queue.len() + c.preparing.len() + c.pending_send.len() >= 8
        {
            return Err(ActionError::Busy);
        }
        let settings = self
            .global
            .as_ref()
            .ok_or(ActionError::NotFound)?
            .borrow()
            .ai();
        if settings.connection.is_none() {
            return Err(ActionError::RequiresUserInteraction);
        }
        let prepared_id = id.clone();
        let registry = self
            .chats
            .as_ref()
            .expect("initialized")
            .provider_registry
            .clone();
        let operation = self.chat_io(true, move |store| {
            let result = (|| -> Result<Start, ActionError> {
                let metadata = store.metadata(&id)?;
                if metadata.value.common.deleted {
                    return Err(ActionError::NotFound);
                }
                let profile = settings
                    .resolve(&metadata.value.alias)
                    .map_err(ai_error)?
                    .clone();
                let connection = settings
                    .connection
                    .as_ref()
                    .ok_or(ActionError::RequiresUserInteraction)?;
                let provider = registry
                    .get(&connection.provider)
                    .ok_or(ActionError::RequiresUserInteraction)?;
                if !provider
                    .capabilities(&profile.model)
                    .map_err(ai_error)?
                    .generation
                {
                    return Err(ActionError::RequiresUserInteraction);
                }
                let previous = store.run(&id)?;
                let run = if resume {
                    let previous = previous.as_ref().ok_or(ActionError::NotFound)?;
                    if previous.value.status != RunStatus::Paused
                        || previous.value.provider != connection.provider.id()
                    {
                        return Err(ActionError::RequiresUserInteraction);
                    }
                    provider
                        .capabilities(&previous.value.model)
                        .map_err(ai_error)?;
                    let mut run = previous.value.clone();
                    run.status = RunStatus::Queued;
                    run.request_limit = run.requests.saturating_add(20);
                    run.tool_limit = run.tools.saturating_add(50);
                    run.error = None;
                    run
                } else {
                    let draft = store.draft(&id)?;
                    if Some(draft.revision.as_str()) != version.as_deref() {
                        return Err(ActionError::Conflict);
                    }
                    if draft.value.text.trim().is_empty() {
                        return Err(ActionError::InvalidArguments);
                    }
                    let run = Run {
                        id: new_id(),
                        status: RunStatus::Queued,
                        provider: connection.provider.id().into(),
                        model: profile.model.clone(),
                        parameters: json!({"effort":profile.effort}),
                        requests: 0,
                        tools: 0,
                        request_limit: 20,
                        tool_limit: 50,
                        error: None,
                        unread: false,
                        pending_calls: Vec::new(),
                        pending_request: None,
                        awaiting_model: true,
                    };
                    store.save_message(
                        &id,
                        None,
                        &Message {
                            id: new_id(),
                            run: run.id.clone(),
                            role: Role::User,
                            text: draft.value.text.clone(),
                            delivery: Delivery::Complete,
                            created_ms: now_ms(),
                            tool: None,
                            provider_state: None,
                            request_id: None,
                        },
                    )?;
                    store.save_draft(&id, &draft.revision, &Draft::default())?;
                    if metadata.value.automatic_title
                        && draft
                            .value
                            .text
                            .lines()
                            .next()
                            .is_some_and(|line| !line.trim().is_empty())
                    {
                        let title = draft
                            .value
                            .text
                            .lines()
                            .next()
                            .unwrap_or("")
                            .chars()
                            .take(64)
                            .collect::<String>();
                        store.patch_metadata(
                            &id,
                            &metadata.revision,
                            CommonMetadataPatch {
                                title: Some(title),
                                ..Default::default()
                            },
                            None,
                            false,
                        )?;
                    }
                    run
                };
                let run =
                    store.save_run(&id, previous.as_ref().map(|r| r.revision.as_str()), &run)?;
                let effort = if resume {
                    serde_json::from_value(run.value.parameters["effort"].clone()).ok()
                } else {
                    profile.effort
                };
                Ok(Start {
                    id: id.clone(),
                    run,
                    credential: connection.credential.clone(),
                    effort,
                })
            })();
            match result {
                Ok(start) => (
                    read_view(&store, &start.id, None, 32).map(Output::View),
                    Some(start),
                    None,
                ),
                Err(e) => (Err(e), None, None),
            }
        });
        if let Ok(operation) = &operation {
            self.chats
                .as_mut()
                .expect("initialized")
                .preparing
                .insert(operation.clone(), prepared_id);
        }
        operation
    }
    pub(crate) fn chat_query(
        &mut self,
        caller: Caller,
        query: Query,
    ) -> Result<super::api::QueryResult, ActionError> {
        self.authorize(caller)?;
        match query {
            Query::List { offset, limit } => {
                self.chat_coordinator()?;
                let chats = self
                    .chats
                    .as_ref()
                    .expect("initialized")
                    .items
                    .iter()
                    .skip(offset)
                    .take(limit.min(100))
                    .cloned()
                    .collect();
                Ok(super::api::QueryResult::Chat(Output::List { chats }))
            }
            Query::State { id } => {
                let id = self.chat_id(id)?;
                let c = self.chat_coordinator()?;
                let run = c
                    .active
                    .values()
                    .find(|a| a.start.id == id)
                    .map(|a| a.start.run.value.clone())
                    .or_else(|| {
                        c.queue
                            .iter()
                            .find(|s| s.id == id)
                            .map(|s| s.run.value.clone())
                    })
                    .or_else(|| {
                        c.items
                            .iter()
                            .find(|s| s.item.item_id == id)
                            .and_then(|s| s.run.as_ref().map(|r| r.value.clone()))
                    });
                Ok(super::api::QueryResult::Chat(Output::State { run }))
            }
            Query::Read { id, before, limit } => {
                let id = self.chat_id(id)?;
                let for_display = matches!(caller, Caller::Ui);
                let operation = self.chat_io(false, move |store| {
                    let result =
                        read_view(&store, &id, before.as_deref(), limit).map(|mut view| {
                            view.for_display = for_display;
                            Output::View(view)
                        });
                    (result, None, None)
                })?;
                Ok(super::api::QueryResult::Pending { operation })
            }
        }
    }
    pub(crate) fn stop_chats(&mut self) {
        if let Some(c) = &mut self.chats {
            c.stop(None);
            for draft in c.drafts.values_mut() {
                draft.due = 0;
            }
        }
    }
    pub(crate) fn chat_unsaved_draft(&self) -> bool {
        self.chats
            .as_ref()
            .is_some_and(|c| c.drafts.values().any(|d| d.failed))
    }
    pub(crate) fn resume_chat_coordinator(&mut self) {
        if let Some(c) = &mut self.chats {
            c.stopping = false;
        }
    }
    pub(crate) fn chats_busy(&self) -> bool {
        self.chats.as_ref().is_some_and(Coordinator::busy)
    }
    pub(super) fn poll_chats(&mut self) -> bool {
        let Some(mut c) = self.chats.take() else {
            return false;
        };
        let mut changed = false;
        while let Ok(done) = c.io_receive.try_recv() {
            c.io = c.io.saturating_sub(1);
            c.preparing.remove(&done.operation);
            if let Some(mut start) = done.start {
                if c.cancelled_preparing.remove(&start.id) {
                    start.run.value.status = RunStatus::Stopped;
                }
                c.queue.push_back(start);
            }
            let draft_id = c
                .drafts
                .iter()
                .find(|(_, d)| d.pending.as_deref() == Some(&done.operation))
                .map(|(id, _)| id.clone());
            if let Some(id) = draft_id {
                if let Some(draft) = c.drafts.get_mut(&id) {
                    draft.pending = None;
                    match &done.result {
                        Ok(Output::Draft { draft: saved, .. }) => {
                            draft.version = saved.revision.clone();
                            if draft.text == saved.value.text {
                                c.drafts.remove(&id);
                                if let Some(version) = c.pending_send.get_mut(&id) {
                                    *version = saved.revision.clone();
                                }
                            }
                        }
                        _ => {
                            draft.failed = true;
                        }
                    }
                }
            }
            if let Err(error) = &done.result {
                self.error = Some(crate::i18n::UiText::Failure {
                    details: format!("{error:?}"),
                });
            }
            if let Ok(Output::State { run: Some(run) }) = &done.result {
                for item in &mut c.items {
                    if let Some(old) = &mut item.run {
                        if old.value.id == run.id {
                            old.value = run.clone();
                            item.item.badge = run.unread.then_some(1);
                        }
                    }
                }
                if let Some(view) = &mut c.view {
                    if let Some(old) = &mut view.run {
                        if old.value.id == run.id {
                            old.value = run.clone();
                        }
                    }
                }
            }
            if let Ok(Output::View(view)) = &done.result {
                if let Some(item) = c
                    .items
                    .iter_mut()
                    .find(|s| s.item.item_id.as_str() == view.id)
                {
                    item.run = view.run.clone();
                }
                if view.for_display
                    && self
                        .workspace
                        .as_ref()
                        .and_then(|w| w.selected_engine_item())
                        .is_some_and(|(engine, id)| {
                            engine == &engine_id() && id.as_str() == view.id
                        })
                {
                    let mut view = view.clone();
                    if view.before.is_some() {
                        if let Some(old) = c.view.as_ref().filter(|old| old.id == view.id) {
                            let mut entries = old.history.entries.clone();
                            entries.extend(view.history.entries.clone());
                            entries.sort_by(|a, b| a.id.cmp(&b.id));
                            entries.dedup_by(|a, b| a.id == b.id);
                            if entries.len() <= 128
                                && serde_json::to_vec(&entries)
                                    .is_ok_and(|bytes| bytes.len() <= stillus_chat::MAX_PAGE_BYTES)
                            {
                                view.history.entries = entries;
                            }
                        }
                    }
                    if let Some(draft) =
                        c.drafts.get(&ItemId::new(&view.id).expect("valid chat id"))
                    {
                        view.draft.value.text = draft.text.clone();
                    }
                    if let Some(active) = c.active.values().find(|a| a.start.id.as_str() == view.id)
                    {
                        if let Some(run) = &mut view.run {
                            if run.value.id == active.start.run.value.id {
                                run.value.status = RunStatus::Running;
                            }
                        }
                        for entry in &mut view.history.entries {
                            if let Some(message) = &mut entry.message {
                                if message.value.run == active.start.run.value.id {
                                    if let Some(tool) = &mut message.value.tool {
                                        if tool.state == ToolState::Unknown {
                                            tool.state = ToolState::Prepared;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    c.view = Some(view);
                }
            }
            if let Ok(Output::Draft { id, draft }) = &done.result {
                if let Some(view) = &mut c.view {
                    if &view.id == id {
                        view.draft = draft.clone();
                    }
                }
            }
            if let Ok(Output::Metadata { id, metadata }) = &done.result {
                if let Some(view) = &mut c.view {
                    if &view.id == id {
                        view.metadata = metadata.clone();
                    }
                }
            }
            if let Some(open) = done.open {
                c.pending_open = Some(open)
            }
            match done.items {
                Some(Ok(items)) => c.items = items,
                None => {}
                Some(Err(error)) => {
                    self.error = Some(crate::i18n::UiText::Failure {
                        details: format!("chat catalog: {error:?}"),
                    })
                }
            }
            if let Some(w) = self.workspace.as_mut() {
                w.accept_engine_items(
                    &engine_id(),
                    c.items.iter().map(|s| s.item.clone()).collect(),
                );
                w.operations
                    .finish(&done.operation, done.result.map(OperationOutput::Chat));
            }
            changed = true;
        }
        while let Ok(request) = c.tools_receive.try_recv() {
            self.chats = Some(c);
            let result = request.cancel.check().map_err(ai_error).and_then(|_| {
                tools::call(
                    self,
                    request.context,
                    &request.call.name,
                    request.call.arguments,
                )
            });
            c = self.chats.take().expect("owner coordinator");
            match result {
                Ok(value) => {
                    if let Some(operation) = value["operation"].as_str() {
                        c.waiting.push(WaitingTool {
                            operation: operation.into(),
                            reply: request.reply,
                        });
                    } else {
                        let _ = request.reply.send(Ok(value));
                    }
                }
                Err(error) => {
                    let _ = request.reply.send(Err(error));
                }
            }
            changed = true;
        }
        c.waiting.retain(|pending| {
            let status = self
                .workspace
                .as_ref()
                .and_then(|w| w.operations.progress(&pending.operation).ok())
                .map(|(s, _)| s);
            match status {
                Some(OperationStatus::Pending | OperationStatus::Running) => true,
                Some(OperationStatus::Failed(error)) => {
                    let _ = pending.reply.send(Err(error));
                    false
                }
                Some(OperationStatus::Cancelled) => {
                    let _ = pending.reply.send(Err(ActionError::Cancelled));
                    false
                }
                Some(status) => {
                    let _ = pending.reply.send(
                        serde_json::to_value(status).map_err(|_| ActionError::InvalidArguments),
                    );
                    false
                }
                None => {
                    let _ = pending.reply.send(Err(ActionError::SessionChanged));
                    false
                }
            }
        });
        let mut finished = Vec::new();
        for (id, active) in &mut c.active {
            while let Ok(update) = active.updates.try_recv() {
                let message = match update {
                    WorkerUpdate::Run(run) => {
                        if let Some(w) = self.workspace.as_mut() {
                            w.operations.update_progress(
                                &active.operation,
                                super::actions::OperationProgress {
                                    phase: if run.awaiting_model {
                                        "chat/model"
                                    } else {
                                        "chat/tools"
                                    }
                                    .into(),
                                    completed: if run.awaiting_model {
                                        run.requests as u64
                                    } else {
                                        run.tools as u64
                                    },
                                    total: Some(if run.awaiting_model {
                                        run.request_limit as u64
                                    } else {
                                        run.tool_limit as u64
                                    }),
                                },
                            );
                        }
                        active.start.run.value = run;
                        changed = true;
                        continue;
                    }
                    WorkerUpdate::Message(message) => message,
                };
                if let Some(view) = c
                    .view
                    .as_mut()
                    .filter(|v| v.id == active.start.id.as_str() && v.before.is_none())
                {
                    let entry = HistoryEntry {
                        id: message.id.clone(),
                        message: Some(Versioned {
                            revision: String::new(),
                            value: message,
                        }),
                        diagnostic: None,
                    };
                    if let Some(old) = view.history.entries.iter_mut().find(|e| e.id == entry.id) {
                        *old = entry;
                    } else {
                        view.history.entries.push(entry);
                    }
                }
                changed = true;
            }
            if let Ok(mut progress) = active.progress.lock() {
                if let Some((message, text)) = progress.take() {
                    if let Some(view) = &mut c.view {
                        if view.id == active.start.id.as_str() && view.before.is_none() {
                            if let Some(entry) =
                                view.history.entries.iter_mut().find(|e| e.id == message)
                            {
                                if let Some(m) = &mut entry.message {
                                    m.value.text = text;
                                }
                            } else {
                                view.history.entries.push(HistoryEntry {
                                    id: message.clone(),
                                    message: Some(Versioned {
                                        revision: String::new(),
                                        value: Message {
                                            id: message,
                                            run: id.clone(),
                                            role: Role::Assistant,
                                            text,
                                            delivery: Delivery::Partial,
                                            created_ms: now_ms(),
                                            tool: None,
                                            provider_state: None,
                                            request_id: None,
                                        },
                                    }),
                                    diagnostic: None,
                                });
                            }
                        }
                    }
                    changed = true;
                }
            }
            match active.completion.try_recv() {
                Ok(result) => finished.push((id.clone(), result)),
                Err(mpsc::TryRecvError::Disconnected) => finished.push((
                    id.clone(),
                    Err(ActionError::Failed("chat worker stopped".into())),
                )),
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        for (id, result) in finished {
            if let Some(active) = c.active.remove(&id) {
                let _ = active.worker.join();
                if let Some(w) = self.workspace.as_mut() {
                    w.operations.finish(
                        &active.operation,
                        result
                            .as_ref()
                            .map(|r| {
                                OperationOutput::Chat(Output::State {
                                    run: Some(r.value.clone()),
                                })
                            })
                            .map_err(Clone::clone),
                    );
                }
                if let Some(item) = c
                    .items
                    .iter_mut()
                    .find(|s| s.item.item_id == active.start.id)
                {
                    if let Ok(run) = result {
                        item.item.badge = run.value.unread.then_some(1);
                        item.run = Some(run);
                    }
                }
                if c.view
                    .as_ref()
                    .is_some_and(|v| v.id == active.start.id.as_str())
                {
                    c.refresh = Some((
                        active.start.id,
                        c.view.as_ref().and_then(|v| v.before.clone()),
                    ));
                }
            }
            changed = true;
        }
        let connection = self
            .global
            .as_ref()
            .and_then(|g| g.borrow().ai().connection);
        for active in c.active.values() {
            if connection.as_ref().is_none_or(|v| {
                v.credential != active.start.credential
                    || v.provider.id() != active.start.run.value.provider
            }) {
                active.cancel.cancel();
            }
        }
        while c.active.len() < 2 && !c.queue.is_empty() {
            let Some(w) = self.workspace.as_mut() else {
                break;
            };
            let Ok(operation) = w.operations.register(w.session_id(), false) else {
                break;
            };
            let mut start = c.queue.pop_front().expect("queued");
            let stopped = start.run.value.status == RunStatus::Stopped;
            start.run.value.status = RunStatus::Running;
            let cancel = Cancellation::default();
            if c.stopping
                || stopped
                || connection
                    .as_ref()
                    .is_none_or(|v| v.credential != start.credential)
            {
                cancel.cancel();
            }
            let context = match ToolContext::capture(self) {
                Ok(context) => context,
                Err(_) => break,
            };
            let (home, registry) = (
                self.global.as_ref().and_then(|g| g.borrow().home()),
                c.provider_registry.clone(),
            );
            let store = c.store.clone();
            let tools = c.tools_send.clone();
            let progress = Arc::new(Mutex::new(None));
            let worker_progress = progress.clone();
            let worker_start = start.clone();
            let worker_cancel = cancel.clone();
            let (send, completion) = mpsc::sync_channel(1);
            let (update_send, updates) = mpsc::sync_channel(2);
            let credentials = c.credentials.clone();
            let worker = thread::spawn(move || {
                let result = run_worker(
                    &store,
                    &worker_start,
                    home,
                    registry,
                    credentials,
                    context,
                    tools,
                    &worker_cancel,
                    worker_progress,
                    update_send,
                );
                let _ = send.send(result);
            });
            let id = start.run.value.id.clone();
            c.active.insert(
                id,
                Active {
                    start,
                    cancel,
                    worker,
                    completion,
                    progress,
                    updates,
                    operation,
                },
            );
            changed = true;
        }
        let deferred = c
            .deferred_metadata
            .iter()
            .filter(|(_, (id, ..))| !c.running(id))
            .map(|(op, _)| op.clone())
            .collect::<Vec<_>>();
        let pending = c.pending_open.clone();
        let refresh = c.refresh.take();
        let now = self.now_ms();
        let drafts = c
            .drafts
            .iter()
            .filter(|(_, d)| d.pending.is_none() && !d.failed && (d.due <= now || c.stopping))
            .map(|(id, d)| (id.clone(), d.version.clone(), d.text.clone()))
            .collect::<Vec<_>>();
        let sends = c
            .pending_send
            .iter()
            .filter(|(id, _)| !c.drafts.contains_key(*id))
            .map(|(id, v)| (id.clone(), v.clone()))
            .collect::<Vec<_>>();
        self.chats = Some(c);
        for op in deferred {
            let claimed = self
                .workspace
                .as_mut()
                .ok_or(ActionError::NotFound)
                .and_then(|w| w.operations.claim_catalogue_write(&op));
            match claimed {
                Ok(()) => {
                    let (id, version, patch, alias) = self
                        .chats
                        .as_mut()
                        .expect("coordinator")
                        .deferred_metadata
                        .remove(&op)
                        .expect("deferred metadata");
                    self.launch_chat_io(op, true, move |store| {
                        (
                            store
                                .patch_metadata(&id, &version, patch, alias, true)
                                .map(|metadata| Output::Metadata {
                                    id: id.to_string(),
                                    metadata,
                                })
                                .map_err(Into::into),
                            None,
                            None,
                        )
                    });
                }
                Err(ActionError::Busy) => {}
                Err(error) => {
                    self.chats
                        .as_mut()
                        .expect("coordinator")
                        .deferred_metadata
                        .remove(&op);
                    if let Some(w) = self.workspace.as_mut() {
                        w.operations.finish(&op, Err(error));
                    }
                }
            }
        }
        for (id, version) in sends {
            self.chats
                .as_mut()
                .expect("coordinator")
                .pending_send
                .remove(&id);
            if let Err(error) = self.enqueue_chat(id.to_string(), Some(version), false) {
                self.error = Some(crate::i18n::UiText::Failure {
                    details: format!("{error:?}"),
                });
            }
            changed = true;
        }
        for (id, version, text) in drafts {
            let target = id.clone();
            if let Ok(operation) = self.chat_io(false, move |store| {
                (
                    store
                        .save_draft(&target, &version, &Draft { text })
                        .map(|draft| Output::Draft {
                            id: target.to_string(),
                            draft,
                        })
                        .map_err(Into::into),
                    None,
                    None,
                )
            }) {
                if let Some(draft) = self.chats.as_mut().and_then(|c| c.drafts.get_mut(&id)) {
                    draft.pending = Some(operation);
                }
            }
        }

        if let Some((id, before)) = refresh {
            if self
                .chat_query(
                    Caller::Ui,
                    Query::Read {
                        id: id.to_string(),
                        before: before.clone(),
                        limit: 32,
                    },
                )
                .is_err()
            {
                self.chats.as_mut().expect("coordinator").refresh = Some((id, before));
            }
        }
        if let Some(id) = pending {
            let opened = self
                .workspace
                .as_mut()
                .is_some_and(|w| w.open_engine_item(&engine_id(), &id).is_ok());
            if opened {
                self.chats.as_mut().expect("coordinator").pending_open = None;
                let _ = self.chat_query(
                    Caller::Ui,
                    Query::Read {
                        id: id.to_string(),
                        before: None,
                        limit: 32,
                    },
                );
                self.effects.push(super::ApplicationEvent::ResetEditor);
                changed = true;
            } else if !self.save_worker_active {
                let _ = self.start_chat_document_save();
            }
        }
        if changed {
            if let (Some(w), Some(c)) = (self.workspace.as_mut(), self.chats.as_ref()) {
                w.accept_engine_items(
                    &engine_id(),
                    c.items.iter().map(|s| s.item.clone()).collect(),
                );
            }
        }
        changed
    }
    fn start_chat_document_save(&mut self) -> bool {
        self.retry_save()
    }
}
fn read_view(
    store: &ChatStore,
    id: &ItemId,
    before: Option<&str>,
    limit: usize,
) -> Result<ChatView, ActionError> {
    let mut history = store.history(id, before, limit)?;
    for entry in &mut history.entries {
        if let Some(m) = &mut entry.message {
            m.value.provider_state = None;
        }
    }
    let mut diagnostics = Vec::new();
    let metadata = match store.metadata(id) {
        Ok(metadata) => metadata,
        Err(ChatError::Missing) => return Err(ActionError::NotFound),
        Err(error) => {
            diagnostics.push(format!("metadata: {error:?}"));
            Versioned {
                revision: String::new(),
                value: Metadata {
                    common: stillus_engine::CommonMetadata {
                        title: id.to_string(),
                        ..Default::default()
                    },
                    automatic_title: false,
                    alias: "default".into(),
                },
            }
        }
    };
    let draft = store.draft(id).unwrap_or_else(|error| {
        diagnostics.push(format!("draft: {error:?}"));
        Versioned {
            revision: String::new(),
            value: Draft::default(),
        }
    });
    let run = store.run(id).unwrap_or_else(|error| {
        diagnostics.push(format!("run: {error:?}"));
        None
    });
    Ok(ChatView {
        id: id.to_string(),
        for_display: true,
        before: before.map(str::to_owned),
        metadata,
        diagnostics,
        draft,
        history,
        run,
    })
}

fn run_worker(
    store: &ChatStore,
    start: &Start,
    home: Option<PathBuf>,
    registry: ProviderRegistry,
    credentials: Arc<dyn CredentialStore>,
    context: ToolContext,
    tools: SyncSender<ToolRequest>,
    cancel: &Cancellation,
    progress: Arc<Mutex<Option<(String, String)>>>,
    updates: SyncSender<WorkerUpdate>,
) -> Result<Versioned<Run>, ActionError> {
    let mut run = start.run.clone();
    run.value.status = RunStatus::Running;
    run = store.save_run(&start.id, Some(&run.revision), &run.value)?;
    updates
        .send(WorkerUpdate::Run(run.value.clone()))
        .map_err(|_| ActionError::Cancelled)?;
    let result = (|| -> Result<(), ActionError> {
        cancel.check().map_err(ai_error)?;
        let home = home.ok_or(ActionError::NotFound)?;
        let journal = super::journal::FileJournal::for_home(&home);
        let provider_id: AiProvider = serde_json::from_value(json!(run.value.provider))
            .map_err(|_| ActionError::InvalidArguments)?;
        let provider = registry
            .get(&provider_id)
            .ok_or(ActionError::RequiresUserInteraction)?;
        let key = credentials.read(&start.credential);
        let key = ApiKey::for_engine(
            key.map_err(|_| ActionError::RequiresUserInteraction)?,
            provider.as_ref(),
        )
        .map_err(ai_error)?;
        let mut summary = store.summary(&start.id)?;
        let (mut input, mut input_ids) = load_context(
            store,
            &start.id,
            summary.as_ref().and_then(|s| s.value.through.as_deref()),
            &run.value.provider,
            &run.value.id,
        )?;
        let definitions = tools::list()
            .into_iter()
            .map(|t| ToolDefinition {
                name: t.name.into(),
                description: t.description.into(),
                schema: t.input_schema,
            })
            .collect::<Vec<_>>();
        loop {
            cancel.check().map_err(ai_error)?;
            if run.value.pending_calls.is_empty() && !run.value.awaiting_model {
                run.value.status = RunStatus::Completed;
                break;
            }
            if (!run.value.pending_calls.is_empty() && run.value.tools >= run.value.tool_limit)
                || (run.value.pending_calls.is_empty()
                    && run.value.requests >= run.value.request_limit)
            {
                run.value.status = RunStatus::Paused;
                break;
            }
            if journal.blocked() {
                return Err(ai_error(stillus_ai::AiError::Journal));
            }
            if let Some(call) = run.value.pending_calls.first().cloned() {
                let previous = previous_tool(store, &start.id, &run.value.id, &call.id)?;
                let (safe, record_id) = if let Some((output, id)) = previous {
                    (output, id)
                } else {
                    run.value.tools += 1;
                    run = store.save_run(&start.id, Some(&run.revision), &run.value)?;
                    updates
                        .send(WorkerUpdate::Run(run.value.clone()))
                        .map_err(|_| ActionError::Cancelled)?;
                    let mut record = Message {
                        id: new_id(),
                        run: run.value.id.clone(),
                        role: Role::Tool,
                        text: String::new(),
                        delivery: Delivery::Complete,
                        created_ms: now_ms(),
                        tool: Some(call.clone()),
                        provider_state: None,
                        request_id: run.value.pending_request.clone(),
                    };
                    let intent = store.save_message(&start.id, None, &record)?;
                    updates
                        .send(WorkerUpdate::Message(record.clone()))
                        .map_err(|_| ActionError::Cancelled)?;
                    let (send, receive) = mpsc::sync_channel(1);
                    tools
                        .send(ToolRequest {
                            context,
                            call: ToolInvocation {
                                id: call.id.clone(),
                                name: call.name.clone(),
                                arguments: call.arguments.clone(),
                            },
                            reply: send,
                            cancel: cancel.clone(),
                        })
                        .map_err(|_| ActionError::Cancelled)?;
                    // Once accepted by the owner, even Stop waits for the actual write outcome.
                    let result = receive.recv().map_err(|_| ActionError::Cancelled)?;
                    let output = match &result {
                        Ok(value) => value.clone(),
                        Err(error) => json!({"error":error}),
                    };
                    let encoded =
                        serde_json::to_vec(&output).map_err(|_| ActionError::InvalidArguments)?;
                    let safe = if encoded.len() > 128 * 1024 {
                        json!({"error":"Result exceeds the 128 KiB tool-result limit. Request a smaller read page; never repeat a mutation to obtain its result."})
                    } else {
                        stillus_ai::journal::safe_response(&encoded, &key)
                    };
                    let tool = record.tool.as_mut().expect("tool record");
                    tool.state = if result.is_ok() {
                        ToolState::Completed
                    } else {
                        ToolState::Failed
                    };
                    tool.result = Some(safe.clone());
                    store.save_message(&start.id, Some(&intent.revision), &record)?;
                    updates
                        .send(WorkerUpdate::Message(record.clone()))
                        .map_err(|_| ActionError::Cancelled)?;
                    (safe, record.id)
                };
                if !input.iter().any(
                    |item| matches!(item,InputItem::ToolResult{call_id,..} if call_id==&call.id),
                ) {
                    input.push(InputItem::ToolResult {
                        call_id: call.id,
                        name: call.name,
                        output: safe,
                    });
                    input_ids.push(record_id);
                }
                run.value.pending_calls.remove(0);
                if run.value.pending_calls.is_empty() {
                    run.value.awaiting_model = true;
                }
                run = store.save_run(&start.id, Some(&run.revision), &run.value)?;
                updates
                    .send(WorkerUpdate::Run(run.value.clone()))
                    .map_err(|_| ActionError::Cancelled)?;
                continue;
            }
            let capabilities = provider.capabilities(&run.value.model).map_err(ai_error)?;
            if capabilities.needs_summary_with_context(
                &input,
                &definitions,
                summary.as_ref().map(|s| s.value.text.as_str()),
            ) {
                let split = compression_split(&input)
                    .ok_or_else(|| ai_error(stillus_ai::AiError::ContextLimit))?;
                run.value.requests += 1;
                run = store.save_run(&start.id, Some(&run.revision), &run.value)?;
                updates
                    .send(WorkerUpdate::Run(run.value.clone()))
                    .map_err(|_| ActionError::Cancelled)?;
                let request = Generation {
                    content_policy: stillus_ai::journal::ContentPolicy::Public,
                    model: run.value.model.clone(),
                    effort: start.effort,
                    input: input[..split].to_vec(),
                    tools: Vec::new(),
                    summary: true,
                    context_summary: summary.as_ref().map(|s| s.value.text.clone()),
                    chat: start.id.to_string(),
                    run: run.value.id.clone(),
                    step: run.value.requests,
                    max_output: capabilities.output_tokens,
                };
                let response = provider
                    .generate(&request, &key, journal.as_ref(), cancel, &mut |_| {})
                    .map_err(ai_error)?;
                let boundary = input_ids.get(split - 1).cloned();
                let value = ContextSummary {
                    through: boundary,
                    text: response.text,
                };
                summary = Some(store.save_summary(
                    &start.id,
                    summary.as_ref().map(|s| s.revision.as_str()),
                    &value,
                )?);
                input.drain(..split);
                input_ids.drain(..split);
                if journal.blocked() {
                    return Err(ai_error(stillus_ai::AiError::Journal));
                }
                continue;
            }
            run.value.requests += 1;
            run = store.save_run(&start.id, Some(&run.revision), &run.value)?;
            updates
                .send(WorkerUpdate::Run(run.value.clone()))
                .map_err(|_| ActionError::Cancelled)?;
            let request = Generation {
                content_policy: stillus_ai::journal::ContentPolicy::Public,
                model: run.value.model.clone(),
                effort: start.effort,
                input: input.clone(),
                tools: definitions.clone(),
                summary: false,
                context_summary: summary.as_ref().map(|s| s.value.text.clone()),
                chat: start.id.to_string(),
                run: run.value.id.clone(),
                step: run.value.requests,
                max_output: capabilities.output_tokens,
            };
            let mut message = Message {
                id: new_id(),
                run: run.value.id.clone(),
                role: Role::Assistant,
                text: String::new(),
                delivery: Delivery::Partial,
                created_ms: now_ms(),
                tool: None,
                provider_state: None,
                request_id: None,
            };
            let mut saved: Option<Versioned<Message>> = None;
            let mut checkpoint = std::time::Instant::now();
            let mut persistence_error = None;
            let response =
                provider.generate(&request, &key, journal.as_ref(), cancel, &mut |event| {
                    if let GenerationEvent::Text(text) = event {
                        message.text = text;
                        if let Ok(mut slot) = progress.lock() {
                            *slot = Some((message.id.clone(), message.text.clone()));
                        }
                        if checkpoint.elapsed() >= Duration::from_millis(500)
                            && persistence_error.is_none()
                        {
                            match store.save_message(
                                &start.id,
                                saved.as_ref().map(|s| s.revision.as_str()),
                                &message,
                            ) {
                                Ok(value) => saved = Some(value),
                                Err(e) => {
                                    persistence_error = Some(e);
                                    cancel.cancel();
                                }
                            }
                            checkpoint = std::time::Instant::now();
                        }
                    }
                });
            if let Some(e) = persistence_error {
                return Err(e.into());
            }
            match response {
                Ok(response) => {
                    message.text = response.text.clone();
                    message.provider_state = Some(
                        json!({"provider":run.value.provider,"data":response.state,"calls":response.calls}),
                    );
                    message.request_id = Some(response.request_id.clone());
                    message.delivery = Delivery::Complete;
                    store.save_message(
                        &start.id,
                        saved.as_ref().map(|s| s.revision.as_str()),
                        &message,
                    )?;
                    input.push(InputItem::Assistant {
                        text: response.text,
                        state: response.state,
                    });
                    input_ids.push(message.id);
                    run.value.pending_calls = response
                        .calls
                        .into_iter()
                        .map(|call| ToolCall {
                            id: call.id,
                            name: call.name,
                            arguments: call.arguments,
                            state: ToolState::Prepared,
                            result: None,
                        })
                        .collect();
                    run.value.pending_request = Some(response.request_id);
                    run.value.awaiting_model = false;
                    run = store.save_run(&start.id, Some(&run.revision), &run.value)?;
                    updates
                        .send(WorkerUpdate::Run(run.value.clone()))
                        .map_err(|_| ActionError::Cancelled)?;
                    if journal.blocked() {
                        return Err(ai_error(stillus_ai::AiError::Journal));
                    }
                    if run.value.pending_calls.is_empty() {
                        run.value.status = RunStatus::Completed;
                        break;
                    }
                }
                Err(e) => {
                    message.delivery = Delivery::Interrupted;
                    store.save_message(
                        &start.id,
                        saved.as_ref().map(|s| s.revision.as_str()),
                        &message,
                    )?;
                    return Err(ai_error(e));
                }
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        run.value.status = if matches!(error, ActionError::Cancelled) {
            RunStatus::Stopped
        } else if error == ai_error(stillus_ai::AiError::Journal) {
            RunStatus::Paused
        } else {
            RunStatus::Failed
        };
        run.value.error = if error == ActionError::Cancelled {
            None
        } else {
            Some(format!("{error:?}"))
        };
    }
    run.value.unread = true;
    store
        .save_run(&start.id, Some(&run.revision), &run.value)
        .map_err(Into::into)
}
fn load_context(
    store: &ChatStore,
    id: &ItemId,
    through: Option<&str>,
    provider: &str,
    active_run: &str,
) -> Result<(Vec<InputItem>, Vec<String>), ActionError> {
    let mut cursor = None;
    let mut messages = Vec::new();
    let mut bytes = 0;
    loop {
        let page = store.history(id, cursor.as_deref(), 64)?;
        let mut done = false;
        for entry in page.entries.into_iter().rev() {
            if through.is_some_and(|t| entry.id.as_str() <= t) {
                done = true;
                break;
            }
            let message = entry
                .message
                .ok_or_else(|| {
                    ActionError::Failed("chat history contains an unreadable record".into())
                })?
                .value;
            bytes += message
                .tool
                .as_ref()
                .map(|t| {
                    serde_json::to_vec(t)
                        .map(|v| v.len())
                        .unwrap_or(usize::MAX / 2)
                })
                .unwrap_or(0)
                + message.text.len()
                + message
                    .provider_state
                    .as_ref()
                    .map(|v| v.to_string().len())
                    .unwrap_or(0);
            if bytes > 2 * 1024 * 1024 {
                return Err(ai_error(stillus_ai::AiError::ContextLimit));
            }
            if message
                .tool
                .as_ref()
                .is_some_and(|t| t.state == ToolState::Unknown)
            {
                return Err(ActionError::RequiresUserInteraction);
            }
            messages.push(message);
        }
        if done || page.next.is_none() {
            break;
        }
        cursor = page.next;
    }
    messages.reverse();
    let completed_calls = messages
        .iter()
        .filter_map(|m| {
            m.tool
                .as_ref()
                .filter(|t| t.result.is_some())
                .map(|t| (m.run.clone(), t.id.clone()))
        })
        .collect::<std::collections::BTreeSet<_>>();
    let providers = messages
        .iter()
        .filter_map(|m| {
            m.provider_state
                .as_ref()
                .and_then(|s| s["provider"].as_str())
                .map(|p| (m.run.clone(), p.to_owned()))
        })
        .collect::<BTreeMap<_, _>>();
    let mut input = Vec::new();
    let mut ids = Vec::new();
    for m in messages {
        let missing_calls = if m.run != active_run
            && m.provider_state
                .as_ref()
                .is_some_and(|s| s["provider"] == provider)
        {
            m.provider_state
                .as_ref()
                .and_then(|s| {
                    serde_json::from_value::<Vec<ToolInvocation>>(s["calls"].clone()).ok()
                })
                .unwrap_or_default()
                .into_iter()
                .filter(|call| !completed_calls.contains(&(m.run.clone(), call.id.clone())))
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let same_provider = providers.get(&m.run).is_some_and(|p| p == provider);
        let item = match m.role {
            Role::User => Some(InputItem::User(m.text)),
            Role::Assistant if m.delivery == Delivery::Complete => Some(InputItem::Assistant {
                text: m.text,
                state: m
                    .provider_state
                    .filter(|s| s["provider"] == provider)
                    .and_then(|s| s.get("data").cloned())
                    .filter(|s| !s.is_null()),
            }),
            Role::Tool => m.tool.and_then(|t| {
                t.result.map(|output| {
                    if same_provider {
                        InputItem::ToolResult {
                            call_id: t.id,
                            name: t.name,
                            output,
                        }
                    } else {
                        InputItem::Assistant {
                            text: format!("Earlier tool result (data): {}\n{}", t.name, output),
                            state: None,
                        }
                    }
                })
            }),
            _ => None,
        };
        if let Some(item) = item {
            input.push(item);
            ids.push(m.id.clone());
        }
        for call in missing_calls {
            input.push(InputItem::ToolResult {call_id:call.id,name:call.name,output:json!({"error":"Task interrupted before executing this action. Do not repeat without a new user request."})});
            ids.push(m.id.clone());
        }
    }
    Ok((input, ids))
}
fn previous_tool(
    store: &ChatStore,
    id: &ItemId,
    run: &str,
    call: &str,
) -> Result<Option<(Value, String)>, ActionError> {
    let mut cursor = None;
    loop {
        let page = store.history(id, cursor.as_deref(), 64)?;
        for entry in page.entries.into_iter().rev() {
            let Some(m) = entry.message else { continue };
            if m.value.run != run {
                return Ok(None);
            }
            if let Some(tool) = m.value.tool {
                if tool.id == call {
                    if tool.state == ToolState::Unknown {
                        return Err(ActionError::RequiresUserInteraction);
                    }
                    return Ok(tool.result.map(|v| (v, entry.id)));
                }
            }
        }
        if page.next.is_none() {
            break;
        }
        cursor = page.next;
    }
    Ok(None)
}

fn registry() -> ProviderRegistry {
    #[cfg(feature = "test-utils")]
    if std::env::var("STILLUS_TEST_AI").as_deref() == Ok("1") {
        return fixtures::registry();
    }
    ProviderRegistry::standard()
}
fn credentials() -> Arc<dyn CredentialStore> {
    #[cfg(feature = "test-utils")]
    if std::env::var("STILLUS_TEST_AI").as_deref() == Ok("1") {
        return Arc::new(super::ai::fixtures::Vault);
    }
    Arc::new(stillus_platform::credentials::SystemCredentials)
}
#[cfg(test)]
impl Application {
    pub(super) fn set_chat_executors(
        &mut self,
        registry: ProviderRegistry,
        credentials: Arc<dyn CredentialStore>,
    ) {
        let c = self.chat_coordinator().unwrap();
        c.provider_registry = registry;
        c.credentials = credentials;
    }
}
#[cfg(feature = "test-utils")]
pub(crate) mod fixtures;
