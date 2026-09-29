//! Manual smoke test against a REAL on-disk Claude Code transcript (not the
//! synthetic fixture `external_import.rs` uses) — run explicitly, not part of
//! `cargo test`'s default set, since it depends on a real file on this
//! machine. Exercises `ExternalSessionImporter::import` against messier,
//! real-world data ahead of the picker UI existing to trigger it manually.
//!
//! Run with:
//!   cargo test -p zeron-engine --test external_import_real_smoke -- --ignored --nocapture

use std::sync::Arc;

use zeron_engine::{EngineCore, EngineProfile, HarnessId, default_registry};

async fn import_one(real_path: &std::path::Path, session_id: &str, chat_id: &str) {
    assert!(real_path.is_file(), "expected real transcript at {real_path:?}");

    let dir = tempfile::tempdir().expect("tempdir");
    let core = EngineCore::assemble_with_profile(
        EngineProfile::development(dir.path(), "dev-org", "dev-user"),
        Arc::new(default_registry()),
        HarnessId::Mock,
        None,
    )
    .expect("assemble profile");

    let result = core
        .external_import
        .import(chat_id, session_id, real_path)
        .expect("import real transcript");

    eprintln!("imported: {result:#?}");
    assert!(result.message_count > 0, "expected at least one message");

    let chat = core
        .workspace
        .chat(chat_id)
        .expect("read chat")
        .expect("chat row exists");
    eprintln!("chat row: {chat:#?}");

    core.shutdown().await;
}

#[tokio::test]
#[ignore = "depends on a real ~/.claude/projects transcript on this machine"]
async fn imports_a_real_session_from_this_machine() {
    import_one(
        std::path::Path::new(
            "/Users/mikey/.claude/projects/-Users-mikey-Projects-agent-mode-tools/572aa472-53ef-435f-8596-190f70bbf034.jsonl",
        ),
        "572aa472-53ef-435f-8596-190f70bbf034",
        "real-smoke-chat",
    )
    .await;
}

#[tokio::test]
#[ignore = "depends on a real ~/.claude/projects transcript on this machine"]
async fn imports_a_large_real_session_from_this_machine() {
    import_one(
        std::path::Path::new(
            "/Users/mikey/.claude/projects/-Users-mikey-Projects-cofactr-workspace/258f3c46-1d77-46e8-b4bc-170ecd3dd0bc.jsonl",
        ),
        "258f3c46-1d77-46e8-b4bc-170ecd3dd0bc",
        "real-smoke-chat-large",
    )
    .await;
}
