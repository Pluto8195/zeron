//! Classifier/title-gen throwaway transcripts must never be adopted as real
//! chats: a `claude --print` classifier ("Classify this coding-agent chat...")
//! or title-generation ("Reply with ONLY a concise...") call writes its own
//! one-shot transcript under `~/.claude/projects` as a side effect, the same
//! as any real session. Ported filter from `agent-mode-tools/
//! session_canvas_server.py`'s `TITLEGEN_PROMPT_PREFIX`/`CLASSIFY_PROMPT_PREFIX`
//! (used in `scan_external_claude_sessions`'s "no real first_user_message
//! survived" check). Covers all three places this needs to hold:
//!   a) `ExternalSessionImporter::scan` (the picker's import-candidate list)
//!   b) `bulk_import_from_session_canvas` (which calls `scan()` internally)
//!   c) `repair_junk_imports` (boot-time cleanup of chats a PRE-fix bulk
//!      import already adopted)
//!
//! Same `$HOME`-redirection convention as `external_import_bulk_session_canvas.rs`
//! for (a)/(b) — kept as one test function per file for the same reason that
//! file documents (`std::env::set_var` is process-global).

use std::sync::Arc;

use zeron_engine::external_import::BulkImportEvent;
use zeron_engine::{EngineCore, EngineProfile, HarnessId, default_registry};

fn assemble(profile: EngineProfile) -> EngineCore {
    EngineCore::assemble_with_profile(profile, Arc::new(default_registry()), HarnessId::Mock, None)
        .expect("assemble profile")
}

fn transcript_with_first_message(session_id: &str, cwd: &str, first_message: &str) -> String {
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
            "message": {"model": "claude-x", "content": [{"type": "text", "text": "ack"}]},
            "uuid": format!("{session_id}-a1"),
            "timestamp": "2026-01-01T00:00:02.000Z",
            "cwd": cwd, "sessionId": session_id,
        }),
    )
}

const TITLEGEN_MESSAGE: &str =
    "Reply with ONLY a concise title (under 8 words) for this coding-agent chat, no punctuation.";
const CLASSIFY_MESSAGE: &str =
    "Classify this coding-agent chat into exactly one task category from the list below.";

#[tokio::test]
async fn scan_and_bulk_import_skip_classifier_and_titlegen_throwaways_but_keep_real_chats() {
    let fake_home = tempfile::tempdir().expect("fake home tempdir");
    let previous_home = std::env::var_os("HOME");
    // SAFETY: test-only, scoped to this one function, restored at the end —
    // same justification as `external_import_bulk_session_canvas.rs`.
    unsafe {
        std::env::set_var("HOME", fake_home.path());
    }

    let projects_dir = fake_home.path().join(".claude").join("projects").join("proj-1");
    std::fs::create_dir_all(&projects_dir).expect("projects dir");

    let sessions = [
        ("sess-real", "/work/real", "please add a health check endpoint"),
        ("sess-titlegen", "/work/titlegen", TITLEGEN_MESSAGE),
        ("sess-classify", "/work/classify", CLASSIFY_MESSAGE),
    ];
    for (id, cwd, msg) in sessions {
        std::fs::write(projects_dir.join(format!("{id}.jsonl")), transcript_with_first_message(id, cwd, msg))
            .expect("write transcript");
    }

    let data_dir = tempfile::tempdir().expect("data dir");
    let core = assemble(EngineProfile::development(data_dir.path(), "dev-org", "dev-user"));

    // (a) scan() itself must exclude both throwaways, keeping only the real one.
    let candidates = core.external_import.scan().expect("scan");
    assert_eq!(candidates.len(), 1, "expected only the real session to survive scan(): {candidates:#?}");
    assert_eq!(candidates[0].session_id, "sess-real");

    // (b) bulk_import_from_session_canvas calls scan() internally, so it must
    // import only the real one too.
    let mut events = Vec::new();
    let summary = core
        .external_import
        .bulk_import_from_session_canvas(|event| events.push(event))
        .expect("bulk import");
    let BulkImportEvent::Summary { total, imported, failed, .. } = summary else {
        panic!("expected a Summary event");
    };
    assert_eq!(total, 1);
    assert_eq!(imported, 1);
    assert_eq!(failed, 0);

    let all_chats = core.workspace.read_chats().expect("read chats");
    assert_eq!(all_chats.len(), 1, "only the real session should have been imported");
    assert_eq!(all_chats[0].harness_session_id.as_deref(), Some("sess-real"));

    core.shutdown().await;

    // SAFETY: restoring whatever HOME was before this test touched it.
    unsafe {
        match previous_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }
}

#[tokio::test]
async fn repair_junk_imports_hard_deletes_already_imported_throwaways_but_leaves_real_chats_idempotently() {
    let dir = tempfile::tempdir().expect("tempdir");
    let core = assemble(EngineProfile::development(dir.path(), "dev-org", "dev-user"));

    // Simulate chats a PRE-fix bulk import already adopted: `import()` itself
    // (unlike `scan()`) never filtered synthetic transcripts, so this is
    // exactly what's sitting in an affected user's workspace today.
    let real_path = dir.path().join("real.jsonl");
    std::fs::write(&real_path, transcript_with_first_message("sess-real", "/work/real", "add a health check endpoint"))
        .expect("write real transcript");
    let titlegen_path = dir.path().join("titlegen.jsonl");
    std::fs::write(
        &titlegen_path,
        transcript_with_first_message("sess-titlegen", "/work/titlegen", TITLEGEN_MESSAGE),
    )
    .expect("write titlegen transcript");
    let classify_path = dir.path().join("classify.jsonl");
    std::fs::write(
        &classify_path,
        transcript_with_first_message("sess-classify", "/work/classify", CLASSIFY_MESSAGE),
    )
    .expect("write classify transcript");

    core.external_import
        .import("chat-real", "sess-real", &real_path)
        .expect("import real");
    core.external_import
        .import("chat-titlegen", "sess-titlegen", &titlegen_path)
        .expect("import titlegen throwaway (pre-fix behavior)");
    core.external_import
        .import("chat-classify", "sess-classify", &classify_path)
        .expect("import classify throwaway (pre-fix behavior)");

    assert_eq!(core.workspace.read_chats().expect("read chats").len(), 3);

    let removed = core.external_import.repair_junk_imports().expect("repair_junk_imports");
    assert_eq!(removed, 2, "both throwaway chats should be removed");

    let remaining = core.workspace.read_chats().expect("read chats");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id, "chat-real");
    assert!(core.workspace.chat("chat-titlegen").expect("read chat").is_none());
    assert!(core.workspace.chat("chat-classify").expect("read chat").is_none());

    // Idempotent: a second pass finds nothing left to remove.
    let removed_again = core.external_import.repair_junk_imports().expect("second repair_junk_imports");
    assert_eq!(removed_again, 0);
    assert_eq!(core.workspace.read_chats().expect("read chats").len(), 1);

    core.shutdown().await;
}
