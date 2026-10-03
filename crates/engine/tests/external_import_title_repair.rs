//! `ExternalSessionImporter::repair_missing_titles`: chats imported before
//! the priority-chain title resolution existed (see
//! `external_import_titles.rs`) got `import()`'s pre-fix behavior — a title
//! only when the transcript happened to carry an `ai-title` record, `None`
//! otherwise (rendered "New session" by every UI surface, never a literal
//! stored placeholder — see the method's own doc comment in
//! `crates/engine/src/external_import.rs`). This repair backfills those,
//! without requiring a re-import, the same way its four siblings
//! (`repair_missing_timestamps`/`_classification`/`_config`, `repair_junk_imports`)
//! do.

use std::sync::Arc;

use zeron_engine::{EngineCore, EngineProfile, HarnessId, default_registry};

fn assemble(profile: EngineProfile) -> EngineCore {
    EngineCore::assemble_with_profile(profile, Arc::new(default_registry()), HarnessId::Mock, None)
        .expect("assemble profile")
}

fn transcript_with_lines(lines: Vec<serde_json::Value>) -> String {
    lines
        .into_iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn user_line(session_id: &str, uuid: &str, cwd: &str, text: &str) -> serde_json::Value {
    serde_json::json!({
        "parentUuid": null, "isSidechain": false, "type": "user",
        "message": {"role": "user", "content": text},
        "uuid": uuid, "timestamp": "2026-01-01T00:00:01.000Z",
        "cwd": cwd, "sessionId": session_id,
    })
}

fn assistant_ack_line(session_id: &str, uuid: &str, parent: &str, cwd: &str) -> serde_json::Value {
    serde_json::json!({
        "parentUuid": parent, "isSidechain": false, "type": "assistant",
        "message": {"model": "claude-x", "content": [{"type": "text", "text": "ack"}]},
        "uuid": uuid, "timestamp": "2026-01-01T00:00:02.000Z",
        "cwd": cwd, "sessionId": session_id,
    })
}

/// Simulates a chat imported by the OLD pre-priority-chain code path: strip
/// its resolved title back to unset by writing a raw registry doc mutation
/// isn't available publicly, so instead this constructs the transcript with
/// NO ai-title record and no registry entry, imports it (which today already
/// resolves the first-message tier)... except we want to test the REPAIR
/// path specifically, so instead each scenario below imports a transcript
/// whose `import()` call happens BEFORE the repair-relevant state is added
/// (an ai-title appended afterward, or a registry file created afterward),
/// proving the repair — not `import()` itself — is what resolves it.
fn write_transcript(
    dir: &std::path::Path,
    name: &str,
    lines: Vec<serde_json::Value>,
) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, transcript_with_lines(lines)).expect("write transcript");
    path
}

#[tokio::test]
async fn repair_resolves_titles_registry_beats_ai_title_beats_first_message_never_overwrites_idempotent()
 {
    let fake_home = tempfile::tempdir().expect("fake home tempdir");
    let previous_home = std::env::var_os("HOME");
    // SAFETY: test-only, scoped to this one function, restored at the end —
    // same justification as `external_import_bulk_session_canvas.rs`.
    unsafe {
        std::env::set_var("HOME", fake_home.path());
    }
    // No agent-mode registry directory at all yet — created partway through,
    // below, to prove the repair reads it fresh each pass rather than
    // caching a snapshot from boot.
    let registry_dir = fake_home
        .path()
        .join(".config")
        .join("agent-mode")
        .join("session-ids");

    let data_dir = tempfile::tempdir().expect("data dir");
    let core = assemble(EngineProfile::development(
        data_dir.path(),
        "dev-org",
        "dev-user",
    ));
    let transcripts_dir = tempfile::tempdir().expect("transcripts dir");

    // Chat A: no ai-title, no registry entry at import time — lands
    // untitled (`import()`'s first-message tier can't help either, since
    // THIS transcript's only user turn is synthetic — a stand-in for the
    // real "no ai-title record was ever written for this old session"
    // scenario). The registry file for it is created only AFTER import.
    let path_a = write_transcript(
        transcripts_dir.path(),
        "a.jsonl",
        vec![
            user_line(
                "sess-a",
                "u1",
                "/work/a",
                "Reply with ONLY a concise title for this chat, no punctuation.",
            ),
            assistant_ack_line("sess-a", "a1", "u1", "/work/a"),
        ],
    );
    core.external_import
        .import("chat-a", "sess-a", &path_a)
        .expect("import a");
    let chat_a = core
        .workspace
        .chat("chat-a")
        .expect("read")
        .expect("exists");
    assert!(chat_a.title.is_none(), "no derivable title at import time");

    // Chat B: no ai-title, no registry entry — falls back to the first real
    // message at import time already. This chat must be LEFT ALONE by the
    // repair (already has a real title) and never overwritten even once a
    // registry entry for its session id shows up later.
    let path_b = write_transcript(
        transcripts_dir.path(),
        "b.jsonl",
        vec![
            user_line("sess-b", "u1", "/work/b", "fix the flaky retry test"),
            assistant_ack_line("sess-b", "a1", "u1", "/work/b"),
        ],
    );
    core.external_import
        .import("chat-b", "sess-b", &path_b)
        .expect("import b");
    let chat_b_before = core
        .workspace
        .chat("chat-b")
        .expect("read")
        .expect("exists");
    assert_eq!(
        chat_b_before.title.as_deref(),
        Some("fix the flaky retry test")
    );

    // Chat C: genuinely nothing derivable at all (transcript with only a
    // synthetic first message, and stays that way) — must end up
    // marker-skipped, not endlessly re-scanned.
    let path_c = write_transcript(
        transcripts_dir.path(),
        "c.jsonl",
        vec![
            user_line(
                "sess-c",
                "u1",
                "/work/c",
                "Classify this coding-agent chat into exactly one task category.",
            ),
            assistant_ack_line("sess-c", "a1", "u1", "/work/c"),
        ],
    );
    core.external_import
        .import("chat-c", "sess-c", &path_c)
        .expect("import c");
    assert!(
        core.workspace
            .chat("chat-c")
            .expect("read")
            .expect("exists")
            .title
            .is_none()
    );

    // Now bring chat A's registry entry into existence and add an ai-title
    // to its transcript too, to prove the repair prefers the registry over
    // the ai-title, exactly like `import()`'s own chain.
    std::fs::create_dir_all(&registry_dir).expect("registry dir");
    std::fs::write(
        registry_dir.join("review-growthbook-wrapper.session-id"),
        "sess-a\n",
    )
    .expect("write registry entry for chat a");
    let mut a_contents = std::fs::read_to_string(&path_a).expect("read a transcript");
    a_contents.push_str(
        &serde_json::json!({"type": "ai-title", "aiTitle": "An AI-generated title nobody asked for", "sessionId": "sess-a"})
            .to_string(),
    );
    a_contents.push('\n');
    std::fs::write(&path_a, a_contents).expect("append ai-title to a transcript");

    // Also give chat B's session a registry entry — must be ignored, since
    // chat B already has a real title.
    std::fs::write(
        registry_dir.join("review-should-be-ignored.session-id"),
        "sess-b\n",
    )
    .expect("write registry entry for chat b (must be ignored)");

    let repaired = core
        .external_import
        .repair_missing_titles(&core.context_usage)
        .expect("repair pass");
    assert_eq!(
        repaired, 1,
        "only chat A should have been resolved (B already titled, C undecidable)"
    );

    let chat_a_after = core
        .workspace
        .chat("chat-a")
        .expect("read")
        .expect("exists");
    assert_eq!(
        chat_a_after.title.as_deref(),
        Some("growthbook-wrapper"),
        "registry name must beat the ai-title added afterward"
    );

    let chat_b_after = core
        .workspace
        .chat("chat-b")
        .expect("read")
        .expect("exists");
    assert_eq!(
        chat_b_after.title.as_deref(),
        Some("fix the flaky retry test"),
        "an already-titled chat must never be overwritten, even with a matching registry entry"
    );

    let chat_c_after = core
        .workspace
        .chat("chat-c")
        .expect("read")
        .expect("exists");
    assert!(
        chat_c_after.title.is_none(),
        "chat C has nothing derivable and must stay untitled"
    );

    // Idempotent: a second pass repairs nothing further (A is done, B was
    // never touched, C is marker-skipped).
    let repaired_again = core
        .external_import
        .repair_missing_titles(&core.context_usage)
        .expect("second repair pass");
    assert_eq!(repaired_again, 0);

    // Prove C really is marker-skipped, not just "still undecidable": even
    // if C's transcript later gains a registry entry, a THIRD pass must not
    // pick it up — the marker file short-circuits before re-parsing.
    std::fs::write(registry_dir.join("review-too-late.session-id"), "sess-c\n")
        .expect("write registry entry for chat c (too late — must be ignored)");
    let repaired_third = core
        .external_import
        .repair_missing_titles(&core.context_usage)
        .expect("third repair pass");
    assert_eq!(
        repaired_third, 0,
        "marker-skipped chat must not be re-resolved even once data appears"
    );
    let chat_c_third = core
        .workspace
        .chat("chat-c")
        .expect("read")
        .expect("exists");
    assert!(chat_c_third.title.is_none());

    core.shutdown().await;

    // SAFETY: restoring whatever HOME was before this test touched it.
    unsafe {
        match previous_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }
}

/// The repair must never touch a NATIVE chat (no import cursor at all) —
/// same "imported, not native" gate `repair_junk_imports` relies on
/// (`read_cursor` returning `None`).
#[tokio::test]
async fn repair_never_touches_a_native_chat_with_no_import_cursor() {
    let dir = tempfile::tempdir().expect("tempdir");
    let core = assemble(EngineProfile::development(
        dir.path(),
        "dev-org",
        "dev-user",
    ));

    // A native chat: created directly through the workspace host, never
    // through `ExternalSessionImporter::import` — no cursor, and critically
    // no `harness_session_id` either at creation (a brand-new chat has
    // neither), which alone is enough to exclude it.
    core.workspace
        .create_chat(
            "native-chat",
            None,
            Some(&core.device_id),
            None,
            Some("~/work".to_string()),
        )
        .expect("create native chat");
    assert!(
        core.workspace
            .chat("native-chat")
            .expect("read")
            .expect("exists")
            .title
            .is_none()
    );

    let repaired = core
        .external_import
        .repair_missing_titles(&core.context_usage)
        .expect("repair pass");
    assert_eq!(
        repaired, 0,
        "a chat with no harness_session_id/import cursor must never be touched"
    );

    let native_after = core
        .workspace
        .chat("native-chat")
        .expect("read")
        .expect("exists");
    assert!(
        native_after.title.is_none(),
        "still untitled — the repair must not have invented anything"
    );

    core.shutdown().await;
}

// The cursor-less "chat Zeron itself launched" case (the actual reported
// miss) and the "harness session but no findable transcript at all" marker
// case live in their own test files —
// `external_import_title_repair_harness_session.rs` /
// `external_import_title_repair_no_transcript.rs` — since each mutates the
// process-global `$HOME` env var and this file already has one such test
// above; more than one per binary races (see those files' doc comments).
