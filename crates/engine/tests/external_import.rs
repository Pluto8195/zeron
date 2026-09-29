//! External-session import: on-disk Claude Code transcript → a brand-new
//! Zeron chat with the same content a live session would have produced.

use std::sync::Arc;

use zeron_engine::{EngineCore, EngineProfile, HarnessId, default_registry};

fn assemble(profile: EngineProfile) -> EngineCore {
    EngineCore::assemble_with_profile(profile, Arc::new(default_registry()), HarnessId::Mock, None)
        .expect("assemble profile")
}

/// A synthetic on-disk transcript shaped like a real `~/.claude/projects/
/// <project>/<uuid>.jsonl` file: two human turns, a tool call + its result
/// mid-turn, a subagent (`isSidechain`) message that must be skipped, and
/// the CLI-internal bookkeeping lines (`queue-operation`/`attachment`/
/// `last-prompt`) that must be dropped as they never reach a live chat's
/// transcript either.
fn synthetic_transcript() -> String {
    [
        r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-01-01T00:00:00.000Z","sessionId":"sess-1","content":"add a health check endpoint"}"#,
        r#"{"type":"queue-operation","operation":"dequeue","timestamp":"2026-01-01T00:00:00.000Z","sessionId":"sess-1"}"#,
        r#"{"parentUuid":null,"isSidechain":false,"type":"user","message":{"role":"user","content":"add a health check endpoint"},"uuid":"u1","timestamp":"2026-01-01T00:00:01.000Z","cwd":"/work/project","sessionId":"sess-1"}"#,
        r#"{"parentUuid":"u1","isSidechain":false,"type":"attachment","attachment":{"type":"environment","snapshot":{"workingDirectory":"/work/project","isGitRepo":true}},"uuid":"a1","timestamp":"2026-01-01T00:00:01.100Z","cwd":"/work/project","sessionId":"sess-1"}"#,
        r#"{"parentUuid":"a1","isSidechain":false,"type":"assistant","message":{"model":"claude-x","content":[{"type":"text","text":"I'll add a health check route."},{"type":"tool_use","id":"tool-1","name":"Write","input":{"file_path":"/work/project/health.rs","content":"fn health() {}"}}]},"uuid":"asst1","timestamp":"2026-01-01T00:00:02.000Z","cwd":"/work/project","sessionId":"sess-1"}"#,
        r#"{"parentUuid":"asst1","isSidechain":false,"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool-1","is_error":false,"content":"ok"}]},"uuid":"tr1","timestamp":"2026-01-01T00:00:02.500Z","cwd":"/work/project","sessionId":"sess-1"}"#,
        r#"{"parentUuid":"tr1","isSidechain":false,"type":"assistant","message":{"model":"claude-x","content":[{"type":"text","text":"Done, health check added."}]},"uuid":"asst2","timestamp":"2026-01-01T00:00:03.000Z","cwd":"/work/project","sessionId":"sess-1"}"#,
        r#"{"parentUuid":"asst2","isSidechain":true,"type":"user","message":{"role":"user","content":"a subagent message that must not appear"},"uuid":"sub1","timestamp":"2026-01-01T00:00:03.100Z","cwd":"/work/project","sessionId":"sess-1"}"#,
        r#"{"parentUuid":"asst2","isSidechain":false,"type":"user","message":{"role":"user","content":"thanks, also add a test"},"uuid":"u2","timestamp":"2026-01-01T00:00:04.000Z","cwd":"/work/project","sessionId":"sess-1"}"#,
        r#"{"parentUuid":"u2","isSidechain":false,"type":"assistant","message":{"model":"claude-x","content":[{"type":"text","text":"Added a test too."}]},"uuid":"asst3","timestamp":"2026-01-01T00:00:05.000Z","cwd":"/work/project","sessionId":"sess-1"}"#,
        r#"{"type":"ai-title","aiTitle":"Add health check endpoint","sessionId":"sess-1"}"#,
        r#"{"type":"last-prompt","lastPrompt":"add a health check endpoint","leafUuid":"asst3","sessionId":"sess-1"}"#,
    ]
    .join("\n")
        + "\n"
}

#[tokio::test]
async fn imports_a_transcript_into_a_new_chat_with_full_history() {
    let dir = tempfile::tempdir().expect("tempdir");
    let core = assemble(EngineProfile::development(
        dir.path(),
        "dev-org",
        "dev-user",
    ));

    let transcript_path = dir.path().join("external-session.jsonl");
    std::fs::write(&transcript_path, synthetic_transcript()).expect("write transcript");

    let chat_id = "imported-chat-1";
    let result = core
        .external_import
        .import(chat_id, "external-session-id-1", &transcript_path)
        .expect("import");

    assert_eq!(result.chat_id, chat_id);
    assert_eq!(result.cwd, "/work/project");
    assert_eq!(result.title.as_deref(), Some("Add health check endpoint"));
    // 2 human turns + 2 assistant turns (the tool-result line folds into the
    // first assistant turn rather than becoming its own entry).
    assert_eq!(result.message_count, 4);

    // The chat row: right cwd, attached to a real (found-or-created) Space,
    // titled from the transcript's own `ai-title`.
    let chat = core
        .workspace
        .chat(chat_id)
        .expect("read chat")
        .expect("chat row exists");
    assert_eq!(chat.cwd.as_deref(), Some("/work/project"));
    assert_eq!(chat.title.as_deref(), Some("Add health check endpoint"));
    let space_id = chat.space_id.clone().expect("chat has a space");
    assert_eq!(space_id, result.space_id);
    let space = core
        .workspace
        .space(&space_id)
        .expect("read space")
        .expect("space exists");
    assert_eq!(space.path, "/work/project");
    assert!(space.git_detected, "environment attachment's isGitRepo=true should carry over");

    // Resume seeding: `resume_for`'s primary lookup path reads exactly this.
    let (session_id, session_cwd) = core
        .workspace
        .chat_harness_session(chat_id)
        .expect("harness session stamped");
    assert_eq!(session_id, "external-session-id-1");
    assert_eq!(session_cwd.as_deref(), Some("/work/project"));

    // Re-running against the same cwd must reuse the Space, not create a
    // second one.
    let second_chat_id = "imported-chat-2";
    let result2 = core
        .external_import
        .import(second_chat_id, "external-session-id-2", &transcript_path)
        .expect("second import");
    assert_eq!(result2.space_id, space_id, "same cwd must reuse the Space");

    core.shutdown().await;
}

#[tokio::test]
async fn transcript_content_lands_in_the_doc_with_live_equivalent_shape() {
    use zeron_doc::{MessagePart, MessageRole};

    let dir = tempfile::tempdir().expect("tempdir");
    let profile = EngineProfile::development(dir.path(), "dev-org", "dev-user");
    let store_root = profile.store_root().to_path_buf();
    let core = assemble(profile);

    let transcript_path = dir.path().join("external-session.jsonl");
    std::fs::write(&transcript_path, synthetic_transcript()).expect("write transcript");

    let chat_id = "imported-chat-doc";
    core.external_import
        .import(chat_id, "external-session-id", &transcript_path)
        .expect("import");
    core.shutdown().await;

    // Read the persisted doc back the same way `zeron-doc`'s own rebuild
    // path does: reopen the snapshot from the store into a fresh doc.
    let store = zeron_sync::DocsStore::open(&store_root).expect("open store");
    let bytes = store
        .load_snapshot(chat_id)
        .expect("load snapshot")
        .expect("snapshot exists");
    let loro = loro::LoroDoc::new();
    loro.import(&bytes).expect("import snapshot");
    let doc = zeron_doc::SessionDoc::from_doc(loro);
    let entries = doc.read_entries().expect("read entries");

    assert_eq!(entries.len(), 4);
    assert_eq!(entries[0].role, MessageRole::User);
    assert!(matches!(
        &entries[0].parts[..],
        [MessagePart::Text { text, .. }] if text == "add a health check endpoint"
    ));

    assert_eq!(entries[1].role, MessageRole::Assistant);
    // Text, then a resolved (non-error) Write tool call whose content was
    // stripped by the render-only privacy policy.
    let tool_part = entries[1]
        .parts
        .iter()
        .find_map(|p| match p {
            MessagePart::Tool {
                call, resolved, is_error, ..
            } => Some((call.clone(), *resolved, *is_error)),
            _ => None,
        })
        .expect("assistant turn has a tool part");
    let (call, resolved, is_error) = tool_part;
    assert!(resolved, "tool_result line must resolve the matching Tool part");
    assert!(!is_error);
    match call {
        zeron_proto::ToolCall::WriteFile { path, content } => {
            assert_eq!(path, "/work/project/health.rs");
            assert_eq!(content, None, "file content must be stripped before entering the doc");
        }
        other => panic!("unexpected tool call: {other:?}"),
    }

    assert_eq!(entries[2].role, MessageRole::User);
    assert!(matches!(
        &entries[2].parts[..],
        [MessagePart::Text { text, .. }] if text == "thanks, also add a test"
    ));
    assert_eq!(entries[3].role, MessageRole::Assistant);
}
