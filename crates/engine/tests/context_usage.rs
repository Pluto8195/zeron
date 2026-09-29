//! `ContextUsageProvider::context_pct_for_chat`: the overview's tile-sizing
//! signal (session-canvas parity) — exercised end to end through a real
//! `EngineCore`, covering both sources (live doc stamp, transcript-tail
//! fallback via an externally-imported chat) and the "nothing to compute
//! from" case.

use std::sync::Arc;

use zeron_doc::SessionDoc;
use zeron_engine::context_usage::pct_from_tokens;
use zeron_engine::{EngineCore, EngineProfile, HarnessId, default_registry};

fn assemble(profile: EngineProfile) -> EngineCore {
    EngineCore::assemble_with_profile(profile, Arc::new(default_registry()), HarnessId::Mock, None)
        .expect("assemble profile")
}

#[tokio::test]
async fn reads_a_live_doc_stamped_context_usage() {
    let dir = tempfile::tempdir().expect("tempdir");
    let profile = EngineProfile::development(dir.path(), "dev-org", "dev-user");
    let store_root = profile.store_root().to_path_buf();
    let core = assemble(profile);

    let chat_id = "live-chat";
    core.workspace
        .create_chat(chat_id, None, Some(&core.device_id), None, Some("/work/project".into()))
        .expect("create chat row");

    // Simulate what the live run loop's `AgentEvent::ContextUsage` handling
    // (`sessions.rs`) does: stamp the doc's own `contextUsage` meta field and
    // persist the snapshot — without ever running an actual turn.
    let doc = SessionDoc::init(chat_id).expect("init doc");
    doc.update_context_usage(Some(60_000), Some(1_000_000))
        .expect("stamp context usage");
    let bytes = doc.export_snapshot().expect("snapshot");
    let store = zeron_sync::DocsStore::open(&store_root).expect("open store");
    store
        .save_snapshot_with_cursor(chat_id, &bytes, 0, 2)
        .expect("save doc");
    drop(store);

    let pct = core
        .context_usage
        .context_pct_for_chat(&core.workspace, &core.external_import, chat_id)
        .expect("context pct")
        .expect("must resolve from the live doc stamp");
    // Fixed 200k divisor, NOT the doc's own recorded 1_000_000 window.
    assert_eq!(pct, pct_from_tokens(60_000));

    core.shutdown().await;
}

/// A synthetic on-disk transcript whose final assistant message carries a
/// real `usage` block — the exact shape a real Claude Code transcript has.
fn synthetic_transcript_with_usage(input: u64, cache_read: u64, cache_creation: u64) -> String {
    [
        r#"{"parentUuid":null,"isSidechain":false,"type":"user","message":{"role":"user","content":"hi"},"uuid":"u1","timestamp":"2026-01-01T00:00:01.000Z","cwd":"/work/project","sessionId":"sess-1"}"#.to_string(),
        format!(
            r#"{{"parentUuid":"u1","isSidechain":false,"type":"assistant","message":{{"model":"claude-x","content":[{{"type":"text","text":"hello"}}],"usage":{{"input_tokens":{input},"cache_read_input_tokens":{cache_read},"cache_creation_input_tokens":{cache_creation}}}}},"uuid":"asst1","timestamp":"2026-01-01T00:00:02.000Z","cwd":"/work/project","sessionId":"sess-1"}}"#
        ),
    ]
    .join("\n")
        + "\n"
}

#[tokio::test]
async fn falls_back_to_the_transcript_tail_for_an_externally_imported_chat() {
    let dir = tempfile::tempdir().expect("tempdir");
    let core = assemble(EngineProfile::development(dir.path(), "dev-org", "dev-user"));

    let transcript_path = dir.path().join("external-session.jsonl");
    std::fs::write(&transcript_path, synthetic_transcript_with_usage(500, 2000, 300)).expect("write transcript");

    let chat_id = "imported-chat";
    core.external_import
        .import(chat_id, "external-session-id-1", &transcript_path)
        .expect("import");

    // `import()` never calls `update_context_usage` — this chat's doc has NO
    // live stamp, so this proves the fallback path, not the live one.
    let pct = core
        .context_usage
        .context_pct_for_chat(&core.workspace, &core.external_import, chat_id)
        .expect("context pct")
        .expect("must resolve from the transcript tail");
    assert_eq!(pct, pct_from_tokens(500 + 2000 + 300));

    core.shutdown().await;
}

#[tokio::test]
async fn a_transcript_with_no_usage_anywhere_resolves_to_none() {
    let dir = tempfile::tempdir().expect("tempdir");
    let core = assemble(EngineProfile::development(dir.path(), "dev-org", "dev-user"));

    let transcript_path = dir.path().join("no-usage-session.jsonl");
    std::fs::write(
        &transcript_path,
        [
            r#"{"parentUuid":null,"isSidechain":false,"type":"user","message":{"role":"user","content":"hi"},"uuid":"u1","timestamp":"2026-01-01T00:00:01.000Z","cwd":"/work/project","sessionId":"sess-1"}"#,
            r#"{"parentUuid":"u1","isSidechain":false,"type":"assistant","message":{"model":"claude-x","content":[{"type":"text","text":"hello, no usage block at all"}]},"uuid":"asst1","timestamp":"2026-01-01T00:00:02.000Z","cwd":"/work/project","sessionId":"sess-1"}"#,
        ]
        .join("\n"),
    )
    .expect("write transcript");

    let chat_id = "no-usage-chat";
    core.external_import
        .import(chat_id, "external-session-id-2", &transcript_path)
        .expect("import");

    let pct = core
        .context_usage
        .context_pct_for_chat(&core.workspace, &core.external_import, chat_id)
        .expect("context pct");
    assert_eq!(pct, None);

    core.shutdown().await;
}

#[tokio::test]
async fn a_chat_with_neither_a_live_stamp_nor_any_resolvable_transcript_is_none() {
    let dir = tempfile::tempdir().expect("tempdir");
    let core = assemble(EngineProfile::development(dir.path(), "dev-org", "dev-user"));

    let chat_id = "bare-native-chat";
    core.workspace
        .create_chat(chat_id, None, Some(&core.device_id), None, None)
        .expect("create chat row");

    let pct = core
        .context_usage
        .context_pct_for_chat(&core.workspace, &core.external_import, chat_id)
        .expect("context pct");
    assert_eq!(pct, None);

    core.shutdown().await;
}
