//! CI-safe (synthetic, no real machine data) regression test for the
//! `SCAN_CHAT_SUBAGENTS` bug: a chat Zeron itself launched or resumed (via
//! `WorkspaceHost::set_chat_harness_session`, never through
//! `ExternalSessionImporter::import`) has no external-import cursor, so
//! `ContextUsageProvider::transcript_path_for_chat` — the same two-source
//! resolution the RPC handler now uses — must still resolve its transcript
//! via the harness-session fallback, and `subagent_scan::scan_subagents` must
//! then find whatever's on disk next to it. Same `$HOME`-redirection
//! convention as `external_import_bulk_session_canvas.rs`.

use std::sync::Arc;

use zeron_engine::{EngineCore, EngineProfile, HarnessId, default_registry};

fn assemble(profile: EngineProfile) -> EngineCore {
    EngineCore::assemble_with_profile(profile, Arc::new(default_registry()), HarnessId::Mock, None)
        .expect("assemble profile")
}

#[tokio::test]
async fn resolves_and_scans_subagents_for_a_harness_session_only_chat() {
    let fake_home = tempfile::tempdir().expect("fake home tempdir");
    let previous_home = std::env::var_os("HOME");
    // SAFETY: test-only, scoped to this one function, restored at the end —
    // same justification as `external_import_bulk_session_canvas.rs`.
    unsafe {
        std::env::set_var("HOME", fake_home.path());
    }

    let session_id = "sess-live-launched";
    let project_dir = fake_home
        .path()
        .join(".claude")
        .join("projects")
        .join("proj-1");
    std::fs::create_dir_all(&project_dir).expect("project dir");
    let transcript_path = project_dir.join(format!("{session_id}.jsonl"));
    std::fs::write(
        &transcript_path,
        r#"{"parentUuid":null,"isSidechain":false,"type":"user","message":{"role":"user","content":"do a thing"},"uuid":"u1","timestamp":"2026-01-01T00:00:01.000Z","cwd":"/work/project","sessionId":"sess-live-launched"}"#,
    )
    .expect("write transcript");

    let subagents_dir = project_dir.join(session_id).join("subagents");
    std::fs::create_dir_all(&subagents_dir).expect("subagents dir");
    std::fs::write(
        subagents_dir.join("agent-sub1.meta.json"),
        r#"{"agentType":"fork","description":"Do a subtask"}"#,
    )
    .expect("write meta");
    std::fs::write(subagents_dir.join("agent-sub1.jsonl"), "{}\n")
        .expect("write subagent transcript");

    let data_dir = tempfile::tempdir().expect("data dir");
    let core = assemble(EngineProfile::development(
        data_dir.path(),
        "dev-org",
        "dev-user",
    ));

    let chat_id = "live-launched-chat";
    core.workspace
        .create_chat(
            chat_id,
            None,
            Some(&core.device_id),
            None,
            Some("/work/project".into()),
        )
        .expect("create chat");
    // The live run loop's own path — deliberately NOT `external_import::import`,
    // so this chat has no sync cursor at all.
    core.workspace
        .set_chat_harness_session(chat_id, session_id, "/work/project");

    assert!(
        core.external_import
            .transcript_path_for(chat_id)
            .expect("transcript_path_for")
            .is_none(),
        "must have no import cursor for this test to exercise the fallback"
    );

    let resolved = core
        .context_usage
        .transcript_path_for_chat(&core.workspace, &core.external_import, chat_id)
        .expect("transcript_path_for_chat")
        .expect("must resolve via the harness-session fallback");
    assert_eq!(resolved, transcript_path);

    let subagents = zeron_engine::subagent_scan::scan_subagents(&resolved).expect("scan_subagents");
    assert_eq!(subagents.len(), 1);
    assert_eq!(subagents[0].agent_id, "sub1");
    assert_eq!(subagents[0].description.as_deref(), Some("Do a subtask"));

    core.shutdown().await;

    // SAFETY: restoring whatever HOME was before this test touched it.
    unsafe {
        match previous_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }
}
