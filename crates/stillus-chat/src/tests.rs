// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only
#![forbid(unsafe_code)]
use super::*;
fn setup() -> (tempfile::TempDir, ChatStore, ItemId) {
    let dir = tempfile::tempdir().unwrap();
    let store = ChatStore::open(dir.path()).unwrap();
    assert!(!store.root().exists());
    let id = store
        .create_chat(Metadata {
            common: CommonMetadata {
                title: "New chat".into(),
                ..Default::default()
            },
            alias: "default".into(),
            automatic_title: true,
        })
        .unwrap();
    (dir, store, id)
}
fn message(run: &str) -> Message {
    Message {
        id: new_id(),
        run: run.into(),
        role: Role::Assistant,
        text: "partial".into(),
        delivery: Delivery::Partial,
        created_ms: 1,
        tool: None,
        provider_state: None,
        request_id: None,
    }
}
#[test]
fn independent_versions_draft_partial_metadata_and_trash() {
    let (_dir, store, id) = setup();
    let initial = store.metadata(&id).unwrap();
    let draft = store.draft(&id).unwrap();
    let draft = store
        .save_draft(
            &id,
            &draft.revision,
            &Draft {
                text: "hello\nworld".into(),
            },
        )
        .unwrap();
    let mut msg = message("run");
    let first = store.save_message(&id, None, &msg).unwrap();
    let changed = store
        .patch_metadata(
            &id,
            &initial.revision,
            CommonMetadataPatch {
                title: Some("Renamed".into()),
                deleted: Some(true),
                ..Default::default()
            },
            None,
            true,
        )
        .unwrap();
    assert!(!changed.value.automatic_title);
    msg.text.push_str(" complete");
    msg.delivery = Delivery::Complete;
    store
        .save_message(&id, Some(&first.revision), &msg)
        .unwrap();
    assert_eq!(store.draft(&id).unwrap().revision, draft.revision);
    assert_eq!(
        store
            .save_message(&id, Some(&first.revision), &msg)
            .unwrap_err(),
        ChatError::Conflict
    );
    assert_eq!(
        store
            .patch_metadata(
                &id,
                &initial.revision,
                CommonMetadataPatch::default(),
                None,
                true
            )
            .unwrap_err(),
        ChatError::Conflict
    );
    store
        .patch_metadata(
            &id,
            &changed.revision,
            CommonMetadataPatch {
                deleted: Some(false),
                ..Default::default()
            },
            None,
            true,
        )
        .unwrap();
    assert_eq!(
        store.history(&id, None, 10).unwrap().entries[0]
            .message
            .as_ref()
            .unwrap()
            .value
            .text,
        "partial complete"
    );
}
#[test]
fn pages_corrupt_and_unsupported_records_are_isolated_and_preserved() {
    let (_dir, store, id) = setup();
    let mut ids = Vec::new();
    for _ in 0..71 {
        let m = message("run");
        ids.push(m.id.clone());
        store.save_message(&id, None, &m).unwrap();
    }
    let first = store.history(&id, None, 64).unwrap();
    assert_eq!(first.entries.len(), 64);
    let last = store.history(&id, first.next.as_deref(), 64).unwrap();
    assert_eq!(last.entries.len(), 7);
    assert!(last.next.is_none());
    let path = store
        .directory(&id)
        .unwrap()
        .join("messages")
        .join(format!("{}.json", ids[70]));
    fs::write(&path, b"broken").unwrap();
    assert!(
        store
            .history(&id, None, 64)
            .unwrap()
            .entries
            .last()
            .unwrap()
            .diagnostic
            .is_some()
    );
    let metadata = store.directory(&id).unwrap().join("metadata.json");
    let mut raw: Value = serde_json::from_slice(&fs::read(&metadata).unwrap()).unwrap();
    raw["version"] = 99.into();
    fs::write(&metadata, serde_json::to_vec(&raw).unwrap()).unwrap();
    let before = fs::read(&metadata).unwrap();
    assert_eq!(store.metadata(&id).unwrap_err(), ChatError::Unsupported(99));
    assert_eq!(store.list().unwrap().len(), 1);
    assert_eq!(fs::read(metadata).unwrap(), before);
}
#[test]
fn crash_projection_never_replays_prepared_actions_or_rewrites() {
    let (_dir, store, id) = setup();
    let run = Run {
        id: new_id(),
        status: RunStatus::Running,
        provider: "future/provider".into(),
        model: "future".into(),
        parameters: Value::Null,
        requests: 1,
        tools: 1,
        request_limit: 20,
        tool_limit: 50,
        error: None,
        unread: false,
        pending_calls: Vec::new(),
        pending_request: None,
        awaiting_model: true,
    };
    let saved = store.save_run(&id, None, &run).unwrap();
    let mut msg = message(&run.id);
    msg.role = Role::Tool;
    msg.tool = Some(ToolCall {
        id: "call/1".into(),
        name: "notes/create".into(),
        arguments: serde_json::json!({}),
        state: ToolState::Prepared,
        result: None,
    });
    store.save_message(&id, None, &msg).unwrap();
    let loaded = store.run(&id).unwrap().unwrap();
    assert_eq!(loaded.revision, saved.revision);
    assert_eq!(loaded.value.status, RunStatus::Interrupted);
    let page = store.history(&id, None, 10).unwrap();
    assert_eq!(
        page.entries[0]
            .message
            .as_ref()
            .unwrap()
            .value
            .tool
            .as_ref()
            .unwrap()
            .state,
        ToolState::Unknown
    );
    let raw: Versioned<Run> = read_record(&store.directory(&id).unwrap().join("run.json")).unwrap();
    assert_eq!(raw.value.status, RunStatus::Running);
}
#[test]
fn concurrent_writers_have_one_winner_and_no_transcript_lock() {
    let (_dir, store, id) = setup();
    let metadata = store.metadata(&id).unwrap();
    let mut workers = Vec::new();
    for _ in 0..4 {
        let store = store.clone();
        let id = id.clone();
        let revision = metadata.revision.clone();
        workers.push(std::thread::spawn(move || {
            store.patch_metadata(
                &id,
                &revision,
                CommonMetadataPatch {
                    title: Some(new_id()),
                    ..Default::default()
                },
                None,
                true,
            )
        }));
    }
    let results = workers
        .into_iter()
        .map(|w| w.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, Err(ChatError::Conflict)))
            .count(),
        3
    );
}
#[test]
fn record_limit_and_external_edits_cannot_be_overwritten() {
    let (_dir, store, id) = setup();
    let draft = store.draft(&id).unwrap();
    assert_eq!(
        store
            .save_draft(
                &id,
                &draft.revision,
                &Draft {
                    text: "x".repeat(MAX_RECORD_BYTES)
                }
            )
            .unwrap_err(),
        ChatError::TooLarge
    );
    let path = store.directory(&id).unwrap().join("draft.json");
    let mut raw: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    raw["data"]["text"] = "external".into();
    fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();
    assert_eq!(
        store
            .save_draft(&id, &draft.revision, &Draft::default())
            .unwrap_err(),
        ChatError::Conflict
    );
}
#[cfg(unix)]
#[test]
fn links_are_rejected() {
    use std::os::unix::fs::symlink;
    let (dir, store, id) = setup();
    let outside = dir.path().join("outside");
    fs::write(&outside, b"untouched").unwrap();
    let path = store.directory(&id).unwrap().join("draft.json");
    fs::remove_file(&path).unwrap();
    symlink(&outside, &path).unwrap();
    assert!(store.draft(&id).is_err());
    assert_eq!(fs::read(outside).unwrap(), b"untouched");
}

#[test]
fn history_pages_bound_bytes_and_cleared_order_does_not_return() {
    let (_dir, store, id) = setup();
    let initial = store.metadata(&id).unwrap();
    let ranked = store
        .patch_metadata(
            &id,
            &initial.revision,
            CommonMetadataPatch {
                order: Some(BTreeMap::from([("Work".into(), 3)])),
                ..Default::default()
            },
            None,
            true,
        )
        .unwrap();
    let cleared = store
        .patch_metadata(
            &id,
            &ranked.revision,
            CommonMetadataPatch {
                order: Some(BTreeMap::new()),
                ..Default::default()
            },
            None,
            true,
        )
        .unwrap();
    assert!(cleared.value.common.order.is_empty());
    assert!(store.metadata(&id).unwrap().value.common.order.is_empty());
    for _ in 0..4 {
        let mut record = message("large");
        record.text = "a".repeat(750000);
        store.save_message(&id, None, &record).unwrap();
    }
    let page = store.history(&id, None, 64).unwrap();
    assert_eq!(page.entries.len(), 2);
    let older = store.history(&id, page.next.as_deref(), 64).unwrap();
    assert_eq!(older.entries.len(), 2);
    assert!(older.next.is_none());
}
