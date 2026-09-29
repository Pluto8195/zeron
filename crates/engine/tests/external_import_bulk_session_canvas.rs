//! Bulk migration: import every session_canvas-tracked session still on
//! disk, carrying over its archive state. A single test function — `scan()`
//! and the session_canvas archive reader both resolve `~/.claude/projects`
//! and `~/.config/session-canvas` off `$HOME`, so this test redirects `HOME`
//! to a synthetic tempdir for its duration. `std::env::set_var` is
//! process-global; keeping everything in one `#[tokio::test]` (rather than
//! several in this file) avoids any race with a sibling test setting it
//! differently while this one runs.

use std::sync::Arc;

use zeron_engine::external_import::BulkImportEvent;
use zeron_engine::{EngineCore, EngineProfile, HarnessId, default_registry};

fn assemble(profile: EngineProfile) -> EngineCore {
    EngineCore::assemble_with_profile(profile, Arc::new(default_registry()), HarnessId::Mock, None)
        .expect("assemble profile")
}

fn synthetic_transcript(session_id: &str, cwd: &str, first_message: &str) -> String {
    format!(
        "{}\n{}\n",
        serde_json::json!({
            "parentUuid": null, "isSidechain": false, "type": "user",
            "message": {"role": "user", "content": first_message},
            "uuid": format!("{session_id}-u1"),
            "timestamp": "2026-01-01T00:00:01.000Z",
            "cwd": cwd, "sessionId": session_id,
        }),
        serde_json::json!({
            "parentUuid": format!("{session_id}-u1"), "isSidechain": false, "type": "assistant",
            "message": {"model": "claude-x", "content": [{"type": "text", "text": "On it."}]},
            "uuid": format!("{session_id}-a1"),
            "timestamp": "2026-01-01T00:00:02.000Z",
            "cwd": cwd, "sessionId": session_id,
        }),
    )
}

#[tokio::test]
async fn bulk_imports_every_session_and_carries_over_session_canvas_archive_state() {
    // A fake $HOME holding the on-disk transcripts and session_canvas's
    // archive store — entirely separate from Zeron's own data dir below.
    let fake_home = tempfile::tempdir().expect("fake home tempdir");
    // SAFETY (test-only, single-threaded-relative-to-this-var use): scoped to
    // this one test function, restored at the end; no other test in this
    // file touches HOME concurrently.
    let previous_home = std::env::var_os("HOME");
    unsafe {
        std::env::set_var("HOME", fake_home.path());
    }

    let projects_dir = fake_home.path().join(".claude").join("projects").join("proj-1");
    std::fs::create_dir_all(&projects_dir).expect("projects dir");

    let sessions = [
        ("sess-active-1", "/work/one", "do the first thing"),
        ("sess-active-2", "/work/two", "do the second thing"),
        ("sess-archived-1", "/work/three", "an old dead session"),
    ];
    for (id, cwd, msg) in sessions {
        std::fs::write(
            projects_dir.join(format!("{id}.jsonl")),
            synthetic_transcript(id, cwd, msg),
        )
        .expect("write transcript");
    }

    let archive_dir = fake_home.path().join(".config").join("session-canvas");
    std::fs::create_dir_all(&archive_dir).expect("archive dir");
    std::fs::write(
        archive_dir.join("archive.json"),
        serde_json::to_vec(&["sess-archived-1", "sess-never-imported-elsewhere"]).unwrap(),
    )
    .expect("write archive store");

    // Zeron's own data dir — unrelated to $HOME, must not collide with it.
    let data_dir = tempfile::tempdir().expect("data dir");
    let core = assemble(EngineProfile::development(
        data_dir.path(),
        "dev-org",
        "dev-user",
    ));

    let mut events = Vec::new();
    let summary = core
        .external_import
        .bulk_import_from_session_canvas(|event| events.push(event))
        .expect("bulk import");

    let BulkImportEvent::Summary {
        total,
        imported,
        archived,
        failed,
        errors,
    } = summary
    else {
        panic!("expected a Summary event, got {summary:?}");
    };
    assert_eq!(total, 3);
    assert_eq!(imported, 3);
    assert_eq!(archived, 1, "only sess-archived-1 should carry over");
    assert_eq!(failed, 0);
    assert!(errors.is_empty(), "unexpected errors: {errors:?}");

    // A Start, one Item per candidate, and the Summary itself.
    assert!(matches!(events.first(), Some(BulkImportEvent::Start { total: 3 })));
    let item_count = events
        .iter()
        .filter(|e| matches!(e, BulkImportEvent::Item { .. }))
        .count();
    assert_eq!(item_count, 3);

    // Verify the actual chat rows: exactly the archived-in-session_canvas
    // session landed archived, the other two did not.
    let all_chats = core.workspace.read_chats().expect("read chats");
    assert_eq!(all_chats.len(), 3);
    for chat in &all_chats {
        let session_id = chat.harness_session_id.as_deref().unwrap_or_default();
        let should_be_archived = session_id == "sess-archived-1";
        assert_eq!(
            chat.archived, should_be_archived,
            "chat for {session_id} archived={}, expected {should_be_archived}",
            chat.archived
        );
    }

    // Re-running must be a safe no-op: scan() already excludes every
    // harness_session_id claimed above, so nothing is left to import.
    let mut second_events = Vec::new();
    let second_summary = core
        .external_import
        .bulk_import_from_session_canvas(|event| second_events.push(event))
        .expect("second bulk import");
    let BulkImportEvent::Summary {
        total: second_total,
        imported: second_imported,
        ..
    } = second_summary
    else {
        panic!("expected a Summary event on the second run");
    };
    assert_eq!(second_total, 0, "re-running should find nothing left to import");
    assert_eq!(second_imported, 0);

    core.shutdown().await;

    // SAFETY: restoring whatever HOME was before this test touched it.
    unsafe {
        match previous_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }
}
