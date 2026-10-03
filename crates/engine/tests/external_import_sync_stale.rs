//! `ExternalSessionImporter::sync_stale_imports`: the boot pass that keeps
//! imported chats current using a stat-only staleness check.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use zeron_engine::{EngineCore, EngineProfile, HarnessId, default_registry};

fn assemble(profile: EngineProfile) -> EngineCore {
    EngineCore::assemble_with_profile(profile, Arc::new(default_registry()), HarnessId::Mock, None)
        .expect("assemble profile")
}

fn user_line(uuid: &str, parent: &str, ts: &str, text: &str) -> String {
    format!(
        r#"{{"parentUuid":"{parent}","isSidechain":false,"type":"user","message":{{"role":"user","content":"{text}"}},"uuid":"{uuid}","timestamp":"{ts}","cwd":"/work/project","sessionId":"sess-1"}}"#
    )
}

fn assistant_line(uuid: &str, parent: &str, ts: &str, text: &str) -> String {
    format!(
        r#"{{"parentUuid":"{parent}","isSidechain":false,"type":"assistant","message":{{"model":"claude-x","content":[{{"type":"text","text":"{text}"}}]}},"uuid":"{uuid}","timestamp":"{ts}","cwd":"/work/project","sessionId":"sess-1"}}"#
    )
}

fn initial() -> String {
    [
        user_line("u1", "null", "2026-01-01T00:00:01.000Z", "hello"),
        assistant_line("a1", "u1", "2026-01-01T00:00:02.000Z", "hi"),
    ]
    .join("\n")
        + "\n"
}

/// One new human message: closes a1's turn, so exactly u2 is safe.
fn growth() -> String {
    user_line("u2", "a1", "2026-01-01T00:00:03.000Z", "more please") + "\n"
}

fn growth_with_assistant() -> String {
    [
        user_line("u2", "a1", "2026-01-01T00:00:03.000Z", "more please"),
        assistant_line("a2", "u2", "2026-01-01T00:00:04.000Z", "final answer"),
    ]
    .join("\n")
        + "\n"
}

fn cursor_path(root: &Path, chat: &str) -> PathBuf {
    root.join("external_import_cursors")
        .join(format!("{chat}.json"))
}

fn read_cursor(root: &Path, chat: &str) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(cursor_path(root, chat)).unwrap()).unwrap()
}

fn write_cursor(root: &Path, chat: &str, v: &serde_json::Value) {
    std::fs::write(cursor_path(root, chat), serde_json::to_vec(v).unwrap()).unwrap();
}

fn message_count(root: &Path, chat: &str) -> usize {
    let store = zeron_sync::DocsStore::open(root).unwrap();
    let bytes = store.load_snapshot(chat).unwrap().unwrap();
    let loro = loro::LoroDoc::new();
    loro.import(&bytes).unwrap();
    zeron_doc::SessionDoc::from_doc(loro)
        .read_entries()
        .unwrap()
        .len()
}

#[tokio::test]
async fn grown_file_syncs_untouched_file_is_skipped_and_second_pass_is_noop() {
    let dir = tempfile::tempdir().unwrap();
    let profile = EngineProfile::development(dir.path(), "dev-org", "dev-user");
    let root = profile.store_root().to_path_buf();
    let core = assemble(profile);
    let path = dir.path().join("s.jsonl");
    std::fs::write(&path, initial()).unwrap();
    core.external_import.import("c1", "sess-1", &path).unwrap();

    let c = read_cursor(&root, "c1");
    assert!(c["lastSyncedLen"].is_u64() && c["lastSyncedMtime"].is_i64());

    // Untouched: skipped. Prove zero reads by rewinding lines_consumed — a real
    // sync would re-push messages; the doc must stay at 2 entries.
    let mut rewound = c.clone();
    rewound["linesConsumed"] = 0.into();
    write_cursor(&root, "c1", &rewound);
    assert_eq!(core.external_import.sync_stale_imports().unwrap(), (0, 0));
    assert_eq!(message_count(&root, "c1"), 2);
    write_cursor(&root, "c1", &c);

    // Grown: syncs.
    let mut f = std::fs::read_to_string(&path).unwrap();
    f.push_str(&growth());
    std::fs::write(&path, f).unwrap();
    assert_eq!(core.external_import.sync_stale_imports().unwrap(), (1, 1));
    assert_eq!(message_count(&root, "c1"), 3);

    // Idempotent.
    assert_eq!(core.external_import.sync_stale_imports().unwrap(), (0, 0));
    assert_eq!(message_count(&root, "c1"), 3);
    core.shutdown().await;
}

#[tokio::test]
async fn legacy_cursor_without_stamp_fields_syncs_once() {
    let dir = tempfile::tempdir().unwrap();
    let profile = EngineProfile::development(dir.path(), "dev-org", "dev-user");
    let root = profile.store_root().to_path_buf();
    let core = assemble(profile);
    let path = dir.path().join("s.jsonl");
    std::fs::write(&path, initial()).unwrap();
    core.external_import.import("c2", "sess-1", &path).unwrap();
    let mut f = std::fs::read_to_string(&path).unwrap();
    f.push_str(&growth());
    std::fs::write(&path, f).unwrap();

    let mut legacy = read_cursor(&root, "c2");
    let obj = legacy.as_object_mut().unwrap();
    obj.remove("lastSyncedMtime");
    obj.remove("lastSyncedLen");
    write_cursor(&root, "c2", &legacy);

    assert_eq!(core.external_import.sync_stale_imports().unwrap(), (1, 1));
    assert_eq!(message_count(&root, "c2"), 3);
    assert!(read_cursor(&root, "c2")["lastSyncedLen"].is_u64());
    assert_eq!(core.external_import.sync_stale_imports().unwrap(), (0, 0));
    core.shutdown().await;
}

#[tokio::test]
async fn unchanged_deferred_assistant_tail_is_finalized_once_during_boot_pass() {
    let dir = tempfile::tempdir().unwrap();
    let profile = EngineProfile::development(dir.path(), "dev-org", "dev-user");
    let root = profile.store_root().to_path_buf();
    let core = assemble(profile);
    let path = dir.path().join("s.jsonl");
    std::fs::write(&path, initial()).unwrap();
    core.external_import
        .import("c-tail", "sess-1", &path)
        .unwrap();

    let mut f = std::fs::read_to_string(&path).unwrap();
    f.push_str(&growth_with_assistant());
    std::fs::write(&path, f).unwrap();

    // The human message closes the preceding turn, but the new assistant at
    // EOF is deliberately deferred on its first observation.
    // The boot pass makes both observations: the first carries the human
    // message and defers the EOF assistant; the settling retry sees the same
    // stamp and finalizes the assistant before journal fallback can race it.
    assert_eq!(core.external_import.sync_stale_imports().unwrap(), (1, 2));
    assert_eq!(message_count(&root, "c-tail"), 4);
    assert_eq!(read_cursor(&root, "c-tail")["pendingAssistantTail"], false);

    // The marker is cleared, so further passes remain idempotent and cannot
    // duplicate the assistant message.
    assert_eq!(core.external_import.sync_stale_imports().unwrap(), (0, 0));
    assert_eq!(message_count(&root, "c-tail"), 4);
    core.shutdown().await;
}

#[tokio::test]
async fn pre_fix_cursor_with_matching_stamp_recovers_stranded_tail() {
    let dir = tempfile::tempdir().unwrap();
    let profile = EngineProfile::development(dir.path(), "dev-org", "dev-user");
    let root = profile.store_root().to_path_buf();
    let core = assemble(profile);
    let path = dir.path().join("s.jsonl");
    std::fs::write(&path, initial()).unwrap();
    core.external_import
        .import("c-legacy-tail", "sess-1", &path)
        .unwrap();

    let mut f = std::fs::read_to_string(&path).unwrap();
    f.push_str(&growth_with_assistant());
    std::fs::write(&path, f).unwrap();
    assert_eq!(
        core.external_import
            .sync("c-legacy-tail")
            .unwrap()
            .new_message_count,
        1
    );
    assert_eq!(message_count(&root, "c-legacy-tail"), 3);

    // Recreate the cursor shape written by the buggy implementation: it
    // advanced only through the human line, stamped the full file, and had
    // no pending-tail marker. The next boot must parse instead of stat-skip.
    let mut legacy = read_cursor(&root, "c-legacy-tail");
    legacy
        .as_object_mut()
        .unwrap()
        .remove("pendingAssistantTail");
    write_cursor(&root, "c-legacy-tail", &legacy);

    assert_eq!(core.external_import.sync_stale_imports().unwrap(), (1, 1));
    assert_eq!(message_count(&root, "c-legacy-tail"), 4);
    assert_eq!(
        read_cursor(&root, "c-legacy-tail")["pendingAssistantTail"],
        false
    );
    assert_eq!(core.external_import.sync_stale_imports().unwrap(), (0, 0));
    assert_eq!(message_count(&root, "c-legacy-tail"), 4);
    core.shutdown().await;
}

#[tokio::test]
async fn growing_deferred_assistant_tail_waits_for_a_stable_observation() {
    let dir = tempfile::tempdir().unwrap();
    let profile = EngineProfile::development(dir.path(), "dev-org", "dev-user");
    let root = profile.store_root().to_path_buf();
    let core = assemble(profile);
    let path = dir.path().join("s.jsonl");
    std::fs::write(&path, initial()).unwrap();
    core.external_import
        .import("c-growing", "sess-1", &path)
        .unwrap();

    let mut f = std::fs::read_to_string(&path).unwrap();
    f.push_str(&growth_with_assistant());
    std::fs::write(&path, f).unwrap();
    assert_eq!(
        core.external_import
            .sync("c-growing")
            .unwrap()
            .new_message_count,
        1
    );
    assert_eq!(message_count(&root, "c-growing"), 3);

    // Another chunk arriving invalidates the first observation. This pass
    // records the new stamp but still does not publish a partial assistant.
    let continuation = assistant_line(
        "a2",
        "u2",
        "2026-01-01T00:00:05.000Z",
        " with a later chunk",
    ) + "\n";
    let mut f = std::fs::read_to_string(&path).unwrap();
    f.push_str(&continuation);
    std::fs::write(&path, f).unwrap();
    assert_eq!(
        core.external_import
            .sync("c-growing")
            .unwrap()
            .new_message_count,
        0
    );
    assert_eq!(message_count(&root, "c-growing"), 3);

    assert_eq!(core.external_import.sync_stale_imports().unwrap(), (1, 1));
    assert_eq!(message_count(&root, "c-growing"), 4);
    assert_eq!(core.external_import.sync_stale_imports().unwrap(), (0, 0));
    assert_eq!(message_count(&root, "c-growing"), 4);
    core.shutdown().await;
}

#[tokio::test]
async fn missing_transcript_is_skipped_silently() {
    let dir = tempfile::tempdir().unwrap();
    let profile = EngineProfile::development(dir.path(), "dev-org", "dev-user");
    let root = profile.store_root().to_path_buf();
    let core = assemble(profile);
    let path = dir.path().join("s.jsonl");
    std::fs::write(&path, initial()).unwrap();
    core.external_import.import("c3", "sess-1", &path).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(core.external_import.sync_stale_imports().unwrap(), (0, 0));
    assert_eq!(message_count(&root, "c3"), 2);
    core.shutdown().await;
}
