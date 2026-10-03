//! `ExternalSessionImporter::repair_missing_timestamps`: chats imported
//! before `import()`/`sync()` learned to stamp `last_message_at` must get it
//! backfilled from their already-persisted doc, so `Chat::unseen()` (and thus
//! their status color) works correctly without requiring a re-import.

use std::sync::Arc;

use zeron_doc::{MessagePart, MessageRole, SessionDoc, SessionMessageEntry};
use zeron_engine::{EngineCore, EngineProfile, HarnessId, default_registry};

fn assemble(profile: EngineProfile) -> EngineCore {
    EngineCore::assemble_with_profile(profile, Arc::new(default_registry()), HarnessId::Mock, None)
        .expect("assemble profile")
}

#[tokio::test]
async fn repairs_a_chat_left_over_by_the_old_pre_fix_import_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let profile = EngineProfile::development(dir.path(), "dev-org", "dev-user");
    let store_root = profile.store_root().to_path_buf();
    let core = assemble(profile);

    // Reproduce exactly what the old (pre-fix) `import()` used to leave
    // behind: a chat row + doc snapshot + harness session, but no
    // `note_message` call, so `last_message_at` stays `None`.
    let chat_id = "pre-fix-imported-chat";
    let doc = SessionDoc::init(chat_id).expect("init doc");
    doc.push_message(&SessionMessageEntry {
        id: "m1".into(),
        role: MessageRole::User,
        parts: vec![MessagePart::Text {
            id: "t1".into(),
            text: "hello from before the fix".into(),
        }],
        created_at: 1_700_000_000_000,
        device_id: core.device_id.clone(),
        status: None,
        continuation_of: None,
    })
    .expect("push message");
    let bytes = doc.export_snapshot().expect("snapshot");
    let store = zeron_sync::DocsStore::open(&store_root).expect("open store");
    store
        .save_snapshot_with_cursor(chat_id, &bytes, 0, 2)
        .expect("save doc");
    drop(store);

    core.workspace
        .create_chat(
            chat_id,
            None,
            Some(&core.device_id),
            None,
            Some("/work/project".into()),
        )
        .expect("create chat row");
    core.workspace
        .set_chat_harness_session(chat_id, "some-external-session-id", "/work/project");

    let before = core
        .workspace
        .chat(chat_id)
        .expect("read chat")
        .expect("chat exists");
    assert!(
        before.last_message_at.is_none(),
        "precondition: simulating the pre-fix broken state"
    );

    let repaired = core
        .external_import
        .repair_missing_timestamps()
        .expect("repair pass");
    assert_eq!(repaired, 1);

    let after = core
        .workspace
        .chat(chat_id)
        .expect("read chat")
        .expect("chat exists");
    assert_eq!(
        after.last_message_at.expect("stamped").timestamp_millis(),
        1_700_000_000_000,
        "must use the entry's real historical timestamp, not the repair's own run time \
         (a 'now' stamp would make every imported chat look freshly active and defeat \
         staleness detection entirely)"
    );
    assert_eq!(
        after.last_message_preview.as_deref(),
        Some("hello from before the fix")
    );

    // Idempotent: a second pass finds nothing left to repair.
    let repaired_again = core
        .external_import
        .repair_missing_timestamps()
        .expect("second repair pass");
    assert_eq!(repaired_again, 0);

    core.shutdown().await;
}

#[tokio::test]
async fn a_chat_with_no_harness_session_is_never_touched() {
    let dir = tempfile::tempdir().expect("tempdir");
    let core = assemble(EngineProfile::development(
        dir.path(),
        "dev-org",
        "dev-user",
    ));

    // An ordinary Zeron-native chat (no harness_session_id at all) must be
    // left alone by this repair — it's scoped to external-session imports only.
    core.workspace
        .create_chat("native-chat", None, Some(&core.device_id), None, None)
        .expect("create chat row");

    let repaired = core
        .external_import
        .repair_missing_timestamps()
        .expect("repair pass");
    assert_eq!(repaired, 0);

    let chat = core
        .workspace
        .chat("native-chat")
        .expect("read chat")
        .expect("chat exists");
    assert!(chat.last_message_at.is_none());

    core.shutdown().await;
}
