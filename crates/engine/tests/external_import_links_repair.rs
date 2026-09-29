//! Durable PR/ticket links (ticket 0xx) backfilled onto chats imported
//! before the feature existed — `ExternalSessionImporter::repair_missing_links`.
//! Covers the realistic Claude Code signal (a PR URL/ticket id the agent
//! narrates back in its own message text — the only reliable one for a real
//! on-disk transcript; see `external_import.rs`'s `mine_links_from_entries`
//! doc comment for why tool-result text isn't), plus the pass's idempotency
//! and its "already linked" / "no chat row" skip guards.

use std::sync::Arc;

use zeron_engine::{EngineCore, EngineProfile, HarnessId, default_registry};
use zeron_proto::ChatLinkSource;

fn assemble(profile: EngineProfile) -> EngineCore {
    EngineCore::assemble_with_profile(profile, Arc::new(default_registry()), HarnessId::Mock, None)
        .expect("assemble profile")
}

/// A synthetic on-disk transcript where the agent creates a PR and narrates
/// its URL back — the realistic "mentioned" signal for a real Claude Code
/// transcript (`mine_links_from_entries` never sees real tool-result text).
fn transcript_with_pr_mention() -> String {
    [
        r#"{"parentUuid":null,"isSidechain":false,"type":"user","message":{"role":"user","content":"open a pr for this fix, ticket ENG-2715"},"uuid":"u1","timestamp":"2026-01-01T00:00:01.000Z","cwd":"/work/project","sessionId":"sess-1"}"#,
        r#"{"parentUuid":"u1","isSidechain":false,"type":"assistant","message":{"model":"claude-x","content":[{"type":"text","text":"Opening a PR now."},{"type":"tool_use","id":"tool-1","name":"Bash","input":{"command":"gh pr create --title 'Fix thing' --body '…'"}}]},"uuid":"asst1","timestamp":"2026-01-01T00:00:02.000Z","cwd":"/work/project","sessionId":"sess-1"}"#,
        r#"{"parentUuid":"asst1","isSidechain":false,"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool-1","is_error":false,"content":"https://github.com/acme/widgets/pull/42"}]},"uuid":"tr1","timestamp":"2026-01-01T00:00:02.500Z","cwd":"/work/project","sessionId":"sess-1"}"#,
        r#"{"parentUuid":"tr1","isSidechain":false,"type":"assistant","message":{"model":"claude-x","content":[{"type":"text","text":"Opened https://github.com/acme/widgets/pull/42 for ENG-2715."}]},"uuid":"asst2","timestamp":"2026-01-01T00:00:03.000Z","cwd":"/work/project","sessionId":"sess-1"}"#,
    ]
    .join("\n")
        + "\n"
}

/// A synthetic transcript with no PR/ticket signal at all.
fn transcript_with_no_link() -> String {
    [
        r#"{"parentUuid":null,"isSidechain":false,"type":"user","message":{"role":"user","content":"hello"},"uuid":"u1","timestamp":"2026-01-01T00:00:01.000Z","cwd":"/work/project","sessionId":"sess-1"}"#,
        r#"{"parentUuid":"u1","isSidechain":false,"type":"assistant","message":{"model":"claude-x","content":[{"type":"text","text":"hi there"}]},"uuid":"asst1","timestamp":"2026-01-01T00:00:02.000Z","cwd":"/work/project","sessionId":"sess-1"}"#,
    ]
    .join("\n")
        + "\n"
}

#[tokio::test]
async fn repair_mines_a_pr_and_ticket_mention_from_the_transcript() {
    let dir = tempfile::tempdir().expect("tempdir");
    let core = assemble(EngineProfile::development(dir.path(), "dev-org", "dev-user"));

    let transcript_path = dir.path().join("external-session.jsonl");
    std::fs::write(&transcript_path, transcript_with_pr_mention()).expect("write transcript");

    let chat_id = "link-less-chat";
    core.external_import
        .import(chat_id, "external-session-id-1", &transcript_path)
        .expect("import");

    let before = core.workspace.chat(chat_id).expect("read chat").expect("chat exists");
    assert!(before.linked_pr_url.is_none(), "precondition: no link yet");
    assert!(before.linked_ticket_id.is_none());

    let repaired = core
        .external_import
        .repair_missing_links(&core.context_usage)
        .expect("repair pass");
    assert_eq!(repaired, 1);

    let after = core.workspace.chat(chat_id).expect("read chat").expect("chat exists");
    assert_eq!(after.linked_pr_url.as_deref(), Some("https://github.com/acme/widgets/pull/42"));
    assert_eq!(after.linked_pr_source, Some(ChatLinkSource::Mentioned));
    assert_eq!(after.linked_ticket_id.as_deref(), Some("ENG-2715"));
    assert_eq!(after.linked_ticket_source, Some(ChatLinkSource::Mentioned));

    // Idempotent: a second pass finds nothing left to repair (the chat now
    // carries both links).
    let repaired_again = core
        .external_import
        .repair_missing_links(&core.context_usage)
        .expect("second repair pass");
    assert_eq!(repaired_again, 0);

    core.shutdown().await;
}

#[tokio::test]
async fn repair_marks_a_link_less_chat_scanned_so_it_is_never_reparsed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let core = assemble(EngineProfile::development(dir.path(), "dev-org", "dev-user"));

    let transcript_path = dir.path().join("external-session.jsonl");
    std::fs::write(&transcript_path, transcript_with_no_link()).expect("write transcript");

    let chat_id = "genuinely-linkless-chat";
    core.external_import
        .import(chat_id, "external-session-id-2", &transcript_path)
        .expect("import");

    let repaired = core
        .external_import
        .repair_missing_links(&core.context_usage)
        .expect("repair pass");
    assert_eq!(repaired, 0, "nothing to mine — no link written");

    let chat = core.workspace.chat(chat_id).expect("read chat").expect("chat exists");
    assert!(chat.linked_pr_url.is_none());
    assert!(chat.linked_ticket_id.is_none());

    // Deleting the source transcript proves the SECOND pass never re-reads
    // it — the marker file skips the chat outright, not a re-parse that
    // just happens to find nothing again.
    std::fs::remove_file(&transcript_path).expect("remove transcript");
    let repaired_again = core
        .external_import
        .repair_missing_links(&core.context_usage)
        .expect("second repair pass");
    assert_eq!(repaired_again, 0);

    core.shutdown().await;
}

#[tokio::test]
async fn repair_never_touches_a_chat_that_already_has_a_link() {
    let dir = tempfile::tempdir().expect("tempdir");
    let core = assemble(EngineProfile::development(dir.path(), "dev-org", "dev-user"));

    let transcript_path = dir.path().join("external-session.jsonl");
    std::fs::write(&transcript_path, transcript_with_pr_mention()).expect("write transcript");

    let chat_id = "already-linked-chat";
    core.external_import
        .import(chat_id, "external-session-id-3", &transcript_path)
        .expect("import");

    // A manual link set before the repair ever runs — e.g. via SET_CHAT_LINK
    // — must win outright; the repair must not even attempt a mined write
    // (which would be blocked by precedence anyway, but this proves the
    // "already has a link" skip fires BEFORE parsing the transcript at all).
    core.workspace
        .set_chat_link(
            chat_id,
            zeron_engine::workspace_host::ChatLinkKind::Pr,
            Some("https://github.com/acme/widgets/pull/999"),
            ChatLinkSource::Manual,
        )
        .expect("manual link write");

    let repaired = core
        .external_import
        .repair_missing_links(&core.context_usage)
        .expect("repair pass");
    assert_eq!(repaired, 0, "a chat that already has a link must be skipped outright");

    let chat = core.workspace.chat(chat_id).expect("read chat").expect("chat exists");
    assert_eq!(chat.linked_pr_url.as_deref(), Some("https://github.com/acme/widgets/pull/999"));
    assert_eq!(chat.linked_pr_source, Some(ChatLinkSource::Manual));

    core.shutdown().await;
}
