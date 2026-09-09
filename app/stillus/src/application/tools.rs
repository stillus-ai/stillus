// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]
//! JSON boundary for direct Rust tool calls. Domain code uses typed values.
pub(crate) use super::actions::{Action, ActionError, Operations};
#[cfg(test)]
use super::workspace::Workspace;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[cfg(test)]
use std::path::PathBuf;
use stillus_core::{NoteEdit, NoteMetadataEdit};
#[derive(Serialize)]
pub(crate) struct ToolDescriptor {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
}

pub(crate) fn list() -> Vec<ToolDescriptor> {
    use super::catalog::{Access, Handler as H};
    let string = json!({"type":"string"});
    let integer = json!({"type":"integer","minimum":0});
    let boolean = json!({"type":"boolean"});
    let strings = json!({"type":"array","items":string,"maxItems":100});
    super::catalog::ACTIONS.iter().filter(|action| action.access == Access::Tool).map(|action| {
        let (properties, required) = match action.handler {
            H::NotesList | H::ExternalList => (json!({"offset":integer,"limit":integer}),vec![]),
            H::NotesRead => (json!({"id":string,"offset":integer,"limit":integer}),vec!["id"]),
            H::NotesUpdate => (json!({"id":string,"version":string,"start":integer,"end":integer,"text":string}),vec!["id","version","start","end","text"]),
            H::NotesCreate => (json!({"title":string}),vec!["title"]),
            H::NotesRename => (json!({"id":string,"version":string,"title":string}),vec!["id","version","title"]),
            H::NotesMetadata => (json!({"id":string,"version":string,"tags":strings,"pinned":boolean,"favorited":boolean,"deleted":boolean}),vec!["id","version"]),
            H::NotesOpen | H::OperationStatus | H::OperationProgress | H::OperationCancel | H::SecurityDisable | H::JournalRead => (json!({"id":string}),vec!["id"]),
            H::NotesSave | H::NotesRestore | H::ExternalClose | H::EditorUndo | H::EditorRedo => (json!({"id":string,"version":string}),vec!["id","version"]),
            H::ExternalOpen => (json!({"path":string}),vec!["path"]),
            H::CatalogOrder => (json!({"scope":string,"items":strings,"version":string}),vec!["scope","items","version"]),
            H::CatalogSort => (json!({"scope":string,"field":{"enum":["name","created","modified"]},"direction":{"enum":["ascending","descending"]},"version":string}),vec!["scope","version"]),
            H::CategoriesOrder => (json!({"categories":strings,"version":string}),vec!["categories","version"]),
            H::CatalogOrderClear => (json!({"scope":string,"version":string}),vec!["scope","version"]),
            H::SearchQuery => (json!({"query":string}),vec!["query"]),
            H::SearchDocument => (json!({"id":string,"version":string,"query":string,"limit":integer}),vec!["id","version","query"]),
            H::RssRefresh => (json!({"id":string}),vec!["id"]),
            H::RssCreate => (json!({"url":string,"categories":strings,"favorited":boolean}),vec!["url"]),
            H::RssMetadata => (json!({"id":string,"revision":integer,"title":string,"categories":strings,"pinned":boolean,"favorited":boolean,"deleted":boolean}),vec!["id","revision"]),
            H::RssRead => (json!({"id":string,"offset":integer,"limit":integer}),vec!["id"]),
            H::RssMarkRead => (json!({"id":string,"revision":integer,"entry":string}),vec!["id","revision","entry"]),
            H::RssFilters => (json!({"id":string,"revision":integer,"preferences":{"type":"object","properties":{"blacklist":string,"whitelist":string,"version":integer},"required":["blacklist","whitelist"],"additionalProperties":false},"apply":boolean}),vec!["id","revision","preferences"]),
            H::SettingsLocale => (json!({"locale":string,"version":string}),vec!["locale","version"]),
            H::AiRefresh | H::AiDisconnect | H::AiCleanup => (json!({"version":string}),vec!["version"]),
            H::AiAliasSave => (json!({"version":string,"old":string,"name":string,"model":string,"effort":string}),vec!["version","name","model"]),
            H::AiAliasRemove => (json!({"version":string,"name":string}),vec!["version","name"]),
            H::UpdatesAutomatic => (json!({"enabled":boolean,"version":string}),vec!["enabled","version"]),
            H::JournalList => (json!({"before":string,"provider":string,"status":string}),vec![]),
            _ => (json!({}),vec![]),
        };
        ToolDescriptor { name:action.name, description:action.description, input_schema:json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}) }
    }).collect()
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Arguments {
    id: Option<String>,
    version: Option<String>,
    offset: Option<usize>,
    limit: Option<usize>,
    start: Option<usize>,
    end: Option<usize>,
    text: Option<String>,
    title: Option<String>,
    query: Option<String>,
    tags: Option<Vec<String>>,
    pinned: Option<bool>,
    favorited: Option<bool>,
    deleted: Option<bool>,
    path: Option<String>,
    scope: Option<String>,
    field: Option<super::settings::NoteSortField>,
    direction: Option<super::settings::SortDirection>,
    items: Option<Vec<String>>,
    url: Option<String>,
    categories: Option<Vec<String>>,
    revision: Option<u64>,
    entry: Option<String>,
    preferences: Option<super::rss::FilterSettings>,
    apply: Option<bool>,
    locale: Option<crate::i18n::Locale>,
    enabled: Option<bool>,
    old: Option<String>,
    name: Option<String>,
    model: Option<String>,
    effort: Option<stillus_ai::AiEffort>,
    before: Option<String>,
    provider: Option<stillus_ai::AiProvider>,
    status: Option<stillus_ai::journal::RequestStatus>,
}

#[cfg(test)]
fn call_workspace(
    workspace: &mut Workspace,
    name: &str,
    value: Value,
    now_ms: u64,
) -> Result<Value, ActionError> {
    let descriptor = list()
        .into_iter()
        .find(|tool| tool.name == name)
        .ok_or(ActionError::NotFound)?;
    let object = value.as_object().ok_or(ActionError::InvalidArguments)?;
    if object
        .keys()
        .any(|key| descriptor.input_schema["properties"].get(key).is_none())
    {
        return Err(ActionError::InvalidArguments);
    }
    let a: Arguments = serde_json::from_value(value).map_err(|_| ActionError::InvalidArguments)?;
    let required = |value: Option<String>| value.ok_or(ActionError::InvalidArguments);
    let action = match name {
        "notes/list" => Action::List {
            offset: a.offset.unwrap_or(0),
            limit: a.limit.unwrap_or(100),
        },
        "notes/read" => Action::Read {
            id: required(a.id)?,
            offset: a.offset.unwrap_or(0),
            limit: a.limit.unwrap_or(65536),
        },
        "notes/update" => Action::Edit {
            id: required(a.id)?,
            version: required(a.version)?,
            edit: NoteEdit {
                start: a.start.ok_or(ActionError::InvalidArguments)?,
                end: a.end.ok_or(ActionError::InvalidArguments)?,
                text: required(a.text)?,
            },
        },
        "notes/create" => Action::Create {
            title: required(a.title)?,
        },
        "notes/rename" => Action::Rename {
            id: required(a.id)?,
            version: required(a.version)?,
            title: required(a.title)?,
        },
        "notes/metadata" => Action::Metadata {
            id: required(a.id)?,
            version: required(a.version)?,
            edit: NoteMetadataEdit {
                tags: a.tags,
                pinned: a.pinned,
                favorited: a.favorited,
                deleted: a.deleted,
            },
        },
        "notes/open" => Action::Open {
            id: required(a.id)?,
        },
        "search/query" => Action::Search {
            query: required(a.query)?,
        },
        "rss/list" => Action::RssList,
        "rss/refresh" => Action::RssRefresh {
            id: required(a.id)?,
        },
        "operations/status" => Action::Status {
            id: required(a.id)?,
        },
        "operations/cancel" => Action::Cancel {
            id: required(a.id)?,
        },
        _ => return Err(ActionError::NotFound),
    };
    workspace
        .execute_action(action, now_ms)
        .and_then(|result| serde_json::to_value(result).map_err(|_| ActionError::InvalidArguments))
}

#[cfg(test)]
mod tests {
    use super::call_workspace as call;
    use super::*;
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
        time::{Duration, Instant},
    };
    use stillus_core::{EditorCommand, initialize_workspace};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture {
        root: PathBuf,
        workspace: Workspace,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "stillus-actions-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            initialize_workspace(&root).unwrap();
            fs::write(
                root.join("notes/alpha.md"),
                "---\ntitle: Alpha\ncustom: preserve\n---\n\nAlpha\nbody\n",
            )
            .unwrap();
            fs::write(
                root.join("notes/beta.md"),
                "---\ntitle: Beta\ncustom: preserve\n---\n\nBeta\nbody\n",
            )
            .unwrap();
            let workspace = Workspace::open(&root).unwrap();
            Self { root, workspace }
        }
        fn id(&mut self, title: &str) -> String {
            self.workspace
                .targets()
                .into_iter()
                .find(|(_, n)| n.title == title)
                .unwrap()
                .0
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).unwrap();
        }
    }
    fn read(workspace: &mut Workspace, id: &str) -> Value {
        call(workspace, "notes/read", json!({"id":id}), 0).unwrap()
    }
    fn wait(workspace: &mut Workspace, id: &str) -> Value {
        let start = Instant::now();
        loop {
            workspace.poll_actions();
            let status = call(workspace, "operations/status", json!({"id":id}), 0).unwrap();
            if status.get("Saved").is_some() {
                return status;
            }
            assert!(status.get("Failed").is_none(), "{status}");
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn ui_adapter_and_tools_apply_the_same_document_edit() {
        let left = Fixture::new();
        let mut right = Fixture::new();
        let mut ui = crate::AppModel::load(&left.root);
        let index = ui
            .workspace
            .as_ref()
            .unwrap()
            .notes()
            .iter()
            .position(|note| note.title == "Alpha")
            .unwrap();
        ui.open_note(index);
        let id = right.id("Alpha");
        call(&mut right.workspace, "notes/open", json!({"id":id}), 0).unwrap();
        let original = read(&mut right.workspace, &id);
        ui.apply(EditorCommand::ReplaceRange {
            start: 0,
            end: 5,
            text: "Общий текст".into(),
        });
        call(
            &mut right.workspace,
            "notes/update",
            json!({"id":id,"version":original["version"],"start":0,"end":5,"text":"Общий текст"}),
            1,
        )
        .unwrap();
        let ui_workspace = ui.workspace.as_mut().unwrap();
        let path = ui_workspace.notes()[ui_workspace.selected_note().unwrap()]
            .path
            .clone();
        let ui_id = ui_workspace.target_id(&path).unwrap();
        assert_eq!(
            read(ui_workspace, &ui_id)["text"],
            read(&mut right.workspace, &id)["text"]
        );
        ui.apply(EditorCommand::Undo);
        assert_eq!(
            read(ui.workspace.as_mut().unwrap(), &ui_id)["text"],
            original["text"]
        );
        ui.shutdown_search_worker();
    }

    #[test]
    fn rename_through_ui_keeps_identity_and_pending_operations_can_be_cancelled() {
        let mut f = Fixture::new();
        let id = f.id("Alpha");
        call(&mut f.workspace, "notes/open", json!({"id":id}), 0).unwrap();
        let before = read(&mut f.workspace, &id);
        f.workspace
            .rename_selected("New title", "2026-09-09T00:00:00.000Z")
            .unwrap();
        assert_eq!(read(&mut f.workspace, &id)["text"], before["text"]);
        assert_eq!(
            f.workspace
                .resolve_target(&id)
                .unwrap()
                .file_stem()
                .unwrap(),
            "New title"
        );
        assert_eq!(
            call(
                &mut f.workspace,
                "notes/update",
                json!({"id":id,"version":before["version"],"start":0,"end":0,"text":"stale"}),
                0
            ),
            Err(ActionError::Conflict)
        );
        let operation = "operations/test/pending";
        f.workspace.operations.test_pending(operation);
        assert!(f.workspace.actions_busy());
        assert_eq!(
            call(
                &mut f.workspace,
                "operations/cancel",
                json!({"id":operation}),
                0
            )
            .unwrap()["cancelled"],
            true
        );
        assert!(!f.workspace.actions_busy());
        assert_eq!(
            call(
                &mut f.workspace,
                "operations/status",
                json!({"id":operation}),
                0
            )
            .unwrap(),
            json!("Cancelled")
        );
        assert_eq!(
            call(
                &mut Workspace::open(&f.root).unwrap(),
                "operations/status",
                json!({"id":operation}),
                0
            ),
            Err(ActionError::NotFound)
        );
    }

    #[test]
    fn actions_respect_active_saves_and_unresolved_recovery() {
        let mut f = Fixture::new();
        let id = f.id("Alpha");
        call(&mut f.workspace, "notes/open", json!({"id":id}), 0).unwrap();
        f.workspace
            .apply_selected_at(EditorCommand::Insert("draft ".into()), 1)
            .unwrap();
        let before = read(&mut f.workspace, &id);
        let save = f
            .workspace
            .begin_autosave(10_000, "2026-09-09T00:00:00.000Z".into())
            .unwrap()
            .unwrap();
        assert_eq!(
            call(
                &mut f.workspace,
                "notes/update",
                json!({"id":id,"version":before["version"],"start":0,"end":0,"text":"blocked"}),
                2
            ),
            Err(ActionError::Busy)
        );
        assert_eq!(
            call(
                &mut f.workspace,
                "notes/create",
                json!({"title":"Before Alpha"}),
                2
            ),
            Err(ActionError::Busy)
        );
        let beta = f.id("Beta");
        let beta_read = read(&mut f.workspace, &beta);
        assert_eq!(
            call(
                &mut f.workspace,
                "notes/metadata",
                json!({"id":beta,"version":beta_read["version"],"pinned":true}),
                2
            ),
            Err(ActionError::Busy)
        );
        assert_eq!(
            call(
                &mut f.workspace,
                "notes/update",
                json!({"id":beta,"version":beta_read["version"],"start":0,"end":0,"text":"blocked"}),
                2
            ),
            Err(ActionError::Busy)
        );
        f.workspace.finish_autosave(save.execute()).unwrap();
        f.workspace
            .apply_selected_at(EditorCommand::Insert("recovery ".into()), 20_000)
            .unwrap();
        let recovery = f
            .workspace
            .begin_persistence(21_000, "2026-09-09T00:00:00.000Z".into())
            .unwrap()
            .unwrap();
        f.workspace.finish_persistence(recovery.execute()).unwrap();
        let mut reopened = Workspace::open(&f.root).unwrap();
        let index = reopened
            .notes()
            .iter()
            .position(|n| n.recovery_available)
            .unwrap();
        let path = reopened.notes()[index].path.clone();
        let target = reopened.target_id(&path).unwrap();
        reopened.open_note(index).unwrap();
        let before = read(&mut reopened, &target);
        assert_eq!(
            call(
                &mut reopened,
                "notes/update",
                json!({"id":target,"version":before["version"],"start":0,"end":0,"text":"blocked"}),
                2
            ),
            Err(ActionError::Busy)
        );
    }

    #[test]
    fn typed_and_tool_calls_share_current_buffer_version_and_undo() {
        let mut f = Fixture::new();
        let id = f.id("Alpha");
        call(&mut f.workspace, "notes/open", json!({"id":id}), 0).unwrap();
        f.workspace
            .apply_selected_at(EditorCommand::Insert("draft ".into()), 1)
            .unwrap();
        let before = read(&mut f.workspace, &id);
        assert!(before["text"].as_str().unwrap().contains("draft"));
        let result = f
            .workspace
            .execute_action(
                Action::Edit {
                    id: id.clone(),
                    version: before["version"].as_str().unwrap().into(),
                    edit: NoteEdit {
                        start: 0,
                        end: 6,
                        text: "edited ".into(),
                    },
                },
                2,
            )
            .unwrap();
        assert!(matches!(
            result,
            super::super::actions::ActionResult::Applied { saved: false, .. }
        ));
        assert!(
            f.workspace.poll_actions(),
            "owner must redraw and schedule persistence even when search is closed"
        );
        assert!(f.workspace.next_persistence_deadline().is_some());
        assert_eq!(
            call(
                &mut f.workspace,
                "notes/update",
                json!({"id":id,"version":before["version"],"start":0,"end":0,"text":"stale"}),
                3
            ),
            Err(ActionError::Conflict)
        );
        f.workspace
            .apply_selected_at(EditorCommand::Undo, 4)
            .unwrap();
        assert_eq!(read(&mut f.workspace, &id)["text"], before["text"]);
        assert!(
            !fs::read_to_string(f.root.join("notes/alpha.md"))
                .unwrap()
                .contains("draft")
        );
    }

    #[test]
    fn background_write_preserves_editor_and_unknown_yaml_and_tracks_rename() {
        let mut f = Fixture::new();
        let alpha = f.id("Alpha");
        let beta = f.id("Beta");
        call(&mut f.workspace, "notes/open", json!({"id":alpha}), 0).unwrap();
        f.workspace
            .apply_selected_at(EditorCommand::Insert("unsaved ".into()), 1)
            .unwrap();
        let selected = read(&mut f.workspace, &alpha);
        let before = read(&mut f.workspace, &beta);
        let result = call(
            &mut f.workspace,
            "notes/update",
            json!({"id":beta,"version":before["version"],"start":0,"end":4,"text":"Renamed"}),
            2,
        )
        .unwrap();
        assert_eq!(result["saved"], false);
        wait(&mut f.workspace, result["operation"].as_str().unwrap());
        assert_eq!(read(&mut f.workspace, &alpha)["text"], selected["text"]);
        assert_eq!(f.workspace.document().unwrap().title(), "unsaved Alpha");
        assert!(
            read(&mut f.workspace, &beta)["text"]
                .as_str()
                .unwrap()
                .starts_with("Renamed")
        );
        let path = f.workspace.resolve_target(&beta).unwrap();
        assert!(
            fs::read_to_string(path)
                .unwrap()
                .contains("custom: preserve")
        );
        assert_eq!(
            f.workspace
                .targets()
                .into_iter()
                .find(|(id, _)| id == &beta)
                .unwrap()
                .1
                .title,
            "Renamed"
        );
    }

    #[test]
    fn stale_file_workspace_and_protected_targets_are_rejected() {
        let mut f = Fixture::new();
        let id = f.id("Alpha");
        let original = read(&mut f.workspace, &id);
        fs::write(f.root.join("notes/alpha.md"), "External\n").unwrap();
        assert_eq!(
            call(
                &mut f.workspace,
                "notes/update",
                json!({"id":id,"version":original["version"],"start":0,"end":0,"text":"bad"}),
                0
            ),
            Err(ActionError::Conflict)
        );
        let mut fresh = Workspace::open(&f.root).unwrap();
        assert_eq!(
            call(&mut fresh, "notes/read", json!({"id":id}), 0),
            Err(ActionError::NotFound)
        );
        fs::write(
            f.root.join("notes/protected.md"),
            "---\ntitle: Protected\nstillus_encryption: age-body-v1\n---\n\nnot plaintext\n",
        )
        .unwrap();
        let mut protected = Workspace::open(&f.root).unwrap();
        let id = protected
            .targets()
            .into_iter()
            .find(|(_, n)| n.path.ends_with("protected.md"))
            .unwrap()
            .0;
        assert_eq!(
            call(&mut protected, "notes/read", json!({"id":id}), 0),
            Err(ActionError::RequiresUserInteraction)
        );
        assert!(matches!(
            protected.execute_action(Action::UserInteraction, 0),
            Err(ActionError::RequiresUserInteraction)
        ));
    }

    #[test]
    fn metadata_and_create_do_not_select_another_note() {
        let mut f = Fixture::new();
        let alpha = f.id("Alpha");
        let beta = f.id("Beta");
        call(&mut f.workspace, "notes/open", json!({"id":alpha}), 0).unwrap();
        let selected = f.workspace.document().unwrap().title().to_owned();
        let before = read(&mut f.workspace, &beta);
        call(
            &mut f.workspace,
            "notes/metadata",
            json!({"id":beta,"version":before["version"],"tags":["Project"],"pinned":true}),
            0,
        )
        .unwrap();
        assert_eq!(f.workspace.document().unwrap().title(), selected);
        assert!(
            f.workspace
                .targets()
                .into_iter()
                .find(|(id, _)| id == &beta)
                .unwrap()
                .1
                .pinned
        );
        let created = call(&mut f.workspace, "notes/create", json!({"title":"New"}), 0).unwrap();
        assert_ne!(created["id"], alpha);
        assert_eq!(f.workspace.document().unwrap().title(), "Alpha");
        assert_eq!(
            call(
                &mut f.workspace,
                "notes/read",
                json!({"id":alpha,"password":"wrong"}),
                0
            ),
            Err(ActionError::InvalidArguments)
        );
        assert_eq!(
            call(&mut f.workspace, "unknown", json!({}), 0),
            Err(ActionError::NotFound)
        );
    }
}

pub(crate) fn call(
    app: &mut super::Application,
    context: super::api::ToolContext,
    name: &str,
    value: Value,
) -> Result<Value, ActionError> {
    app.authorize(context.caller())?;
    use super::{
        api::{Command as C, Query as Q},
        catalog::{Access, Handler as H},
        rss::Addressed as R,
    };
    let descriptor = list()
        .into_iter()
        .find(|tool| tool.name == name)
        .ok_or(ActionError::NotFound)?;
    let object = value.as_object().ok_or(ActionError::InvalidArguments)?;
    if object
        .keys()
        .any(|key| descriptor.input_schema["properties"].get(key).is_none())
    {
        return Err(ActionError::InvalidArguments);
    }
    let a: Arguments = serde_json::from_value(value).map_err(|_| ActionError::InvalidArguments)?;
    let handler = super::catalog::ACTIONS
        .iter()
        .find(|action| action.name == name && action.access == Access::Tool)
        .ok_or(ActionError::NotFound)?
        .handler;
    let required = |value: Option<String>| value.ok_or(ActionError::InvalidArguments);
    let item = |value: Option<String>| {
        stillus_core::ItemId::new(value.ok_or(ActionError::InvalidArguments)?)
            .map_err(|_| ActionError::InvalidArguments)
    };
    let scope = |value: Option<String>| -> Result<super::runtime::SidebarFilter, ActionError> {
        let value = required(value)?;
        Ok(if value == "favorites" {
            super::runtime::SidebarFilter::Favorites
        } else {
            super::runtime::SidebarFilter::Tag(value)
        })
    };
    let query = match handler {
        H::WorkspaceState => Some(Q::Workspace),
        H::SettingsRead => Some(Q::Settings),
        H::NotesList => Some(Q::Notes {
            offset: a.offset.unwrap_or(0),
            limit: a.limit.unwrap_or(100),
        }),
        H::NotesRead => Some(Q::Read {
            id: required(a.id.clone())?,
            offset: a.offset.unwrap_or(0),
            limit: a.limit.unwrap_or(65536),
        }),
        H::ExternalList => Some(Q::External {
            offset: a.offset.unwrap_or(0),
            limit: a.limit.unwrap_or(100),
        }),
        H::Categories => Some(Q::Categories),
        H::SearchDocument => Some(Q::Find {
            id: required(a.id.clone())?,
            version: required(a.version.clone())?,
            text: required(a.query.clone())?,
            limit: a.limit.unwrap_or(100),
        }),
        H::AiSettings => Some(Q::Ai),
        H::UpdatesState => Some(Q::Updates),
        H::OperationStatus => Some(Q::Operation(required(a.id.clone())?)),
        H::OperationProgress => Some(Q::OperationProgress(required(a.id.clone())?)),
        _ => None,
    };
    if let Some(query) = query {
        return serde_json::to_value(app.query(context.caller(), query)?)
            .map_err(|_| ActionError::InvalidArguments);
    }
    let command = match handler {
        H::NotesUpdate => C::Notes(Action::Edit {
            id: required(a.id)?,
            version: required(a.version)?,
            edit: NoteEdit {
                start: a.start.ok_or(ActionError::InvalidArguments)?,
                end: a.end.ok_or(ActionError::InvalidArguments)?,
                text: required(a.text)?,
            },
        }),
        H::NotesCreate => C::Notes(Action::Create {
            title: required(a.title)?,
        }),
        H::NotesRename => C::Notes(Action::Rename {
            id: required(a.id)?,
            version: required(a.version)?,
            title: required(a.title)?,
        }),
        H::NotesMetadata => C::Notes(Action::Metadata {
            id: required(a.id)?,
            version: required(a.version)?,
            edit: NoteMetadataEdit {
                tags: a.tags,
                pinned: a.pinned,
                favorited: a.favorited,
                deleted: a.deleted,
            },
        }),
        H::NotesOpen => C::Notes(Action::Open {
            id: required(a.id)?,
        }),
        H::NotesSave => C::Save {
            id: required(a.id)?,
            version: required(a.version)?,
        },
        H::NotesRestore => C::Restore {
            id: required(a.id)?,
            version: required(a.version)?,
        },
        H::ExternalOpen => C::ExternalOpen {
            path: required(a.path)?.into(),
        },
        H::ExternalClose => C::ExternalClose {
            id: required(a.id)?,
            version: required(a.version)?,
        },
        H::EditorUndo | H::EditorRedo => C::Editor {
            id: required(a.id)?,
            version: required(a.version)?,
            command: if handler == H::EditorUndo {
                stillus_core::EditorCommand::Undo
            } else {
                stillus_core::EditorCommand::Redo
            },
        },
        H::CatalogOrder => C::Order {
            scope: scope(a.scope)?,
            items: a.items.ok_or(ActionError::InvalidArguments)?,
            version: required(a.version)?,
        },
        H::CatalogSort => C::Sort {
            scope: scope(a.scope)?,
            field: a.field,
            direction: a
                .direction
                .unwrap_or(super::settings::SortDirection::Ascending),
            version: required(a.version)?,
        },
        H::CategoriesOrder => C::CategoryOrder {
            categories: a.categories.ok_or(ActionError::InvalidArguments)?,
            version: required(a.version)?,
        },
        H::CatalogOrderClear => C::ClearOrder {
            scope: scope(a.scope)?,
            version: required(a.version)?,
        },
        H::SearchQuery => C::Notes(Action::Search {
            query: required(a.query)?,
        }),
        H::SearchRebuild => C::RebuildSearch,
        H::RssList => C::Notes(Action::RssList),
        H::RssRefresh => C::Notes(Action::RssRefresh {
            id: required(a.id)?,
        }),
        H::RssCreate => C::Rss(R::Create {
            url: required(a.url)?,
            categories: a.categories.unwrap_or_default(),
            favorited: a.favorited.unwrap_or(false),
        }),
        H::RssMetadata => C::Rss(R::Metadata {
            id: item(a.id)?,
            version: a.revision.ok_or(ActionError::InvalidArguments)?,
            patch: stillus_engine::CommonMetadataPatch {
                title: a.title,
                categories: a.categories,
                pinned: a.pinned,
                favorited: a.favorited,
                deleted: a.deleted,
                ..Default::default()
            },
        }),
        H::RssRead => C::Rss(R::Read {
            id: item(a.id)?,
            offset: a.offset.unwrap_or(0),
            limit: a.limit.unwrap_or(40),
        }),
        H::RssMarkRead => C::Rss(R::MarkRead {
            id: item(a.id)?,
            version: a.revision.ok_or(ActionError::InvalidArguments)?,
            entry: required(a.entry)?,
        }),
        H::RssFilters => C::Rss(R::Filters {
            id: item(a.id)?,
            version: a.revision.ok_or(ActionError::InvalidArguments)?,
            preferences: a.preferences.ok_or(ActionError::InvalidArguments)?,
            apply: a.apply.unwrap_or(false),
        }),
        H::SettingsLocale => C::Locale {
            value: a.locale.ok_or(ActionError::InvalidArguments)?,
            version: required(a.version)?,
        },
        H::AiRefresh | H::AiDisconnect | H::AiCleanup | H::AiAliasSave | H::AiAliasRemove => {
            let expected = app.ai_expected(&required(a.version)?)?;
            let action = match handler {
                H::AiRefresh => super::ai::Action::Refresh,
                H::AiDisconnect => super::ai::Action::Disconnect,
                H::AiCleanup => super::ai::Action::Cleanup,
                H::AiAliasRemove => super::ai::Action::Remove(required(a.name)?),
                H::AiAliasSave => super::ai::Action::Save {
                    old: a.old,
                    name: required(a.name)?,
                    profile: stillus_ai::AiProfile {
                        model: required(a.model)?,
                        effort: a.effort,
                    },
                },
                _ => unreachable!(),
            };
            C::Ai { expected, action }
        }
        H::SecurityDisable => C::DisableProtection {
            id: required(a.id)?,
        },
        H::UpdatesCheck => C::CheckUpdates,
        H::UpdatesInstall => C::InstallUpdate,
        H::UpdatesAutomatic => C::UpdateAutomatic {
            value: a.enabled.ok_or(ActionError::InvalidArguments)?,
            version: required(a.version)?,
        },
        H::JournalList | H::JournalRead | H::JournalRetry => {
            C::Journal(super::global::JournalRequest {
                before: a.before,
                filter: super::journal::Filter {
                    provider: a.provider,
                    status: a.status,
                },
                selected: if handler == H::JournalRead {
                    Some(required(a.id)?)
                } else {
                    None
                },
                clear: false,
                retry: handler == H::JournalRetry,
            })
        }
        H::OperationCancel => C::Notes(Action::Cancel {
            id: required(a.id)?,
        }),
        // UI-only handlers are rejected both by the catalogue and the typed dispatcher.
        _ => return Err(ActionError::RequiresUserInteraction),
    };
    serde_json::to_value(app.dispatch(context.caller(), command)?)
        .map_err(|_| ActionError::InvalidArguments)
}
