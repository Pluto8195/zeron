//! `ExternalSessionImporter::sync`: catching an already-imported chat up on
//! new content its source session accumulated after import (the user kept
//! talking to it outside Zeron).
//!
//! Key semantic under test: an assistant turn is only "safe" to sync once a
//! genuine FOLLOWING HUMAN MESSAGE closes it — not merely by more lines of
//! any kind appearing after it (a `last-prompt`/`ai-title` bookkeeping line
//! does NOT close a turn), and not by EOF (the source session may still be
//! mid-response). This trades a little latency (the very latest reply shows
//! up on the NEXT sync, once you or the CLI logs anything further as a real
//! user turn) for a hard guarantee against ever duplicating a message.

use std::sync::Arc;

use zeron_engine::{EngineCore, EngineProfile, HarnessId, default_registry};

fn assemble(profile: EngineProfile) -> EngineCore {
    EngineCore::assemble_with_profile(profile, Arc::new(default_registry()), HarnessId::Mock, None)
        .expect("assemble profile")
}

fn user_line(uuid: &str, parent: &str, ts: &str, text: &str) -> String {
    format!(
        r#"{{"parentUuid":"{parent}","isSidechain":false,"type":"user","message":{{"role":"user","content":"{text}"}},"uuid":"{uuid}","timestamp":"{ts}","cwd":"/work/project","sessionId":"sess-1"}}"#
    )
}

fn assistant_line(uuid: &str, parent: &str, ts: &str, text: &str) -> String {
    format!(
        r#"{{"parentUuid":"{parent}","isSidechain":false,"type":"assistant","message":{{"model":"claude-x","content":[{{"type":"text","text":"{text}"}}]}},"uuid":"{uuid}","timestamp":"{ts}","cwd":"/work/project","sessionId":"sess-1"}}"#
    )
}

fn bookkeeping_line() -> String {
    // Matches a real transcript's trailing `last-prompt` line — appears
    // after a turn but must NOT be treated as closing it.
    r#"{"type":"last-prompt","lastPrompt":"…","leafUuid":"asst1","sessionId":"sess-1"}"#.to_string()
}

/// The transcript as it stands at import time: one turn, left open (nothing
/// follows `asst1`) — the common case, since import usually happens on
/// whatever the file currently holds.
fn initial_transcript() -> String {
    [
        user_line(
            "u1",
            "null",
            "2026-01-01T00:00:01.000Z",
            "add a health check endpoint",
        ),
        assistant_line(
            "asst1",
            "u1",
            "2026-01-01T00:00:02.000Z",
            "I'll add a health check route.",
        ),
    ]
    .join("\n")
        + "\n"
}

/// Appended after import: a bookkeeping line (must not close asst1's turn),
/// then a genuine second human message — which DOES close asst1's turn (via
/// `flush_assistant_turn`) and is itself immediately safe, but its own reply
/// (`asst2`) is left open again (nothing follows it yet).
fn second_turn_opens() -> String {
    [
        bookkeeping_line(),
        user_line(
            "u2",
            "asst1",
            "2026-01-01T00:00:03.000Z",
            "thanks, also add a test",
        ),
        assistant_line(
            "asst2",
            "u2",
            "2026-01-01T00:00:04.000Z",
            "Added a test too.",
        ),
    ]
    .join("\n")
        + "\n"
}

/// Closes `asst2`'s turn with a third genuine human message.
fn third_message_closes_it() -> String {
    [user_line(
        "u3",
        "asst2",
        "2026-01-01T00:00:05.000Z",
        "great, thanks",
    )]
    .join("\n")
        + "\n"
}

#[tokio::test]
async fn sync_with_no_new_content_is_a_safe_noop() {
    let dir = tempfile::tempdir().expect("tempdir");
    let core = assemble(EngineProfile::development(
        dir.path(),
        "dev-org",
        "dev-user",
    ));
    let transcript_path = dir.path().join("session.jsonl");
    std::fs::write(&transcript_path, initial_transcript()).expect("write transcript");

    let chat_id = "sync-chat-noop";
    core.external_import
        .import(chat_id, "sess-1", &transcript_path)
        .expect("import");

    let result = core.external_import.sync(chat_id).expect("sync");
    assert_eq!(result.new_message_count, 0);
    let result2 = core.external_import.sync(chat_id).expect("sync again");
    assert_eq!(result2.new_message_count, 0);

    core.shutdown().await;
}

#[tokio::test]
async fn sync_picks_up_a_closed_message_but_defers_the_new_open_reply() {
    let dir = tempfile::tempdir().expect("tempdir");
    let profile = EngineProfile::development(dir.path(), "dev-org", "dev-user");
    let store_root = profile.store_root().to_path_buf();
    let core = assemble(profile);
    let transcript_path = dir.path().join("session.jsonl");
    std::fs::write(&transcript_path, initial_transcript()).expect("write transcript");

    let chat_id = "sync-chat-appends";
    let initial = core
        .external_import
        .import(chat_id, "sess-1", &transcript_path)
        .expect("import");
    assert_eq!(initial.message_count, 2); // u1 + asst1 (asst1 included even though open, per import's full-content policy)

    // The source session keeps going, outside Zeron: a bookkeeping line
    // (ignored), then a real follow-up message (closes asst1's turn), then
    // a NEW assistant reply that is itself left open.
    let mut grown = initial_transcript();
    grown.push_str(&second_turn_opens());
    std::fs::write(&transcript_path, &grown).expect("append to transcript");

    let result = core.external_import.sync(chat_id).expect("sync");
    assert_eq!(
        result.new_message_count, 1,
        "only u2 is safe — asst2's turn is still open, deferred to the next sync"
    );

    // Nothing further to add while the tail stays open.
    let result_repeat = core
        .external_import
        .sync(chat_id)
        .expect("sync again, still open");
    assert_eq!(result_repeat.new_message_count, 0);

    // A third message closes asst2's turn.
    grown.push_str(&third_message_closes_it());
    std::fs::write(&transcript_path, &grown).expect("close the turn");

    let result_final = core
        .external_import
        .sync(chat_id)
        .expect("sync after close");
    assert_eq!(result_final.new_message_count, 2, "asst2 (now closed) + u3");

    core.shutdown().await;

    // Verify the doc directly: 5 entries total, correct order, no duplicates.
    let store = zeron_sync::DocsStore::open(&store_root).expect("open store");
    let bytes = store
        .load_snapshot(chat_id)
        .expect("load snapshot")
        .expect("snapshot exists");
    let loro = loro::LoroDoc::new();
    loro.import(&bytes).expect("import snapshot");
    let doc = zeron_doc::SessionDoc::from_doc(loro);
    let entries = doc.read_entries().expect("read entries");
    let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, vec!["u1", "asst1", "u2", "asst2", "u3"]);
}

#[tokio::test]
async fn sync_on_a_never_imported_chat_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let core = assemble(EngineProfile::development(
        dir.path(),
        "dev-org",
        "dev-user",
    ));
    let err = core
        .external_import
        .sync("no-such-chat")
        .expect_err("must error, not silently no-op");
    assert!(err.to_string().contains("no external-import record"));
    core.shutdown().await;
}
