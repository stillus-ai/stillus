// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]

//! Every public action declares its typed handler and its trust boundary here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Access {
    Tool,
    Ui(&'static str),
}
macro_rules! catalogue {
    ($( $kind:ident => ($name:literal, $description:literal, $access:expr) ),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub(crate) enum Handler { $( $kind ),+ }
        pub(crate) const ACTIONS:&[Definition] = &[$(Definition { handler:Handler::$kind, name:$name, description:$description, access:$access }),+];
    }
}
pub(crate) struct Definition {
    pub handler: Handler,
    pub name: &'static str,
    pub description: &'static str,
    pub access: Access,
}
catalogue! {
    WorkspaceState => ("workspace/state", "Read the current workspace session", Access::Tool),
    WorkspaceOpen => ("workspace/open", "Open and switch workspace", Access::Ui("Workspace selection belongs to trusted UI")),
    WorkspaceInitialize => ("workspace/initialize", "Initialize a workspace", Access::Ui("Workspace selection belongs to trusted UI")),
    NotesList => ("notes/list", "List workspace notes", Access::Tool),
    NotesRead => ("notes/read", "Read current note content and its version", Access::Tool),
    NotesUpdate => ("notes/update", "Replace a UTF-8 range at the expected version", Access::Tool),
    NotesCreate => ("notes/create", "Create a note without changing selection", Access::Tool),
    NotesRename => ("notes/rename", "Rename at the expected version", Access::Tool),
    NotesMetadata => ("notes/metadata", "Set categories, pin, favorite or trash state", Access::Tool),
    NotesOpen => ("notes/open", "Open a plain document", Access::Tool),
    NotesSave => ("notes/save", "Save the open document; completion is tracked", Access::Tool),
    NotesRestore => ("notes/restore", "Restore unsaved recovery work", Access::Tool),
    NotesDiscard => ("notes/discard", "Discard local changes and reload disk", Access::Ui("Requires the existing destructive-action confirmation")),
    ExternalList => ("external/list", "List attached external files", Access::Tool),
    ExternalOpen => ("external/open", "Open a supported external file by absolute path", Access::Tool),
    ExternalClose => ("external/close", "Close an external file preserving unsaved work", Access::Tool),
    EditorUndo => ("editor/undo", "Undo in the open document at the expected revision", Access::Tool),
    EditorRedo => ("editor/redo", "Redo in the open document at the expected revision", Access::Tool),
    EditorInput => ("editor/input", "Apply native editor input and selection", Access::Ui("Clipboard, focus and selection belong to native UI")),
    Categories => ("catalog/categories", "Read categories and ordering version", Access::Tool),
    CatalogOrder => ("catalog/order", "Set manual ordering in a category", Access::Tool),
    CatalogOrderClear => ("catalog/order/clear", "Clear manual ordering", Access::Tool),
    CatalogSort => ("catalog/sort", "Save category sort preferences", Access::Tool),
    CategoriesOrder => ("catalog/categories/order", "Set category display order", Access::Tool),
    SearchQuery => ("search/query", "Search the local workspace index", Access::Tool),
    SearchRebuild => ("search/rebuild", "Rebuild the workspace search index", Access::Tool),
    SearchDocument => ("search/document", "Find matches in the current document buffer", Access::Tool),
    ChatsList => ("chats/list", "List saved AI chats in the current workspace", Access::Tool),
    ChatsRead => ("chats/read", "Read a bounded chat history page", Access::Tool),
    ChatsCreate => ("chats/create", "Create an empty chat without starting an assistant", Access::Tool),
    ChatsMetadata => ("chats/metadata", "Rename or organize a chat at its metadata version", Access::Tool),
    ChatsOpen => ("chats/open", "Open an AI chat", Access::Ui("Navigation belongs to trusted UI")),
    ChatsCompose => ("chats/compose", "Edit the composer buffer and schedule persistence", Access::Ui("Composer belongs to trusted UI")),
    ChatsDraft => ("chats/draft", "Save a composer draft", Access::Ui("Composer belongs to trusted UI")),
    ChatsSend => ("chats/send", "Send a chat message", Access::Ui("Assistant recursion is not allowed")),
    ChatsStop => ("chats/stop", "Stop a chat task", Access::Ui("Task lifecycle belongs to trusted UI")),
    ChatsSeen => ("chats/seen", "Acknowledge viewing the latest response", Access::Ui("Read markers belong to trusted UI")),
    ChatsAcknowledge => ("chats/acknowledge", "Acknowledge an unknown action outcome without repeating it", Access::Ui("An explicit user decision is required")),
    ChatsContinue => ("chats/continue", "Continue a paused chat task", Access::Ui("Task lifecycle belongs to trusted UI")),
    RssList => ("rss/list", "List RSS subscriptions", Access::Tool),
    RssRefresh => ("rss/refresh", "Refresh RSS and report its actual outcome", Access::Tool),
    RssCreate => ("rss/create", "Create an RSS subscription", Access::Tool),
    RssMetadata => ("rss/metadata", "Change RSS title, categories, pin, favorite or trash", Access::Tool),
    RssRead => ("rss/read", "Read entries and their state", Access::Tool),
    RssMarkRead => ("rss/mark/read", "Mark an RSS entry read", Access::Tool),
    RssFilters => ("rss/filters", "Change RSS filter preferences", Access::Tool),
    SettingsUi => ("settings/ui", "Persist native view preferences", Access::Ui("Window geometry and navigation belong to UI")),
    SettingsRead => ("settings/read", "Read ordinary settings and their version", Access::Tool),
    SettingsLocale => ("settings/locale", "Set the application language", Access::Tool),
    AiSettings => ("ai/settings", "Read model aliases without credentials", Access::Tool),
    AiRefresh => ("ai/refresh", "Refresh the model catalogue", Access::Tool),
    AiDisconnect => ("ai/disconnect", "Disconnect the AI provider", Access::Tool),
    AiCleanup => ("ai/cleanup", "Retry deferred credential removal", Access::Tool),
    AiAliasSave => ("ai/alias/save", "Save a model alias at the expected settings version", Access::Tool),
    AiAliasRemove => ("ai/alias/remove", "Remove a non-default model alias", Access::Tool),
    AiConnect => ("ai/connect", "Check and store an API key", Access::Ui("Secrets are accepted only by non-serializable trusted commands")),
    SecurityLock => ("security/lock", "Lock the selected protected note", Access::Ui("Protected document versions are not exposed to tools")),
    SecurityProtect => ("security/protect", "Protect a document", Access::Ui("Requires trusted password input and search purge")),
    SecurityUnlock => ("security/unlock", "Unlock a document", Access::Ui("Requires trusted password input")),
    SecurityDisable => ("security/disable", "Request removal of protection in UI", Access::Tool),
    SecurityPassword => ("security/password", "Change the master password", Access::Ui("Requires trusted password input")),
    SecurityIntegrity => ("security/integrity", "Resolve an integrity failure", Access::Ui("Requires the existing recovery confirmation")),
    SecurityRecovery => ("security/recovery", "Retry password-change recovery", Access::Ui("Recovery decisions belong to trusted UI")),
    UpdatesState => ("updates/state", "Read update progress", Access::Tool),
    UpdatesCheck => ("updates/check", "Check available releases", Access::Tool),
    UpdatesInstall => ("updates/install", "Install the available verified release", Access::Tool),
    UpdatesAutomatic => ("updates/automatic", "Set automatic update checks", Access::Tool),
    UpdatesDismiss => ("updates/dismiss", "Remember a declined release", Access::Ui("Controls the native update prompt")),
    UpdatesRestart => ("updates/restart", "Restart an installed update", Access::Ui("An explicit Restart click is mandatory")),
    JournalList => ("journal/list", "Read a bounded journal page", Access::Tool),
    JournalRead => ("journal/read", "Read a journal record", Access::Tool),
    JournalRetry => ("journal/retry", "Persist retained results without repeating HTTP", Access::Tool),
    JournalClear => ("journal/clear", "Clear completed request history", Access::Ui("Requires the existing confirmation dialog")),
    OperationStatus => ("operations/status", "Read operation progress and final result", Access::Tool),
    OperationProgress => ("operations/progress", "Read unified operation status and progress", Access::Tool),
    OperationCancel => ("operations/cancel", "Cancel before an irreversible write starts", Access::Tool),
}

impl super::runtime::Application {
    pub(crate) fn edit_catalog_note_title(
        &mut self,
        path: &std::path::Path,
        title: &str,
    ) -> Result<(), crate::i18n::UiText> {
        if !self.catalog_note_is_selected(path) {
            return Err(crate::i18n::msg!(SelectionNotOpen).into());
        }
        if self.edit_note_title(title) || self.title_edit_pending(title) {
            Ok(())
        } else {
            Err(self
                .error
                .clone()
                .unwrap_or_else(|| crate::i18n::msg!(ResolveSaveFirst).into()))
        }
    }

    fn catalog_change<T>(
        &mut self,
        change: impl FnOnce(
            &mut super::workspace::Workspace,
            &str,
        ) -> Result<T, stillus_core::CoreError>,
    ) -> Result<T, crate::i18n::UiText> {
        self.update_action_clock();
        let result = self
            .workspace
            .as_mut()
            .ok_or_else(|| {
                stillus_core::CoreError::NoteUnavailable("workspace is unavailable".into())
            })
            .and_then(|workspace| {
                let timestamp = stillus_core::format_utc_timestamp(workspace.wall_time)?;
                change(workspace, &timestamp)
            });
        // Batch category changes can stop after a version conflict; reconcile the
        // committed prefix too, while preserving the explicit failure for retry.
        self.sync_chat_catalog();
        self.request_search_reconcile();
        self.state_dirty = true;
        match result {
            Ok(value) => {
                self.error = None;
                Ok(value)
            }
            Err(error) => {
                let error = crate::i18n::UiText::Failure {
                    details: error.to_string(),
                };
                self.error = Some(error.clone());
                Err(error)
            }
        }
    }

    pub(crate) fn rename_catalog_category(&mut self, source: &str, target: &str) -> bool {
        self.catalog_change(|workspace, timestamp| {
            workspace.rename_category(source, target, timestamp)
        })
        .is_ok()
    }

    pub(crate) fn remove_catalog_category(&mut self, source: &str) -> bool {
        self.catalog_change(|workspace, timestamp| workspace.remove_category(source, timestamp))
            .is_ok()
    }

    pub(crate) fn prepare_catalog_note_deletion(
        &self,
        path: &std::path::Path,
    ) -> Result<stillus_core::PermanentNoteDeletion, crate::i18n::UiText> {
        self.workspace
            .as_ref()
            .ok_or_else(|| {
                stillus_core::CoreError::NoteUnavailable("workspace is unavailable".into())
            })
            .and_then(|workspace| workspace.prepare_note_deletion(path))
            .map_err(|error| crate::i18n::UiText::Failure {
                details: error.to_string(),
            })
    }

    pub(crate) fn delete_catalog_note_permanently(
        &mut self,
        request: stillus_core::PermanentNoteDeletion,
    ) -> bool {
        self.catalog_change(|workspace, _| workspace.delete_note_permanently(request))
            .is_ok()
    }

    fn catalog_note_is_selected(&self, path: &std::path::Path) -> bool {
        self.workspace.as_ref().is_some_and(|workspace| {
            workspace
                .selected_note()
                .and_then(|index| workspace.notes().get(index))
                .is_some_and(|note| note.path == path)
        })
    }

    pub(crate) fn set_catalog_note_pinned(&mut self, path: &std::path::Path, pinned: bool) -> bool {
        if self.catalog_note_is_selected(path) {
            if self.workspace.as_ref().is_some_and(|workspace| {
                workspace
                    .notes()
                    .iter()
                    .any(|note| note.path == path && note.pinned == pinned)
            }) {
                return true;
            }
            return self.toggle_pinned_selected();
        }
        self.catalog_change(|workspace, timestamp| {
            workspace.update_catalog_note_metadata(
                path,
                stillus_core::NoteMetadataEdit {
                    pinned: Some(pinned),
                    ..Default::default()
                },
                timestamp,
            )
        })
        .is_ok()
    }

    pub(crate) fn set_catalog_note_favorited(
        &mut self,
        path: &std::path::Path,
        favorited: bool,
    ) -> bool {
        if self.catalog_note_is_selected(path) {
            if self.workspace.as_ref().is_some_and(|workspace| {
                workspace
                    .notes()
                    .iter()
                    .any(|note| note.path == path && note.favorited == favorited)
            }) {
                return true;
            }
            return self.toggle_favorited_selected();
        }
        self.catalog_change(|workspace, timestamp| {
            workspace.update_catalog_note_metadata(
                path,
                stillus_core::NoteMetadataEdit {
                    favorited: Some(favorited),
                    ..Default::default()
                },
                timestamp,
            )
        })
        .is_ok()
    }

    pub(crate) fn set_catalog_note_deleted(
        &mut self,
        path: &std::path::Path,
        deleted: bool,
    ) -> bool {
        if self.catalog_note_is_selected(path) {
            return self.set_deleted_selected(deleted);
        }
        self.catalog_change(|workspace, timestamp| {
            workspace.update_catalog_note_metadata(
                path,
                stillus_core::NoteMetadataEdit {
                    deleted: Some(deleted),
                    ..Default::default()
                },
                timestamp,
            )
        })
        .is_ok()
    }
}

#[cfg(test)]
mod sidebar_action_tests {
    use super::super::runtime::Application;
    use std::{fs, path::PathBuf};
    use stillus_core::EditorCommand;

    struct Fixture {
        app: Application,
        root: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "stillus-sidebar-actions-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir_all(root.join("notes")).unwrap();
            fs::write(root.join("notes/A.md"), "# A\n\nfirst body\n").unwrap();
            fs::write(root.join("notes/B.md"), "# B\n\nsecond body\n").unwrap();
            let mut app = Application::load(&root);
            app.open_note(0);
            Self { app, root }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.app.shutdown().unwrap();
            fs::remove_dir_all(&self.root).unwrap();
        }
    }

    #[test]
    fn sidebar_metadata_keeps_another_dirty_note_selected_and_is_not_a_toggle() {
        let mut fixture = Fixture::new();
        let workspace = fixture.app.workspace.as_ref().unwrap();
        let selected = workspace.selected_note().unwrap();
        let selected_path = workspace.notes()[selected].path.clone();
        let target = workspace.notes()[1 - selected].path.clone();
        let end = workspace.document().unwrap().len_bytes();
        fixture.app.apply(EditorCommand::SetCaret {
            offset: end,
            extend: false,
        });
        fixture
            .app
            .apply(EditorCommand::Insert("dirty text".into()));
        let revision = fixture
            .app
            .workspace
            .as_ref()
            .unwrap()
            .document()
            .unwrap()
            .content_revision();
        assert!(fixture.app.set_catalog_note_pinned(&target, true));
        assert!(fixture.app.set_catalog_note_pinned(&target, true));
        assert!(fixture.app.set_catalog_note_favorited(&target, true));
        assert!(fixture.app.set_catalog_note_deleted(&target, true));
        let workspace = fixture.app.workspace.as_ref().unwrap();
        assert_eq!(
            workspace.notes()[workspace.selected_note().unwrap()].path,
            selected_path
        );
        assert_eq!(workspace.document().unwrap().content_revision(), revision);
        assert!(workspace.document().unwrap().has_unsaved_work());
        let note = workspace
            .notes()
            .iter()
            .find(|note| note.path == target)
            .unwrap();
        assert!(note.pinned && note.favorited && note.deleted);
        assert!(fs::read_to_string(target).unwrap().contains("second body"));
        assert!(
            !fs::read_to_string(selected_path)
                .unwrap()
                .contains("dirty text")
        );
    }

    #[test]
    fn stale_sidebar_path_does_not_mutate_the_current_note() {
        let mut fixture = Fixture::new();
        let missing = fixture.root.join("notes/Missing.md");
        assert!(!fixture.app.set_catalog_note_pinned(&missing, true));
        assert!(fixture.app.error.is_some());
        let workspace = fixture.app.workspace.as_ref().unwrap();
        assert_eq!(workspace.selected_note(), Some(0));
        assert!(workspace.notes().iter().all(|note| !note.pinned));
    }
}
