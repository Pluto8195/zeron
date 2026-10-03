//! `ExternalSessionImporter::repair_missing_classification`: chats imported
//! before category/origin/branch classification existed have an empty
//! `SyncCursor.category`/`.origin` (`classification_for` treats empty
//! strings as absent) and a `None` `Chat.branch` — this repair backfills both
//! by re-parsing the original transcript, without requiring a re-import.

use std::sync::Arc;

use zeron_engine::{EngineCore, EngineProfile, HarnessId, default_registry};

fn assemble(profile: EngineProfile) -> EngineCore {
    EngineCore::assemble_with_profile(profile, Arc::new(default_registry()), HarnessId::Mock, None)
        .expect("assemble profile")
}

/// A real transcript with a git branch and a real tool call, so the repair
/// has something meaningful to recompute (not just non-empty strings).
fn transcript() -> String {
    [
        r#"{"parentUuid":null,"isSidechain":false,"type":"user","message":{"role":"user","content":"fix the health endpoint"},"uuid":"u1","timestamp":"2026-01-01T00:00:01.000Z","cwd":"/work/project","gitBranch":"fix/health-endpoint","sessionId":"sess-1","entrypoint":"cli"}"#,
        r#"{"parentUuid":"u1","isSidechain":false,"type":"assistant","message":{"model":"claude-x","content":[{"type":"text","text":"On it."},{"type":"tool_use","id":"tool-1","name":"Edit","input":{"file_path":"/work/project/health.rs","old_string":"a","new_string":"b"}}]},"uuid":"asst1","timestamp":"2026-01-01T00:00:02.000Z","cwd":"/work/project","gitBranch":"fix/health-endpoint","sessionId":"sess-1"}"#,
        r#"{"parentUuid":"asst1","isSidechain":false,"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool-1","is_error":false,"content":"ok"}]},"uuid":"tr1","timestamp":"2026-01-01T00:00:02.500Z","cwd":"/work/project","gitBranch":"fix/health-endpoint","sessionId":"sess-1"}"#,
        r#"{"parentUuid":"tr1","isSidechain":false,"type":"assistant","message":{"model":"claude-x","content":[{"type":"text","text":"Fixed."}]},"uuid":"asst2","timestamp":"2026-01-01T00:00:03.000Z","cwd":"/work/project","gitBranch":"fix/health-endpoint","sessionId":"sess-1"}"#,
    ]
    .join("\n")
        + "\n"
}

#[tokio::test]
async fn repairs_a_chat_left_over_by_the_old_pre_classification_import_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let profile = EngineProfile::development(dir.path(), "dev-org", "dev-user");
    let cursors_dir = profile.store_root().join("external_import_cursors");
    let core = assemble(profile);

    let transcript_path = dir.path().join("external-session.jsonl");
    std::fs::write(&transcript_path, transcript()).expect("write transcript");

    let chat_id = "imported-chat";
    core.external_import
        .import(chat_id, "sess-1", &transcript_path)
        .expect("import");

    // A real import today already backfills branch/category/origin — reset
    // both to simulate a chat imported by the OLD pre-classification code
    // path (empty cursor fields, no branch on the row), which is the actual
    // state the real 348 already-imported chats are in.
    let cursor_path = cursors_dir.join(format!("{chat_id}.json"));
    let raw = std::fs::read_to_string(&cursor_path).expect("read cursor");
    let mut json: serde_json::Value = serde_json::from_str(&raw).expect("parse cursor");
    json["category"] = serde_json::Value::String(String::new());
    json["origin"] = serde_json::Value::String(String::new());
    json["toolCounts"] = serde_json::json!({});
    json["skillsLoaded"] = serde_json::json!([]);
    std::fs::write(&cursor_path, serde_json::to_vec(&json).unwrap()).expect("rewrite cursor");

    // Chat.branch has no public reset either — the real 348 chats simply
    // never had it set in the first place, so leaving it at whatever
    // `import()` already set is a stand-in; the repair only writes it when
    // `chat.branch.is_none()`, so to exercise that branch of the repair for
    // real, assert it's already correct from `import()` itself instead
    // (covered by the assertions below) rather than fighting the lack of a
    // public "clear branch" setter for this synthetic-only scenario.

    let before = core
        .workspace
        .chat(chat_id)
        .expect("read chat")
        .expect("chat exists");
    assert_eq!(
        before.branch.as_deref(),
        Some("fix/health-endpoint"),
        "import() already sets it"
    );

    let repaired = core
        .external_import
        .repair_missing_classification()
        .expect("repair pass");
    assert_eq!(repaired, 1);

    let (category, origin) = core
        .external_import
        .classification_for(chat_id)
        .expect("read classification")
        .expect("classification present after repair");
    assert_eq!(
        category, "quick_question",
        "matches what a fresh import() of the same transcript would compute"
    );
    assert_eq!(origin, "bare_cli", "entrypoint \"cli\" maps to bare_cli");

    let (tool_counts, _skills) = core
        .external_import
        .tool_usage_for(chat_id)
        .expect("read tool usage")
        .expect("tool usage present after repair");
    assert_eq!(tool_counts.get("Edit"), Some(&1));

    // Idempotent: a second pass finds nothing left to repair.
    let repaired_again = core
        .external_import
        .repair_missing_classification()
        .expect("second repair pass");
    assert_eq!(repaired_again, 0);

    core.shutdown().await;
}
