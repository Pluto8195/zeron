//! `ExternalSessionImporter::repair_missing_titles`: a chat with a stamped
//! harness session but NO findable transcript on disk (moved/deleted, or the
//! `~/.claude/projects/*/*.jsonl` glob just never matches) must be
//! marker-skipped exactly like the "nothing derivable" case — never
//! re-scanned on every boot forever. Kept in its own file since it mutates
//! the process-global `$HOME` env var — same "at most one `$HOME`-mutating
//! test per binary" convention every other file in this crate that does this
//! follows, to avoid a cross-test race within one test binary.

use std::sync::Arc;

use zeron_engine::{EngineCore, EngineProfile, HarnessId, default_registry};

fn assemble(profile: EngineProfile) -> EngineCore {
    EngineCore::assemble_with_profile(profile, Arc::new(default_registry()), HarnessId::Mock, None)
        .expect("assemble profile")
}

#[tokio::test]
async fn repair_marks_a_harness_session_chat_with_no_findable_transcript_and_does_not_loop() {
    let fake_home = tempfile::tempdir().expect("fake home tempdir");
    let previous_home = std::env::var_os("HOME");
    // SAFETY: test-only, scoped to this one function, restored at the end.
    unsafe {
        std::env::set_var("HOME", fake_home.path());
    }
    // Deliberately no `~/.claude/projects` tree at all — the harness-session
    // glob can never match anything.

    let data_dir = tempfile::tempdir().expect("data dir");
    let core = assemble(EngineProfile::development(
        data_dir.path(),
        "dev-org",
        "dev-user",
    ));

    let chat_id = "orphaned-harness-chat";
    let session_id = "sess-vanished";
    core.workspace
        .create_chat(
            chat_id,
            None,
            Some(&core.device_id),
            None,
            Some("/work/gone".to_string()),
        )
        .expect("create chat");
    core.workspace
        .set_chat_harness_session(chat_id, session_id, "/work/gone");

    let repaired = core
        .external_import
        .repair_missing_titles(&core.context_usage)
        .expect("repair pass");
    assert_eq!(repaired, 0, "no transcript anywhere to derive a title from");
    assert!(
        core.workspace
            .chat(chat_id)
            .expect("read")
            .expect("exists")
            .title
            .is_none()
    );

    // Second pass must short-circuit on the marker file rather than
    // re-globbing the filesystem every boot — not directly observable from
    // outside, but at minimum it must stay a stable, repeatable no-op.
    let repaired_again = core
        .external_import
        .repair_missing_titles(&core.context_usage)
        .expect("second repair pass");
    assert_eq!(repaired_again, 0);
    assert!(
        core.workspace
            .chat(chat_id)
            .expect("read")
            .expect("exists")
            .title
            .is_none()
    );

    core.shutdown().await;

    // SAFETY: restoring whatever HOME was before this test touched it.
    unsafe {
        match previous_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }
}
