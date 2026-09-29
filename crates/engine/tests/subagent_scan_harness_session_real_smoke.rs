//! Manual smoke test against REAL data on this machine for the exact bug
//! reported live: `SCAN_CHAT_SUBAGENTS` never showed subagent rows for a chat
//! Zeron itself launched (as opposed to one adopted via
//! `ExternalSessionImporter::import`), because the RPC handler only ever
//! tried `ExternalSessionImporter::transcript_path_for` (which reads a
//! per-chat import-cursor file that a live-launched chat never has) instead
//! of also falling back to the chat's own recorded harness session id, the
//! way `ContextUsageProvider::context_pct_for_chat` already did for context
//! usage. Same `#[ignore]` convention as `external_import_real_smoke.rs`/
//! `subagent_scan_real_smoke.rs` — depends on a real `~/.claude/projects`
//! session on this machine, known (per the bug report) to have a
//! `subagents/` directory sibling to its own transcript.
//!
//! Run with:
//!   cargo test -p zeron-engine --test subagent_scan_harness_session_real_smoke -- --ignored --nocapture

use std::sync::Arc;

use zeron_engine::{EngineCore, EngineProfile, HarnessId, default_registry};

#[tokio::test]
#[ignore = "depends on real ~/.claude/projects data on this machine"]
async fn resolves_and_scans_subagents_for_a_harness_session_chat_never_imported() {
    let real_session_id = "8de11ea8-2a2a-42a2-9784-81b4142b404d";
    let real_cwd = "/Users/mikey/Projects/agent-mode-tools";
    let real_transcript = std::path::Path::new(
        "/Users/mikey/.claude/projects/-Users-mikey-Projects-agent-mode-tools/8de11ea8-2a2a-42a2-9784-81b4142b404d.jsonl",
    );
    assert!(real_transcript.is_file(), "expected real transcript at {real_transcript:?}");

    let dir = tempfile::tempdir().expect("tempdir");
    let core = EngineCore::assemble_with_profile(
        EngineProfile::development(dir.path(), "dev-org", "dev-user"),
        Arc::new(default_registry()),
        HarnessId::Mock,
        None,
    )
    .expect("assemble profile");

    let chat_id = "harness-session-subagent-smoke-chat";
    core.workspace
        .create_chat(chat_id, None, Some(&core.device_id), None, Some(real_cwd.to_string()))
        .expect("create chat");
    // The live run loop's own path (`sessions.rs`) for stamping a chat with
    // its harness session id — deliberately NOT going through
    // `ExternalSessionImporter::import`, so no sync-cursor file exists for
    // this chat. This is exactly the shape of every chat Zeron itself
    // launches or resumes.
    core.workspace.set_chat_harness_session(chat_id, real_session_id, real_cwd);

    // Sanity: confirm there really is no import cursor for this chat, so the
    // test actually exercises the fallback path and isn't accidentally
    // passing via the cursor route.
    assert!(
        core.external_import
            .transcript_path_for(chat_id)
            .expect("transcript_path_for")
            .is_none(),
        "this chat must have no external-import cursor for this test to mean anything"
    );

    let resolved = core
        .context_usage
        .transcript_path_for_chat(&core.workspace, &core.external_import, chat_id)
        .expect("transcript_path_for_chat");
    eprintln!("resolved transcript path: {resolved:?}");
    assert_eq!(
        resolved.as_deref(),
        Some(real_transcript),
        "must resolve via the harness-session fallback, not just the import cursor"
    );

    let subagents = zeron_engine::subagent_scan::scan_subagents(&resolved.unwrap()).expect("scan_subagents");
    eprintln!("found {} subagents", subagents.len());
    for s in &subagents {
        eprintln!("{s:?}");
    }
    assert!(
        !subagents.is_empty(),
        "this session is known (per the live bug report) to have spawned subagents on disk"
    );

    core.shutdown().await;
}
