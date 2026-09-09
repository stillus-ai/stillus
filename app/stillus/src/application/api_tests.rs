// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]
#![cfg(test)]

use super::{
    Application,
    actions::ActionError,
    api::{Caller, Command, Query, ToolContext, TrustedCommand},
    tools,
};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    fs,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

struct Clock(AtomicU64);
impl super::runtime::Clock for Clock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
    fn wall_time(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH
            + Duration::from_secs(1_800_000_000)
            + Duration::from_millis(self.now_ms())
    }
}
struct Fixture {
    root: PathBuf,
    app: Application,
    context: ToolContext,
    clock: Arc<Clock>,
}
impl Fixture {
    fn new() -> Self {
        let root = crate::test_support::workspace("stillus-headless-actions");
        stillus_core::initialize_workspace(&root).unwrap();
        fs::write(
            root.join("notes/Alpha.md"),
            "---\ntitle: Alpha\ncustom: preserved\n---\n\nAlpha\nbody\n",
        )
        .unwrap();
        fs::write(root.join("notes/Beta.md"), "Beta\nsecond\n").unwrap();
        let mut app = Application::load(&root);
        let clock = Arc::new(Clock(AtomicU64::new(0)));
        app.set_clock(clock.clone());
        app.set_rss_executor(Arc::new(|request| {
            Ok(stillus_core::RssRefreshResult::NotModified {
                item_id: request.item_id,
                fetched_at: "2026-09-09T00:00:00Z".into(),
            })
        }));
        let context = ToolContext::capture(&app).unwrap();
        Self {
            root,
            app,
            context,
            clock,
        }
    }
    fn call(&mut self, name: &str, value: Value) -> Result<Value, ActionError> {
        tools::call(&mut self.app, self.context, name, value)
    }
    fn id(&mut self, title: &str) -> String {
        self.call("notes/list", json!({})).unwrap()["notes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|note| note["title"] == title)
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .into()
    }
    fn wait(&mut self, operation: &str) -> Value {
        let until = Instant::now() + Duration::from_secs(8);
        loop {
            let value = self
                .call("operations/status", json!({"id":operation}))
                .unwrap();
            if value.get("Completed").is_some()
                || value.get("Saved").is_some()
                || value.get("Failed").is_some()
                || value == "Cancelled"
            {
                return value;
            }
            assert!(
                Instant::now() < until,
                "operation did not complete: {value}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn accepted(&mut self, name: &str, args: Value) -> Value {
        let value = self.call(name, args).unwrap();
        self.wait(value["operation"].as_str().expect("accepted operation"))
    }
    fn global(&mut self) {
        self.app.global = Some(Rc::new(RefCell::new(
            super::global::GlobalApplication::load(Some(&self.root)).store,
        )));
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.app.shutdown().unwrap();
        fs::remove_dir_all(&self.root).unwrap();
    }
}

#[test]
fn headless_buffer_undo_versions_and_durable_completion_share_the_native_editor() {
    let mut f = Fixture::new();
    let id = f.id("Alpha");
    f.call("notes/open", json!({"id":id})).unwrap();
    let before = f.call("notes/read", json!({"id":id})).unwrap();
    f.app
        .apply(stillus_core::EditorCommand::Insert("draft ".into()));
    assert_eq!(
        f.call(
            "notes/update",
            json!({"id":id,"version":before["version"],"start":0,"end":0,"text":"stale"})
        ),
        Err(ActionError::Conflict)
    );
    let draft = f.call("notes/read", json!({"id":id})).unwrap();
    assert!(draft["text"].as_str().unwrap().contains("draft"));
    f.call("editor/undo", json!({"id":id,"version":draft["version"]}))
        .unwrap();
    assert_eq!(
        f.call("notes/read", json!({"id":id})).unwrap()["text"],
        before["text"]
    );
    let current = f.call("notes/read", json!({"id":id})).unwrap();
    f.call(
        "notes/update",
        json!({"id":id,"version":current["version"],"start":0,"end":0,"text":"persisted "}),
    )
    .unwrap();
    let current = f.call("notes/read", json!({"id":id})).unwrap();
    let saved = f.accepted("notes/save", json!({"id":id,"version":current["version"]}));
    assert_eq!(saved["Completed"]["saved"], true);
    let workspace = f.app.workspace.as_ref().unwrap();
    let path = workspace.resolve_target(&id).unwrap();
    let disk = fs::read_to_string(path).unwrap();
    assert!(disk.contains("persisted "));
    assert!(disk.contains("custom: preserved"));
}

#[test]
fn autosave_runs_with_no_window_or_settings_page() {
    let mut f = Fixture::new();
    let id = f.id("Alpha");
    let before = f.call("notes/read", json!({"id":id})).unwrap();
    f.call(
        "notes/update",
        json!({"id":id,"version":before["version"],"start":0,"end":0,"text":"automatic "}),
    )
    .unwrap();
    f.clock.0.store(10_000, Ordering::Relaxed);
    assert!(f.app.next_deadline() <= 10_025);
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        f.app.poll();
        if matches!(
            f.app
                .workspace
                .as_ref()
                .unwrap()
                .document()
                .unwrap()
                .save_status(),
            stillus_core::SaveStatus::Clean { .. }
        ) {
            break;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(5));
    }
    let path = f
        .app
        .workspace
        .as_ref()
        .unwrap()
        .resolve_target(&id)
        .unwrap();
    assert!(fs::read_to_string(path).unwrap().contains("automatic "));
}

#[test]
fn addressed_external_edit_preserves_selection_and_deduplicates_absolute_paths() {
    let mut f = Fixture::new();
    let external = f.root.join("outside.txt");
    fs::write(&external, "one\ntwo\n").unwrap();
    let opened = f.call("external/open", json!({"path":external})).unwrap();
    let external_id = opened["id"].as_str().unwrap().to_owned();
    f.call("external/open", json!({"path":external})).unwrap();
    assert_eq!(
        f.call("external/list", json!({})).unwrap()["files"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let note = f.id("Alpha");
    f.call("notes/open", json!({"id":note})).unwrap();
    let before = f.call("notes/read", json!({"id":external_id})).unwrap();
    let result = f.accepted(
        "notes/update",
        json!({"id":external_id,"version":before["version"],"start":0,"end":3,"text":"updated"}),
    );
    assert_eq!(result["Saved"]["saved"], true);
    assert_eq!(fs::read_to_string(&external).unwrap(), "updated\ntwo\n");
    assert_eq!(
        f.app
            .workspace
            .as_ref()
            .unwrap()
            .document()
            .unwrap()
            .title(),
        "Alpha"
    );
    assert_eq!(
        f.call("external/open", json!({"path":"outside.txt"})),
        Err(ActionError::InvalidArguments)
    );
    let wrong = f.root.join("unknown.bin");
    fs::write(&wrong, "body").unwrap();
    assert!(f.call("external/open", json!({"path":wrong})).is_err());
    #[cfg(unix)]
    {
        let link = f.root.join("link.txt");
        std::os::unix::fs::symlink(&external, &link).unwrap();
        assert!(f.call("external/open", json!({"path":link})).is_err());
    }
}

#[test]
fn sessions_are_fixed_and_cannot_be_switched_through_tools_or_settings() {
    let mut f = Fixture::new();
    let other = Fixture::new();
    for name in ["workspace/open", "workspace/initialize", "settings/ui"] {
        assert_eq!(
            f.call(name, json!({"path":other.root})),
            Err(ActionError::NotFound)
        );
    }
    assert!(matches!(
        f.app.dispatch(
            f.context.caller(),
            Command::Trusted(TrustedCommand::OpenWorkspace(other.root.clone()))
        ),
        Err(ActionError::RequiresUserInteraction)
    ));
    assert_eq!(
        f.call(
            "settings/locale",
            json!({"locale":"en","last_workspace":other.root})
        ),
        Err(ActionError::InvalidArguments)
    );
    f.app
        .dispatch(
            Caller::Ui,
            Command::Trusted(TrustedCommand::OpenWorkspace(other.root.clone())),
        )
        .unwrap();
    let until = Instant::now() + Duration::from_secs(8);
    loop {
        f.app.poll();
        if let Some(result) = f.app.take_workspace_switch_result() {
            assert!(result.is_ok());
            break;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        f.call("notes/list", json!({})),
        Err(ActionError::SessionChanged)
    );
    assert_eq!(
        f.call("ai/refresh", json!({"version":"old"})),
        Err(ActionError::SessionChanged)
    );
    f.context = ToolContext::capture(&f.app).unwrap();
    assert_eq!(
        f.call("workspace/state", json!({})).unwrap()["path"],
        other.root.to_string_lossy().as_ref()
    );
    // Both applications have visited this directory; stop its writers before its owner is dropped.
    f.app.shutdown().unwrap();
}

#[test]
fn catalogue_registry_completeness_and_tool_schemas_enforce_the_trust_boundary() {
    let tools = tools::list();
    let mut names = std::collections::BTreeSet::new();
    for action in super::catalog::ACTIONS {
        assert!(names.insert(action.name));
        assert!(!action.name.contains('-'));
        match action.access {
            super::catalog::Access::Tool => {
                assert!(tools.iter().any(|tool| tool.name == action.name))
            }
            super::catalog::Access::Ui(reason) => {
                assert!(!reason.is_empty());
                assert!(!tools.iter().any(|tool| tool.name == action.name));
            }
        }
    }
    for tool in tools {
        let schema = tool.input_schema.to_string();
        assert!(!schema.contains("password"));
        assert!(!schema.contains("credential"));
        assert!(!schema.contains("last_workspace"));
    }
}

#[test]
fn rss_read_metadata_filters_refresh_and_outcome_run_headlessly_with_fixture_transport() {
    let mut f = Fixture::new();
    let created = f.accepted(
        "rss/create",
        json!({"url":"https://example.invalid/feed","categories":["Work"]}),
    );
    let id = created["Completed"]["id"].as_str().unwrap().to_owned();
    let read = f.accepted("rss/read", json!({"id":id}));
    let revision = read["Completed"]["metadata_version"].clone();
    let updated = f.accepted(
        "rss/metadata",
        json!({"id":id,"revision":revision,"title":"My feed","favorited":true}),
    );
    assert_eq!(updated["Completed"]["saved"], true);
    let conflict = f.accepted(
        "rss/metadata",
        json!({"id":id,"revision":revision,"title":"stale"}),
    );
    assert_eq!(conflict["Failed"], "Conflict");
    let refreshed = f.accepted("rss/refresh", json!({"id":id}));
    assert_eq!(refreshed["Completed"]["saved"], true);
    let read = f.accepted("rss/read", json!({"id":id}));
    assert!(read["Completed"]["entries"].as_array().unwrap().is_empty());
    assert_eq!(
        f.app
            .workspace
            .as_ref()
            .unwrap()
            .document()
            .unwrap()
            .title(),
        "Alpha"
    );
}

#[test]
fn headless_ai_journal_and_update_state_never_expose_credential_references() {
    let mut f = Fixture::new();
    f.global();
    let ai = f.call("ai/settings", json!({})).unwrap();
    assert!(ai["provider"].is_null());
    assert!(ai.get("pending_deletions").is_none());
    assert!(ai.get("connection").is_none());
    let journal = f.accepted("journal/list", json!({}));
    assert_eq!(journal["Completed"]["rows"], json!([]));
    assert!(journal["Completed"]["detail"].is_null());
    let retry = f.accepted("journal/retry", json!({}));
    assert_eq!(retry["Completed"]["blocked"], false);
    assert_eq!(f.call("updates/state", json!({})).unwrap()["busy"], false);
    assert_eq!(
        f.call("updates/restart", json!({})),
        Err(ActionError::NotFound)
    );
    let settings = f.call("settings/read", json!({})).unwrap();
    f.call(
        "settings/locale",
        json!({"locale":"ru","version":settings["version"]}),
    )
    .unwrap();
    assert_eq!(
        f.app.global.as_ref().unwrap().borrow().locale(),
        crate::i18n::Locale::Russian
    );
}

#[test]
fn protected_notes_require_ui_even_when_called_through_the_typed_dispatcher() {
    let mut f = Fixture::new();
    let path = f.root.join("notes/Protected.md");
    fs::write(
        &path,
        "---\ntitle: Protected\nstillus_encryption: age-body-v1\n---\n\nencrypted body\n",
    )
    .unwrap();
    f.app.shutdown().unwrap();
    f.app = Application::load(&f.root);
    f.context = ToolContext::capture(&f.app).unwrap();
    let id = f.id("Protected");
    assert_eq!(
        f.call("notes/read", json!({"id":id})),
        Err(ActionError::RequiresUserInteraction)
    );
    assert_eq!(
        f.call("security/disable", json!({"id":id})),
        Err(ActionError::RequiresUserInteraction)
    );
    assert!(matches!(
        f.app.query(
            f.context.caller(),
            Query::Read {
                id,
                offset: 0,
                limit: 100
            }
        ),
        Err(ActionError::RequiresUserInteraction)
    ));
}

#[test]
fn search_completes_with_addressable_identifiers_and_can_overlap_document_work() {
    let mut f = Fixture::new();
    let operation = f.call("search/query", json!({"query":"second"})).unwrap();
    let alpha = f.id("Alpha");
    let read = f.call("notes/read", json!({"id":alpha}));
    assert!(read.is_ok());
    let result = f.wait(operation["operation"].as_str().unwrap());
    let progress = f
        .call("operations/progress", json!({"id":operation["operation"]}))
        .unwrap();
    assert_eq!(progress["progress"]["phase"], "finished");
    assert_eq!(
        progress["progress"]["completed"],
        progress["progress"]["total"]
    );
    let hit = &result["Completed"]["results"][0];
    assert!(hit["id"].as_str().unwrap().starts_with("notes/"));
    assert!(f.call("notes/read", json!({"id":hit["id"]})).is_ok());
}

#[test]
fn version_budget_is_shared_by_documents_and_ai_and_never_replays_stale_edits() {
    let mut fixture = Fixture::new();
    fixture.global();
    let id = fixture.id("Alpha");
    let initial = fixture.call("notes/read", json!({"id":id})).unwrap();
    for _ in 0..257 {
        fixture.call("ai/settings", json!({})).unwrap();
    }
    assert_eq!(
        fixture.call(
            "notes/update",
            json!({"id":id,"version":initial["version"],"start":0,"end":0,"text":"stale"})
        ),
        Err(ActionError::Conflict)
    );
}

#[test]
fn bounded_operations_reject_work_before_starting_an_external_executor() {
    let mut fixture = Fixture::new();
    fixture.global();
    for index in 0..8 {
        fixture
            .app
            .test_workspace_mut()
            .unwrap()
            .operations
            .test_pending(&format!("operations/test/{index}"));
    }
    let ai = fixture.call("ai/settings", json!({})).unwrap();
    assert_eq!(
        fixture.call("ai/refresh", json!({"version":ai["version"]})),
        Err(ActionError::Busy)
    );
    assert!(!fixture.app.global.as_ref().unwrap().borrow().ai_busy());
    assert_eq!(
        fixture.call("journal/list", json!({})),
        Err(ActionError::Busy)
    );
    for index in 0..8 {
        fixture
            .call(
                "operations/cancel",
                json!({"id":format!("operations/test/{index}")}),
            )
            .unwrap();
    }
    assert_eq!(
        fixture.accepted("journal/list", json!({}))["Completed"]["rows"],
        json!([])
    );
}

#[test]
fn history_retains_the_last_completions_even_when_the_oldest_request_finishes_last() {
    use super::actions::{OperationOutput, Operations};
    let mut operations = Operations::default();
    let slow = operations.register(1, false).unwrap();
    let first = operations.register(1, false).unwrap();
    let done = || {
        Ok(OperationOutput::Effect {
            changed: false,
            saved: false,
        })
    };
    operations.finish(&first, done());
    for _ in 0..64 {
        let id = operations.register(1, false).unwrap();
        operations.finish(&id, done());
    }
    operations.finish(&slow, done());
    let mut fixture = Fixture::new();
    fixture.app.test_workspace_mut().unwrap().operations = operations;
    assert!(
        fixture
            .call("operations/status", json!({"id":slow}))
            .is_ok()
    );
    assert_eq!(
        fixture.call("operations/status", json!({"id":first})),
        Err(ActionError::NotFound)
    );
}

#[test]
fn trusted_confirmations_cannot_be_bypassed_through_typed_tool_commands() {
    let mut fixture = Fixture::new();
    fixture.global();
    assert!(matches!(
        fixture.app.dispatch(
            fixture.context.caller(),
            Command::Journal(super::global::JournalRequest {
                before: None,
                filter: Default::default(),
                selected: None,
                clear: true,
                retry: false,
            })
        ),
        Err(ActionError::RequiresUserInteraction)
    ));
    assert!(matches!(
        fixture.app.dispatch(
            fixture.context.caller(),
            Command::Discard {
                id: "any".into(),
                version: "any".into()
            }
        ),
        Err(ActionError::RequiresUserInteraction)
    ));
}

#[test]
fn rss_filters_reject_arbitrary_json_and_return_only_typed_public_fields() {
    let mut fixture = Fixture::new();
    let created = fixture.accepted("rss/create", json!({"url":"https://example.test/rss"}));
    let id = created["Completed"]["id"].as_str().unwrap();
    assert_eq!(fixture.call("rss/filters", json!({"id":id,"revision":0,"preferences":{"blacklist":"","whitelist":"","last_workspace":"/other"}})), Err(ActionError::InvalidArguments));
    let feed = fixture.accepted("rss/read", json!({"id":id}));
    let preferences = &feed["Completed"]["preferences"];
    assert_eq!(preferences.as_object().unwrap().len(), 3);
    let saved = fixture.accepted("rss/filters", json!({"id":id,"revision":preferences["version"],"preferences":{"blacklist":"ignore","whitelist":""}}));
    assert_eq!(saved["Completed"]["saved"], true);
    let fresh = fixture.accepted("rss/read", json!({"id":id}));
    assert_eq!(fresh["Completed"]["preferences"]["blacklist"], "ignore");
}

#[test]
fn headless_sort_uses_the_settings_worker_and_rejects_a_stale_catalogue() {
    let mut fixture = Fixture::new();
    let loaded = super::preferences::Preferences::load(&fixture.root);
    fixture.app.preferences = Some(Rc::new(RefCell::new(loaded.store)));
    let before = fixture.call("catalog/categories", json!({})).unwrap();
    let operation = fixture.call("catalog/sort", json!({"scope":"favorites","version":before["version"],"field":"modified","direction":"descending"})).unwrap();
    assert_eq!(operation["saved"], false);
    assert_eq!(
        fixture.call(
            "catalog/sort",
            json!({"scope":"favorites","version":before["version"],"field":"created"})
        ),
        Err(ActionError::Conflict)
    );
    fixture.clock.0.store(1000, Ordering::Relaxed);
    assert_eq!(
        fixture.wait(operation["operation"].as_str().unwrap())["Completed"]["saved"],
        true
    );
    let loaded = super::preferences::Preferences::load(&fixture.root);
    assert_eq!(
        loaded.settings.sidebar.note_sort[0].field,
        super::settings::NoteSortField::Modified
    );
    assert!(fixture.app.take_preferences_projection().is_some());
}

#[test]
fn settings_failure_finishes_the_operation_even_after_the_ui_reads_the_error() {
    let mut preferences = super::preferences::Preferences::unbound();
    let mut settings = super::settings::UiSettings::default();
    settings.window.width += 20.0;
    assert!(preferences.stage(settings));
    let revision = preferences.revision();
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        preferences.poll(1000);
        if preferences.take_error().is_some() {
            break;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(preferences.completion(revision).unwrap().is_err());
}

#[test]
fn inactive_recovery_preserves_the_editor_and_never_overwrites_a_conflicting_disk_file() {
    for conflict in [false, true] {
        let mut fixture = Fixture::new();
        let path = fixture.root.join("notes/Beta.md");
        {
            let mut seed = stillus_core::WorkspaceSession::open(&fixture.root).unwrap();
            let index = seed
                .notes()
                .iter()
                .position(|note| note.path == path)
                .unwrap();
            seed.open_note(index).unwrap();
            seed.apply_selected_at(stillus_core::EditorCommand::Insert("recovered ".into()), 0)
                .unwrap();
            let job = seed
                .begin_persistence(1000, "2026-09-09T00:00:00.000Z".into())
                .unwrap()
                .unwrap();
            seed.finish_persistence(job.execute()).unwrap();
        }
        if conflict {
            fs::write(&path, "Beta\nchanged outside\n").unwrap();
        }
        fixture.app.shutdown().unwrap();
        fixture.app = Application::load(&fixture.root);
        fixture.app.set_clock(fixture.clock.clone());
        fixture.context = ToolContext::capture(&fixture.app).unwrap();
        let alpha = fixture.id("Alpha");
        fixture.call("notes/open", json!({"id":alpha})).unwrap();
        fixture
            .app
            .apply(stillus_core::EditorCommand::Insert("visible draft ".into()));
        let draft = fixture.call("notes/read", json!({"id":alpha})).unwrap();
        let beta = fixture.id("Beta");
        let before = fixture.call("notes/read", json!({"id":beta})).unwrap();
        let restored = fixture.accepted(
            "notes/restore",
            json!({"id":beta,"version":before["version"]}),
        );
        if conflict {
            assert_eq!(restored["Failed"], "Conflict");
            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                "Beta\nchanged outside\n"
            );
        } else {
            assert_eq!(restored["Saved"]["saved"], true);
            assert!(
                fixture.call("notes/read", json!({"id":beta})).unwrap()["text"]
                    .as_str()
                    .unwrap()
                    .contains("recovered")
            );
        }
        assert_eq!(
            fixture.call("notes/read", json!({"id":alpha})).unwrap()["text"],
            draft["text"]
        );
        fixture
            .call(
                "editor/undo",
                json!({"id":alpha,"version":draft["version"]}),
            )
            .unwrap();
    }
}

#[test]
fn ordinary_settings_require_versions_and_merge_independent_fields() {
    let mut fixture = Fixture::new();
    fixture.global();
    let read = fixture.call("settings/read", json!({})).unwrap();
    fixture
        .call(
            "settings/locale",
            json!({"locale":"ru","version":read["version"]}),
        )
        .unwrap();
    fixture
        .call(
            "updates/automatic",
            json!({"enabled":false,"version":read["version"]}),
        )
        .unwrap();
    assert_eq!(
        fixture.call(
            "settings/locale",
            json!({"locale":"ar","version":read["version"]})
        ),
        Err(ActionError::Conflict)
    );
    let fresh = fixture.call("settings/read", json!({})).unwrap();
    assert_eq!(fresh["settings"], json!({"locale":"ru","automatic":false}));
}

#[test]
fn search_is_independent_of_other_background_coordinators() {
    let mut fixture = Fixture::new();
    let operation = fixture
        .app
        .test_workspace_mut()
        .unwrap()
        .operations
        .register(1, false)
        .unwrap();
    let result = fixture.accepted("search/query", json!({"query":"second"}));
    assert!(result["Completed"]["results"].as_array().unwrap().len() == 1);
    fixture.app.test_workspace_mut().unwrap().operations.finish(
        &operation,
        Ok(super::actions::OperationOutput::Effect {
            changed: false,
            saved: false,
        }),
    );
}

fn wait_workspace(
    app: &mut Application,
) -> Result<super::runtime::WorkspaceChanged, crate::i18n::UiText> {
    let until = Instant::now() + Duration::from_secs(8);
    loop {
        app.poll();
        if let Some(result) = app.take_workspace_switch_result() {
            return result;
        }
        assert!(
            Instant::now() < until,
            "workspace operation did not complete"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn gate_workspace_load(
    app: &mut Application,
) -> (
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::SyncSender<()>,
) {
    let (started, observed) = std::sync::mpsc::sync_channel(1);
    let (release, blocked) = std::sync::mpsc::sync_channel(1);
    let blocked = std::sync::Mutex::new(blocked);
    app.set_workspace_executor(Arc::new(move |path, initialize| {
        let loaded = super::runtime::prepare_workspace_load(path, initialize)?;
        started.send(()).unwrap();
        blocked
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(8))
            .unwrap();
        Ok(loaded)
    }));
    (observed, release)
}
#[test]
fn workspace_loading_is_background_owner_applied_bounded_and_preserves_clock() {
    let mut f = Fixture::new();
    let mut other = Fixture::new();
    other.app.shutdown().unwrap();
    let session = f.app.session_id();
    let (started, release) = gate_workspace_load(&mut f.app);
    let id = f.app.begin_workspace_switch(&other.root, false).unwrap();
    started.recv_timeout(Duration::from_secs(8)).unwrap();
    assert_eq!(f.app.session_id(), session);
    assert!(matches!(
        f.app.workspace_load_status(&id),
        Some(super::actions::OperationStatus::Running)
    ));
    assert_eq!(
        f.app
            .begin_workspace_switch(&other.root, false)
            .unwrap_err()
            .reason,
        ActionError::Busy
    );
    assert_eq!(
        f.call("operations/cancel", json!({"id":id})),
        Err(ActionError::RequiresUserInteraction)
    );
    assert!(!f.app.workspace_projection_pending());
    release.send(()).unwrap();
    // The executor cannot replace the owner even after its I/O completes.
    assert_eq!(f.app.session_id(), session);
    assert!(wait_workspace(&mut f.app).unwrap().changed);
    assert_ne!(f.app.session_id(), session);
    f.clock.0.store(4242, Ordering::Relaxed);
    assert_eq!(f.app.now_ms(), 4242);
    let status = f.app.query(Caller::Ui, Query::Operation(id)).unwrap();
    assert!(matches!(
        status,
        super::api::QueryResult::Action(super::actions::ActionResult::Status(
            super::actions::OperationStatus::Completed(_)
        ))
    ));
    assert!(matches!(
        f.call("notes/list", json!({})),
        Err(ActionError::SessionChanged)
    ));
    f.app.shutdown().unwrap();
}
#[test]
fn workspace_loading_rechecks_dirty_buffer_and_retains_failure() {
    let mut f = Fixture::new();
    let mut other = Fixture::new();
    other.app.shutdown().unwrap();
    let session = f.app.session_id();
    let note = f.id("Alpha");
    let (started, release) = gate_workspace_load(&mut f.app);
    let id = f.app.begin_workspace_switch(&other.root, false).unwrap();
    started.recv_timeout(Duration::from_secs(8)).unwrap();
    f.app
        .apply(stillus_core::EditorCommand::Insert("must survive ".into()));
    release.send(()).unwrap();
    assert!(wait_workspace(&mut f.app).is_err());
    assert_eq!(f.app.session_id(), session);
    assert!(
        f.call("notes/read", json!({"id":note})).unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("must survive")
    );
    assert_eq!(
        f.call("operations/status", json!({"id":id})).unwrap()["Failed"],
        "Busy"
    );
}
#[test]
fn workspace_load_cancel_and_shutdown_dispose_prepared_search_workers() {
    let mut f = Fixture::new();
    let mut other = Fixture::new();
    other.app.shutdown().unwrap();
    let session = f.app.session_id();
    let (started, release) = gate_workspace_load(&mut f.app);
    let id = f.app.begin_workspace_switch(&other.root, false).unwrap();
    started.recv_timeout(Duration::from_secs(8)).unwrap();
    assert_eq!(f.app.cancel_workspace_load(&id), Some(true));
    release.send(()).unwrap();
    assert!(wait_workspace(&mut f.app).is_err());
    assert_eq!(f.app.session_id(), session);
    assert_eq!(
        f.call("operations/status", json!({"id":id})).unwrap(),
        "Cancelled"
    );
    let (started, release) = gate_workspace_load(&mut f.app);
    f.app.begin_workspace_switch(&other.root, false).unwrap();
    started.recv_timeout(Duration::from_secs(8)).unwrap();
    release.send(()).unwrap();
    f.app.shutdown().unwrap();
    // Shutdown joins the loader and its prepared search worker, so removal is safe now.
    fs::remove_dir_all(other.root.join(".stillus")).unwrap();
    assert!(f.app.session_id().is_none());
}
#[test]
fn workspace_initialization_and_unloaded_failure_have_queryable_completions() {
    let root = crate::test_support::workspace("stillus-headless-workspace-init");
    let mut app = Application::unloaded();
    assert!(matches!(
        app.dispatch(
            Caller::Ui,
            Command::Trusted(TrustedCommand::OpenWorkspace(PathBuf::from("relative")))
        ),
        Err(ActionError::InvalidArguments)
    ));
    let missing = root.join("missing");
    let id = app.begin_workspace_switch(&missing, false).unwrap();
    assert!(wait_workspace(&mut app).is_err());
    assert!(matches!(
        app.query(Caller::Ui, Query::Operation(id)),
        Ok(super::api::QueryResult::Action(
            super::actions::ActionResult::Status(super::actions::OperationStatus::Failed(_))
        ))
    ));
    app.dispatch(
        Caller::Ui,
        Command::Trusted(TrustedCommand::InitializeWorkspace(root.clone())),
    )
    .unwrap();
    assert!(wait_workspace(&mut app).unwrap().changed);
    assert!(root.join("notes").is_dir());
    let session = app.session_id();
    app.begin_workspace_switch(&root.join("."), false).unwrap();
    assert!(!wait_workspace(&mut app).unwrap().changed);
    assert_eq!(app.session_id(), session);
    app.shutdown().unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn status_poll_reauthorizes_the_tool_after_loading_changes_the_session() {
    let mut f = Fixture::new();
    let mut other = Fixture::new();
    other.app.shutdown().unwrap();
    let (started, release) = gate_workspace_load(&mut f.app);
    let id = f.app.begin_workspace_switch(&other.root, false).unwrap();
    started.recv_timeout(Duration::from_secs(8)).unwrap();
    release.send(()).unwrap();
    let until = Instant::now() + Duration::from_secs(8);
    loop {
        match f.call("operations/progress", json!({"id":id})) {
            Err(ActionError::SessionChanged) => break,
            Ok(value) => assert_eq!(value["status"], "Running"),
            other => panic!("unexpected old-session response: {other:?}"),
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(f.app.take_workspace_switch_result().unwrap().is_ok());
    f.app.shutdown().unwrap();
}

#[test]
fn shutdown_finishes_an_addressed_write_before_releasing_its_workspace() {
    let mut f = Fixture::new();
    let id = f.id("Beta");
    let read = f.call("notes/read", json!({"id":id})).unwrap();
    let accepted = f
        .call(
            "notes/update",
            json!({"id":id,"version":read["version"],"start":5,"end":11,"text":"durable"}),
        )
        .unwrap();
    assert!(accepted["operation"].is_string());
    f.app.shutdown().unwrap();
    assert!(
        fs::read_to_string(f.root.join("notes/Beta.md"))
            .unwrap()
            .contains("durable")
    );
}
