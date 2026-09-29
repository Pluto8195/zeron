//! `ChatConfig` on imported chats: imports are by definition Claude Code
//! sessions (the scan only reads `~/.claude/projects`), so `import()` must
//! stamp `ChatConfig.harness = ClaudeCode` rather than leaving `config: None`
//! — otherwise `Pickers::effective_harness` (`crates/ui/src/pickers.rs`)
//! falls back to the remembered default/first-offered harness, which can
//! differ from Claude Code and gets sent as `RunRequest.harness`, overriding
//! `DocHost::harness_for_request`'s own chat-config lookup. Also covers
//! `ExternalSessionImporter::repair_missing_config`, the boot-time backfill
//! for chats imported before this fix existed.

use std::sync::Arc;

use zeron_engine::{EngineCore, EngineProfile, HarnessId, default_registry};
use zeron_proto::SandboxLevel;

fn assemble(profile: EngineProfile) -> EngineCore {
    EngineCore::assemble_with_profile(profile, Arc::new(default_registry()), HarnessId::Mock, None)
        .expect("assemble profile")
}

/// Minimal on-disk transcript shaped like a real `~/.claude/projects/
/// <project>/<uuid>.jsonl` file — just enough for `parse_transcript` to
/// resolve a cwd and one message.
fn synthetic_transcript() -> String {
    [
        r#"{"parentUuid":null,"isSidechain":false,"type":"user","message":{"role":"user","content":"hello"},"uuid":"u1","timestamp":"2026-01-01T00:00:01.000Z","cwd":"/work/project","sessionId":"sess-1"}"#,
        r#"{"parentUuid":"u1","isSidechain":false,"type":"assistant","message":{"model":"claude-x","content":[{"type":"text","text":"hi there"}]},"uuid":"asst1","timestamp":"2026-01-01T00:00:02.000Z","cwd":"/work/project","sessionId":"sess-1"}"#,
    ]
    .join("\n")
        + "\n"
}

#[tokio::test]
async fn import_stamps_a_claude_code_config() {
    let dir = tempfile::tempdir().expect("tempdir");
    let core = assemble(EngineProfile::development(dir.path(), "dev-org", "dev-user"));

    let transcript_path = dir.path().join("external-session.jsonl");
    std::fs::write(&transcript_path, synthetic_transcript()).expect("write transcript");

    let chat_id = "imported-chat-with-config";
    core.external_import
        .import(chat_id, "external-session-id-1", &transcript_path)
        .expect("import");

    let chat = core
        .workspace
        .chat(chat_id)
        .expect("read chat")
        .expect("chat row exists");
    let config = chat.config.expect("import must stamp a ChatConfig, not leave it None");
    assert_eq!(config.harness, HarnessId::ClaudeCode);
    // Model/reasoning must stay unset so the user's own picks/sticky
    // defaults still apply — only the harness is forced.
    assert!(config.model.is_none());
    assert!(config.reasoning.is_none());

    core.shutdown().await;
}

#[tokio::test]
async fn repair_backfills_a_config_less_imported_chat() {
    let dir = tempfile::tempdir().expect("tempdir");
    let profile = EngineProfile::development(dir.path(), "dev-org", "dev-user");
    let store_root = profile.store_root().to_path_buf();
    let core = assemble(profile);

    // Reproduce exactly what the old (pre-fix) `import()` used to leave
    // behind: a chat row with a harness session and NO config, plus the
    // on-disk sync-cursor file `import()` has always written (the real
    // signal this repair keys off — see its doc comment for why
    // `harness_session_id` alone isn't safe to use here).
    let chat_id = "pre-fix-imported-chat";
    core.workspace
        .create_chat(chat_id, None, Some(&core.device_id), None, Some("/work/project".into()))
        .expect("create chat row");
    core.workspace
        .set_chat_harness_session(chat_id, "some-external-session-id", "/work/project");
    let cursors_dir = store_root.join("external_import_cursors");
    std::fs::create_dir_all(&cursors_dir).expect("mkdir cursors dir");
    std::fs::write(
        cursors_dir.join(format!("{chat_id}.json")),
        serde_json::json!({
            "transcriptPath": "/home/user/.claude/projects/-work-project/some-external-session-id.jsonl",
            "externalSessionId": "some-external-session-id",
            "linesConsumed": 2,
        })
        .to_string(),
    )
    .expect("write cursor file");

    let before = core.workspace.chat(chat_id).expect("read chat").expect("chat exists");
    assert!(before.config.is_none(), "precondition: simulating the pre-fix broken state");

    let repaired = core
        .external_import
        .repair_missing_config()
        .expect("repair pass");
    assert_eq!(repaired, 1);

    let after = core.workspace.chat(chat_id).expect("read chat").expect("chat exists");
    let config = after.config.expect("repair must backfill a config");
    assert_eq!(config.harness, HarnessId::ClaudeCode);
    assert_eq!(config.sandbox, SandboxLevel::WorkspaceWrite);
    assert!(config.model.is_none());
    assert!(config.reasoning.is_none());

    // Idempotent: a second pass finds nothing left to repair.
    let repaired_again = core
        .external_import
        .repair_missing_config()
        .expect("second repair pass");
    assert_eq!(repaired_again, 0);

    core.shutdown().await;
}

#[tokio::test]
async fn repair_never_touches_a_chat_with_no_harness_session() {
    let dir = tempfile::tempdir().expect("tempdir");
    let core = assemble(EngineProfile::development(dir.path(), "dev-org", "dev-user"));

    // An ordinary Zeron-native chat (no harness_session_id at all) must be
    // left alone by this repair — it's scoped to external-session imports
    // only. Native chats with no config are expected (no run yet); backfilling
    // a harness onto them would be wrong.
    core.workspace
        .create_chat("native-chat", None, Some(&core.device_id), None, None)
        .expect("create chat row");

    let repaired = core
        .external_import
        .repair_missing_config()
        .expect("repair pass");
    assert_eq!(repaired, 0);

    let chat = core.workspace.chat("native-chat").expect("read chat").expect("chat exists");
    assert!(chat.config.is_none());

    core.shutdown().await;
}

#[tokio::test]
async fn repair_never_touches_a_live_chat_with_a_stamped_resume_session_but_no_import_cursor() {
    // Regression for the exact bug this repair's own first draft had:
    // `set_chat_harness_session` is the GENERAL resume-continuity mechanism
    // (`crates/engine/src/workspace_host.rs`) — it fires for any chat's
    // ordinary live run, not just external imports. A live chat that has run
    // once (harness session stamped) but never had `SetChatConfig` called on
    // it — e.g. it was created with `config: None` and dispatched purely via
    // the engine's `default_harness` fallback — must NOT be mistaken for an
    // externally-imported chat and have a Claude Code config forced onto it.
    // The one reliable signal is the import sync-cursor file, which only
    // `ExternalSessionImporter::import`/`sync` ever write.
    let dir = tempfile::tempdir().expect("tempdir");
    let core = assemble(EngineProfile::development(dir.path(), "dev-org", "dev-user"));

    let chat_id = "live-chat-with-resume-session";
    core.workspace
        .create_chat(chat_id, None, Some(&core.device_id), None, Some("/work/project".into()))
        .expect("create chat row");
    core.workspace
        .set_chat_harness_session(chat_id, "some-live-session-id", "/work/project");

    let repaired = core
        .external_import
        .repair_missing_config()
        .expect("repair pass");
    assert_eq!(repaired, 0, "no import cursor exists for this chat — must be left alone");

    let chat = core.workspace.chat(chat_id).expect("read chat").expect("chat exists");
    assert!(chat.config.is_none(), "config must stay None, not be forced to Claude Code");

    core.shutdown().await;
}

#[tokio::test]
async fn repair_never_overwrites_an_existing_config() {
    let dir = tempfile::tempdir().expect("tempdir");
    let core = assemble(EngineProfile::development(dir.path(), "dev-org", "dev-user"));

    let chat_id = "already-configured-chat";
    core.workspace
        .create_chat(chat_id, None, Some(&core.device_id), None, Some("/work/project".into()))
        .expect("create chat row");
    core.workspace
        .set_chat_harness_session(chat_id, "some-external-session-id", "/work/project");
    let custom_config = zeron_proto::ChatConfig {
        harness: HarnessId::Codex,
        model: Some("some-model".into()),
        reasoning: None,
        model_options: Default::default(),
        sandbox: SandboxLevel::ReadOnly,
    };
    core.workspace
        .set_chat_config(chat_id, &custom_config)
        .expect("set config");

    let repaired = core
        .external_import
        .repair_missing_config()
        .expect("repair pass");
    assert_eq!(repaired, 0, "a chat that already has a config must be left untouched");

    let after = core.workspace.chat(chat_id).expect("read chat").expect("chat exists");
    assert_eq!(after.config, Some(custom_config));

    core.shutdown().await;
}
