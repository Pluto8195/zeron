//! `ExternalSessionImporter::repair_missing_titles`, cursor-less case: a chat
//! Zeron itself launched (via `WorkspaceHost::set_chat_harness_session`, e.g.
//! the `review-zeron-fork` registry-file case) has no import cursor at all,
//! so the OLD `read_cursor(chat_id).is_some()` gate skipped it forever even
//! though it has a real, findable transcript and a registry entry. Kept in
//! its own file (rather than alongside `external_import_title_repair.rs`'s
//! other cases) since it mutates the process-global `$HOME` env var — same
//! "at most one `$HOME`-mutating test per binary" convention every other
//! file in this crate that does this follows, to avoid a cross-test race
//! within one test binary.

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

#[tokio::test]
async fn repair_resolves_a_cursor_less_chat_via_its_harness_session() {
    let fake_home = tempfile::tempdir().expect("fake home tempdir");
    let previous_home = std::env::var_os("HOME");
    // SAFETY: test-only, scoped to this one function, restored at the end —
    // same justification as `external_import_bulk_session_canvas.rs`.
    unsafe {
        std::env::set_var("HOME", fake_home.path());
    }

    let session_id = "29fd48ab-0258-4406-b725-26265045e05f";
    let project_dir = fake_home
        .path()
        .join(".claude")
        .join("projects")
        .join("proj-1");
    std::fs::create_dir_all(&project_dir).expect("project dir");
    let transcript_path = project_dir.join(format!("{session_id}.jsonl"));
    std::fs::write(
        &transcript_path,
        transcript_with_lines(vec![
            user_line(
                session_id,
                "u1",
                "/work/zeron",
                "review the zeron fork changes",
            ),
            assistant_ack_line(session_id, "a1", "u1", "/work/zeron"),
        ]),
    )
    .expect("write transcript");

    // The registry entry a real "review-zeron-fork" launch would have left —
    // keyed by the chat's own (harness) session id, same as the cursor-based
    // case.
    let registry_dir = fake_home
        .path()
        .join(".config")
        .join("agent-mode")
        .join("session-ids");
    std::fs::create_dir_all(&registry_dir).expect("registry dir");
    std::fs::write(
        registry_dir.join("review-zeron-fork.session-id"),
        format!("{session_id}\n"),
    )
    .expect("write registry entry");

    let data_dir = tempfile::tempdir().expect("data dir");
    let core = assemble(EngineProfile::development(
        data_dir.path(),
        "dev-org",
        "dev-user",
    ));

    let chat_id = "5c2fbb5f-cda0-42d5-9215-7f28ad99e4d9";
    core.workspace
        .create_chat(
            chat_id,
            None,
            Some(&core.device_id),
            None,
            Some("/work/zeron".to_string()),
        )
        .expect("create chat");
    core.workspace
        .set_chat_harness_session(chat_id, session_id, "/work/zeron");

    assert!(
        core.external_import
            .transcript_path_for(chat_id)
            .expect("transcript_path_for")
            .is_none(),
        "this chat must have no import cursor for this test to exercise the fix"
    );
    assert!(
        core.workspace
            .chat(chat_id)
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
        repaired, 1,
        "the cursor-less chat with a harness session must now be resolved"
    );

    let chat_after = core.workspace.chat(chat_id).expect("read").expect("exists");
    assert_eq!(chat_after.title.as_deref(), Some("zeron-fork"));

    // Idempotent, same as the cursor-based path.
    let repaired_again = core
        .external_import
        .repair_missing_titles(&core.context_usage)
        .expect("second repair pass");
    assert_eq!(repaired_again, 0);

    core.shutdown().await;

    // SAFETY: restoring whatever HOME was before this test touched it.
    unsafe {
        match previous_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }
}
