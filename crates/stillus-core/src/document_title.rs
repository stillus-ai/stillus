// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]

use super::*;

impl WorkspaceSession {
    /// Rename from the editor header by editing the authoritative body title.
    /// Persistence, path collision handling and encryption use normal autosave.
    pub fn edit_selected_title(
        &mut self,
        title: &str,
        now_ms: u64,
    ) -> Result<CommandOutcome, CoreError> {
        self.ensure_no_secure_operation()?;
        let title = validate_note_title(title)?;
        let document = self.document.as_ref().ok_or_else(|| {
            CoreError::NoteUnavailable("title editing requires an unlocked note".into())
        })?;
        if document.is_external() {
            return Err(CoreError::NoteUnavailable(
                "external file titles are owned by their filenames".into(),
            ));
        }
        let byte_limit = document
            .editor
            .len_bytes()
            .min(stillus_storage::BODY_TITLE_SCAN_BYTES);
        let mut end = ByteOffset::new(byte_limit);
        while end.get() > 0 && !document.editor.is_codepoint_boundary(end)? {
            end = ByteOffset::new(end.get() - 1);
        }
        let body = document
            .editor
            .slice(ByteRange::new(ByteOffset::new(0), end)?)?;
        let (start, end, text) = title_replacement(&body, document.editor.len_bytes(), &title);
        self.apply_selected_at(EditorCommand::ReplaceRange { start, end, text }, now_ms)
    }
}

fn title_replacement(body: &str, total_bytes: usize, title: &str) -> (usize, usize, String) {
    // Preserve the exact visible title, including literal Markdown punctuation.
    let escaped = title
        .chars()
        .flat_map(|c| {
            c.is_ascii_punctuation()
                .then_some('\\')
                .into_iter()
                .chain(std::iter::once(c))
        })
        .collect::<String>();
    let mut start = 0;
    for line in body
        .split_inclusive('\n')
        .take(stillus_storage::BODY_TITLE_SCAN_LINES)
    {
        let content = line
            .strip_suffix('\n')
            .unwrap_or(line)
            .strip_suffix('\r')
            .unwrap_or_else(|| line.strip_suffix('\n').unwrap_or(line));
        if !content.trim().is_empty() {
            // A partial oversized first line must never be truncated by renaming.
            if !line.ends_with('\n') && body.len() < total_bytes {
                break;
            }
            let trimmed = content.trim_start_matches(' ');
            let indentation = content.len() - trimmed.len();
            let hashes = trimmed.bytes().take_while(|byte| *byte == b'#').count();
            let prefix = if indentation <= 3
                && (1..=6).contains(&hashes)
                && trimmed
                    .as_bytes()
                    .get(hashes)
                    .is_some_and(u8::is_ascii_whitespace)
            {
                format!("{} ", "#".repeat(hashes))
            } else {
                String::new()
            };
            return (start, start + content.len(), format!("{prefix}{escaped}"));
        }
        start += line.len();
    }
    // Empty or unprojectable bodies get a new heading; all old bytes survive.
    (0, 0, format!("# {escaped}\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use stillus_platform::fs;

    struct TestWorkspace(PathBuf);
    impl TestWorkspace {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "stillus-title-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir_all(root.join("notes")).unwrap();
            Self(root)
        }
        fn path(&self) -> &Path {
            &self.0
        }
        fn note_path(&self, name: &str) -> PathBuf {
            self.0.join("notes").join(name)
        }
        fn write_note(&self, name: &str, body: &str) {
            fs::write(self.note_path(name), body).unwrap();
        }
    }
    impl Drop for TestWorkspace {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn header_title_edit_preserves_body_selection_undo_and_atomic_save() {
        let workspace = TestWorkspace::new();
        workspace.write_note(
            "old.md",
            "---\nfuture: keep\n---\n\n## Старое\r\nbody remains\n",
        );
        let mut session = WorkspaceSession::open(workspace.path()).unwrap();
        session.open_note(0).unwrap();
        let original = session
            .document()
            .unwrap()
            .editor
            .slice(
                ByteRange::new(
                    ByteOffset::new(0),
                    ByteOffset::new(session.document().unwrap().editor.len_bytes()),
                )
                .unwrap(),
            )
            .unwrap();
        let caret = original.find("remains").unwrap();
        session
            .apply_selected_at(
                EditorCommand::SetCaret {
                    offset: caret,
                    extend: false,
                },
                1,
            )
            .unwrap();
        session.edit_selected_title("Новое [имя]", 2).unwrap();
        assert_eq!(session.document().unwrap().title(), "Новое [имя]");
        let changed = session
            .document()
            .unwrap()
            .editor
            .slice(
                ByteRange::new(
                    ByteOffset::new(0),
                    ByteOffset::new(session.document().unwrap().editor.len_bytes()),
                )
                .unwrap(),
            )
            .unwrap();
        assert!(changed.ends_with("\r\nbody remains\n"));
        assert_eq!(
            session.document().unwrap().selection().focus().get(),
            changed.find("remains").unwrap()
        );
        session.apply_selected_at(EditorCommand::Undo, 3).unwrap();
        assert_eq!(session.document().unwrap().title(), "Старое");
        assert_eq!(session.document().unwrap().selection().focus().get(), caret);
        session.apply_selected_at(EditorCommand::Redo, 4).unwrap();
        let save = session
            .begin_autosave(4 + AUTOSAVE_DEBOUNCE_MS, "now".into())
            .unwrap()
            .unwrap();
        session.finish_autosave(save.execute()).unwrap();
        assert!(!workspace.note_path("old.md").exists());
        let bytes = fs::read_to_string(&session.notes()[0].path).unwrap();
        assert!(bytes.contains("future: keep"));
        assert!(bytes.ends_with(&changed));
        assert_eq!(session.notes()[0].title, "Новое [имя]");
    }

    #[test]
    fn header_title_edit_requires_unlock_and_saves_the_body_only_as_ciphertext() {
        let workspace = TestWorkspace::new();
        workspace.write_note(
            "Secret.md",
            "---\nfuture: keep\n---\n# Secret\nbody-secret-marker\n",
        );
        let path = workspace.note_path("Secret.md");
        let password = MasterPassword::new("header title password".into());
        let (_, version) = open_versioned(&path).unwrap();
        protect_note_body(&path, &version, &password, "Secret").unwrap();
        let mut session = WorkspaceSession::open(workspace.path()).unwrap();
        session.select_protected_note(0).unwrap();
        assert!(session.edit_selected_title("Renamed", 1).is_err());
        session.unlock_note(0, password.clone()).unwrap();
        session.edit_selected_title("Renamed", 2).unwrap();
        let save = session
            .begin_persistence(2 + AUTOSAVE_DEBOUNCE_MS, "now".into())
            .unwrap()
            .unwrap();
        session.finish_persistence(save.execute()).unwrap();
        let path = session.notes()[session.selected_note().unwrap()]
            .path
            .clone();
        let ciphertext = fs::read_to_string(&path).unwrap();
        assert!(ciphertext.contains("future: keep"));
        assert!(!ciphertext.contains("body-secret-marker"));
        assert!(!ciphertext.contains("# Renamed"));
        session.lock_selected().unwrap();
        session.unlock_note(0, password).unwrap();
        assert_eq!(session.document().unwrap().title(), "Renamed");
        assert_eq!(
            session
                .document()
                .unwrap()
                .viewport(ViewportRequest::default())
                .unwrap()
                .lines[1]
                .text,
            "body-secret-marker"
        );
    }

    #[test]
    fn header_title_edit_never_truncates_a_long_or_empty_body() {
        for body in [
            String::new(),
            "\n".repeat(80),
            "x".repeat(stillus_storage::BODY_TITLE_SCAN_BYTES + 100),
        ] {
            let bounded = &body[..body.len().min(stillus_storage::BODY_TITLE_SCAN_BYTES)];
            let (start, end, text) = title_replacement(bounded, body.len(), "A [literal] title");
            assert_eq!((start, end), (0, 0));
            assert_eq!(
                project_body_title(&format!("{text}{body}")).as_deref(),
                Some("A [literal] title")
            );
        }
        assert_eq!(
            title_replacement("### First\nRest", 14, "Next"),
            (0, 9, "### Next".into())
        );
    }
}
