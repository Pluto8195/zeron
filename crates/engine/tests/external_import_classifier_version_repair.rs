//! `ExternalSessionImporter::reclassify_stale_classifier_version`: chats whose
//! cursor was stamped by an older `CLASSIFIER_VERSION` (or never stamped) get
//! `classify_heuristic` re-run on boot; same-version chats are untouched, and
//! the pass is idempotent. There is no manual-recategorize path, so there is
//! no manual-override case to test (see the pass's doc comment).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use zeron_engine::external_import::CLASSIFIER_VERSION;
use zeron_engine::{EngineCore, EngineProfile, HarnessId, default_registry};

fn assemble(profile: EngineProfile) -> EngineCore {
    EngineCore::assemble_with_profile(profile, Arc::new(default_registry()), HarnessId::Mock, None)
        .expect("assemble profile")
}

/// A Sentry-flavored investigation: four turns of read-only tool calls, no
/// edits. The pre-debug heuristic filed this under `research`; the current
/// one reads the opener and says `debug`.
fn sentry_transcript() -> String {
    let mut lines = vec![
        r#"{"parentUuid":null,"isSidechain":false,"type":"user","message":{"role":"user","content":"sentry is showing a crash in the health endpoint, find the root cause"},"uuid":"u1","timestamp":"2026-01-01T00:00:01.000Z","cwd":"/work/project","sessionId":"sess-1","entrypoint":"cli"}"#.to_string(),
    ];
    for i in 1..=3 {
        lines.push(format!(
            r#"{{"parentUuid":"u{i}","isSidechain":false,"type":"assistant","message":{{"model":"claude-x","content":[{{"type":"tool_use","id":"t{i}","name":"Read","input":{{"file_path":"/work/project/f{i}.rs"}}}}]}},"uuid":"a{i}","timestamp":"2026-01-01T00:00:0{}.000Z","cwd":"/work/project","sessionId":"sess-1"}}"#,
            i + 1
        ));
        lines.push(format!(
            r#"{{"parentUuid":"a{i}","isSidechain":false,"type":"user","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t{i}","is_error":false,"content":"ok"}}]}},"uuid":"u{}","timestamp":"2026-01-01T00:00:0{}.500Z","cwd":"/work/project","sessionId":"sess-1"}}"#,
            i + 1,
            i + 1
        ));
    }
    lines.join("\n") + "\n"
}

struct Fixture {
    _dir: tempfile::TempDir,
    core: EngineCore,
    cursors_dir: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let profile = EngineProfile::development(dir.path(), "dev-org", "dev-user");
        let cursors_dir = profile.store_root().join("external_import_cursors");
        let core = assemble(profile);
        Self { _dir: dir, core, cursors_dir }
    }

    fn import(&self, chat_id: &str, session: &str) {
        let path = self._dir.path().join(format!("{session}.jsonl"));
        std::fs::write(&path, sentry_transcript()).expect("write transcript");
        self.core.external_import.import(chat_id, session, &path).expect("import");
    }

    fn cursor(&self, chat_id: &str) -> serde_json::Value {
        let raw = std::fs::read_to_string(self.cursor_path(chat_id)).expect("read cursor");
        serde_json::from_str(&raw).expect("parse cursor")
    }

    fn cursor_path(&self, chat_id: &str) -> PathBuf {
        self.cursors_dir.join(format!("{chat_id}.json"))
    }

    fn edit_cursor(&self, chat_id: &str, f: impl FnOnce(&mut serde_json::Value)) {
        let mut json = self.cursor(chat_id);
        f(&mut json);
        std::fs::write(self.cursor_path(chat_id), serde_json::to_vec(&json).unwrap()).expect("rewrite cursor");
    }

    /// Make the cursor look like a pre-versioning one that filed this chat
    /// under `research`.
    fn make_stale(&self, chat_id: &str) {
        self.edit_cursor(chat_id, |j| {
            j["category"] = "research".into();
            j.as_object_mut().unwrap().remove("classifierVersion");
        });
    }

    fn category(&self, chat_id: &str) -> String {
        self.core
            .external_import
            .classification_for(chat_id)
            .expect("read classification")
            .expect("classification present")
            .0
    }
}

#[tokio::test]
async fn import_stamps_the_current_classifier_version_and_classifies_debug() {
    let fx = Fixture::new();
    fx.import("chat-a", "sess-a");
    assert_eq!(fx.cursor("chat-a")["classifierVersion"], CLASSIFIER_VERSION);
    assert_eq!(fx.category("chat-a"), "debug");
    fx.core.shutdown().await;
}

#[tokio::test]
async fn stale_stamped_chat_is_reclassified_and_restamped() {
    let fx = Fixture::new();
    fx.import("chat-a", "sess-a");
    fx.make_stale("chat-a");
    assert_eq!(fx.category("chat-a"), "research");

    let (recategorized, restamped) = fx.core.external_import.reclassify_stale_classifier_version().expect("pass");
    assert_eq!((recategorized, restamped), (1, 0));
    assert_eq!(fx.category("chat-a"), "debug");
    assert_eq!(fx.cursor("chat-a")["classifierVersion"], CLASSIFIER_VERSION);

    // Other cursor state is preserved.
    assert_eq!(fx.cursor("chat-a")["externalSessionId"], "sess-a");
    fx.core.shutdown().await;
}

#[tokio::test]
async fn older_numeric_stamp_is_also_stale() {
    let fx = Fixture::new();
    fx.import("chat-a", "sess-a");
    fx.edit_cursor("chat-a", |j| {
        j["category"] = "research".into();
        j["classifierVersion"] = 1.into();
    });
    let (recategorized, _) = fx.core.external_import.reclassify_stale_classifier_version().expect("pass");
    assert_eq!(recategorized, 1);
    assert_eq!(fx.category("chat-a"), "debug");
    fx.core.shutdown().await;
}

#[tokio::test]
async fn unchanged_category_only_updates_the_stamp() {
    let fx = Fixture::new();
    fx.import("chat-a", "sess-a");
    // Correct category, but unstamped.
    fx.edit_cursor("chat-a", |j| {
        j.as_object_mut().unwrap().remove("classifierVersion");
    });
    let (recategorized, restamped) = fx.core.external_import.reclassify_stale_classifier_version().expect("pass");
    assert_eq!((recategorized, restamped), (0, 1));
    assert_eq!(fx.category("chat-a"), "debug");
    assert_eq!(fx.cursor("chat-a")["classifierVersion"], CLASSIFIER_VERSION);
    fx.core.shutdown().await;
}

#[tokio::test]
async fn same_version_chat_is_untouched() {
    let fx = Fixture::new();
    fx.import("chat-a", "sess-a");
    // Deliberately "wrong" category under the current stamp: must not be recomputed.
    fx.edit_cursor("chat-a", |j| j["category"] = "research".into());
    let (recategorized, restamped) = fx.core.external_import.reclassify_stale_classifier_version().expect("pass");
    assert_eq!((recategorized, restamped), (0, 0));
    assert_eq!(fx.category("chat-a"), "research");
    fx.core.shutdown().await;
}

#[tokio::test]
async fn pass_is_idempotent() {
    let fx = Fixture::new();
    fx.import("chat-a", "sess-a");
    fx.make_stale("chat-a");
    let first = fx.core.external_import.reclassify_stale_classifier_version().expect("first");
    assert_eq!(first, (1, 0));
    let bytes_after_first = std::fs::read(fx.cursor_path("chat-a")).unwrap();
    let second = fx.core.external_import.reclassify_stale_classifier_version().expect("second");
    assert_eq!(second, (0, 0));
    assert_eq!(std::fs::read(fx.cursor_path("chat-a")).unwrap(), bytes_after_first);
    fx.core.shutdown().await;
}

#[tokio::test]
async fn missing_transcript_is_skipped_without_stamping() {
    let fx = Fixture::new();
    fx.import("chat-a", "sess-a");
    fx.make_stale("chat-a");
    std::fs::remove_file(Path::new(fx.cursor("chat-a")["transcriptPath"].as_str().unwrap())).unwrap();
    let result = fx.core.external_import.reclassify_stale_classifier_version().expect("pass");
    assert_eq!(result, (0, 0));
    assert_eq!(fx.category("chat-a"), "research");
    assert!(fx.cursor("chat-a").get("classifierVersion").is_none_or(|v| v.is_null()));
    fx.core.shutdown().await;
}

#[tokio::test]
async fn empty_category_backfill_still_works_and_stamps() {
    let fx = Fixture::new();
    fx.import("chat-a", "sess-a");
    fx.edit_cursor("chat-a", |j| {
        j["category"] = "".into();
        j["origin"] = "".into();
        j.as_object_mut().unwrap().remove("classifierVersion");
    });

    // The version pass leaves an empty-category chat to the backfill...
    assert_eq!(fx.core.external_import.reclassify_stale_classifier_version().unwrap(), (0, 0));
    // ...which classifies it with the current heuristic and stamps it.
    assert_eq!(fx.core.external_import.repair_missing_classification().unwrap(), 1);
    assert_eq!(fx.category("chat-a"), "debug");
    assert_eq!(fx.cursor("chat-a")["classifierVersion"], CLASSIFIER_VERSION);
    assert_eq!(fx.core.external_import.repair_missing_classification().unwrap(), 0);
    assert_eq!(fx.core.external_import.reclassify_stale_classifier_version().unwrap(), (0, 0));
    fx.core.shutdown().await;
}
