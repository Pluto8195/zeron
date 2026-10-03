//! `ExternalSessionImporter::reclassify_stale_classifier_version`: chats whose
//! cursor was stamped by an older `CLASSIFIER_VERSION` (or never stamped) get
//! `classify_heuristic` re-run on boot; same-version chats are untouched, and
//! the pass is idempotent. There is no manual-recategorize path, so there is
//! no manual-override case to test (see the pass's doc comment).
//!
//! The Jev (TypeSafe) half: a canned [`MockJev`] stands in for the classifier
//! — these tests never touch the network or the real `TYPESAFE_API_KEY`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use zeron_engine::external_import::{
    CLASSIFIER_VERSION, ExternalSessionImporter, JEV_RECLASSIFY_MAX_PER_PASS,
};
use zeron_engine::typesafe::{ChatCategoryClassifier, ChatClassificationInput, JevOutcome};
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
        Self {
            _dir: dir,
            core,
            cursors_dir,
        }
    }

    fn import(&self, chat_id: &str, session: &str) {
        let path = self._dir.path().join(format!("{session}.jsonl"));
        std::fs::write(&path, sentry_transcript()).expect("write transcript");
        self.core
            .external_import
            .import(chat_id, session, &path)
            .expect("import");
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
        std::fs::write(
            self.cursor_path(chat_id),
            serde_json::to_vec(&json).unwrap(),
        )
        .expect("rewrite cursor");
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

    let (recategorized, restamped) = fx
        .core
        .external_import
        .reclassify_stale_classifier_version()
        .expect("pass");
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
    let (recategorized, _) = fx
        .core
        .external_import
        .reclassify_stale_classifier_version()
        .expect("pass");
    assert_eq!(recategorized, 1);
    assert_eq!(fx.category("chat-a"), "debug");
    fx.core.shutdown().await;
}

#[tokio::test]
async fn previous_version_stamp_is_reclassified_after_classifier_change() {
    assert_eq!(
        CLASSIFIER_VERSION, 7,
        "update this fixture for the next bump"
    );
    let fx = Fixture::new();
    fx.import("chat-a", "sess-a");
    fx.edit_cursor("chat-a", |j| {
        j["category"] = "research".into();
        j["classifierVersion"] = 6.into();
    });

    let (recategorized, restamped) = fx
        .core
        .external_import
        .reclassify_stale_classifier_version()
        .expect("pass");
    assert_eq!((recategorized, restamped), (1, 0));
    assert_eq!(fx.category("chat-a"), "debug");
    assert_eq!(fx.cursor("chat-a")["classifierVersion"], 7);
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
    let (recategorized, restamped) = fx
        .core
        .external_import
        .reclassify_stale_classifier_version()
        .expect("pass");
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
    let (recategorized, restamped) = fx
        .core
        .external_import
        .reclassify_stale_classifier_version()
        .expect("pass");
    assert_eq!((recategorized, restamped), (0, 0));
    assert_eq!(fx.category("chat-a"), "research");
    fx.core.shutdown().await;
}

#[tokio::test]
async fn pass_is_idempotent() {
    let fx = Fixture::new();
    fx.import("chat-a", "sess-a");
    fx.make_stale("chat-a");
    let first = fx
        .core
        .external_import
        .reclassify_stale_classifier_version()
        .expect("first");
    assert_eq!(first, (1, 0));
    let bytes_after_first = std::fs::read(fx.cursor_path("chat-a")).unwrap();
    let second = fx
        .core
        .external_import
        .reclassify_stale_classifier_version()
        .expect("second");
    assert_eq!(second, (0, 0));
    assert_eq!(
        std::fs::read(fx.cursor_path("chat-a")).unwrap(),
        bytes_after_first
    );
    fx.core.shutdown().await;
}

#[tokio::test]
async fn missing_transcript_is_skipped_without_stamping() {
    let fx = Fixture::new();
    fx.import("chat-a", "sess-a");
    fx.make_stale("chat-a");
    std::fs::remove_file(Path::new(
        fx.cursor("chat-a")["transcriptPath"].as_str().unwrap(),
    ))
    .unwrap();
    let result = fx
        .core
        .external_import
        .reclassify_stale_classifier_version()
        .expect("pass");
    assert_eq!(result, (0, 0));
    assert_eq!(fx.category("chat-a"), "research");
    assert!(
        fx.cursor("chat-a")
            .get("classifierVersion")
            .is_none_or(|v| v.is_null())
    );
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
    assert_eq!(
        fx.core
            .external_import
            .reclassify_stale_classifier_version()
            .unwrap(),
        (0, 0)
    );
    // ...which classifies it with the current heuristic and stamps it.
    assert_eq!(
        fx.core
            .external_import
            .repair_missing_classification()
            .unwrap(),
        1
    );
    assert_eq!(fx.category("chat-a"), "debug");
    assert_eq!(fx.cursor("chat-a")["classifierVersion"], CLASSIFIER_VERSION);
    assert_eq!(
        fx.core
            .external_import
            .repair_missing_classification()
            .unwrap(),
        0
    );
    assert_eq!(
        fx.core
            .external_import
            .reclassify_stale_classifier_version()
            .unwrap(),
        (0, 0)
    );
    fx.core.shutdown().await;
}

// ── Jev classification ──────────────────────────────────────────────────────

/// Canned classifier: fixed outcome, switchable availability ("is the key
/// present"), and a call counter.
struct MockJev {
    available: AtomicBool,
    outcome: Mutex<JevOutcome>,
    calls: AtomicUsize,
    inputs: Mutex<Vec<ChatClassificationInput>>,
}

impl MockJev {
    fn new(available: bool, outcome: JevOutcome) -> Arc<Self> {
        Arc::new(Self {
            available: AtomicBool::new(available),
            outcome: Mutex::new(outcome),
            calls: AtomicUsize::new(0),
            inputs: Mutex::new(Vec::new()),
        })
    }

    fn says(category: &str) -> Arc<Self> {
        Self::new(true, JevOutcome::Category(category.to_string()))
    }

    fn keyless() -> Arc<Self> {
        Self::new(false, JevOutcome::Category("planning".into()))
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl ChatCategoryClassifier for MockJev {
    fn available(&self) -> bool {
        self.available.load(Ordering::SeqCst)
    }
    fn classify(&self, input: &ChatClassificationInput) -> JevOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inputs.lock().unwrap().push(input.clone());
        self.outcome.lock().unwrap().clone()
    }
}

impl Fixture {
    fn importer(&self, jev: &Arc<MockJev>) -> ExternalSessionImporter {
        self.core
            .external_import
            .clone()
            .with_chat_category_classifier(jev.clone())
    }

    fn import_via(&self, importer: &ExternalSessionImporter, chat_id: &str, session: &str) {
        let path = self._dir.path().join(format!("{session}.jsonl"));
        std::fs::write(&path, sentry_transcript()).expect("write transcript");
        importer.import(chat_id, session, &path).expect("import");
    }

    fn source(&self, chat_id: &str) -> String {
        self.cursor(chat_id)["classifierSource"]
            .as_str()
            .unwrap_or("<absent>")
            .to_string()
    }

    /// Import `n` chats heuristically (no Jev), i.e. stamped current-version
    /// + `heuristic`; they are stale only by the key-present rule.
    fn heuristic_chats(&self, n: usize) -> Vec<String> {
        (0..n)
            .map(|i| {
                let id = format!("chat-{i:02}");
                self.import(&id, &format!("sess-{i:02}"));
                assert_eq!(self.source(&id), "heuristic");
                id
            })
            .collect()
    }

    fn count_with_source(&self, ids: &[String], source: &str) -> usize {
        ids.iter().filter(|id| self.source(id) == source).count()
    }
}

#[tokio::test]
async fn import_tries_jev_first_and_stamps_source_jev() {
    let fx = Fixture::new();
    let jev = MockJev::says("planning");
    fx.import_via(&fx.importer(&jev), "chat-a", "sess-a");
    assert_eq!(jev.calls(), 1);
    assert_eq!(fx.category("chat-a"), "planning");
    assert_eq!(fx.source("chat-a"), "jev");
    assert_eq!(fx.cursor("chat-a")["classifierVersion"], CLASSIFIER_VERSION);
    // Jev was given the same inputs the heuristic saw.
    let input = jev.inputs.lock().unwrap()[0].clone();
    assert!(input.first_messages[0].contains("sentry is showing a crash"));
    assert_eq!(input.tool_counts.get("Read"), Some(&3));
    assert_eq!(input.turn_count, 4); // 1 human message + 3 assistant turns
    fx.core.shutdown().await;
}

#[tokio::test]
async fn import_falls_back_to_the_heuristic_when_jev_fails() {
    let fx = Fixture::new();
    let jev = MockJev::new(true, JevOutcome::Failed);
    fx.import_via(&fx.importer(&jev), "chat-a", "sess-a");
    assert_eq!(jev.calls(), 1);
    assert_eq!(fx.category("chat-a"), "debug");
    assert_eq!(fx.source("chat-a"), "heuristic");
    assert!(
        fx.cursor("chat-a")
            .get("jevInconclusiveVersion")
            .is_none_or(|v| v.is_null())
    );
    fx.core.shutdown().await;
}

#[tokio::test]
async fn import_with_inconclusive_jev_falls_back_and_remembers() {
    let fx = Fixture::new();
    let jev = MockJev::new(true, JevOutcome::Inconclusive);
    fx.import_via(&fx.importer(&jev), "chat-a", "sess-a");
    assert_eq!(fx.category("chat-a"), "debug");
    assert_eq!(fx.source("chat-a"), "heuristic");
    assert_eq!(
        fx.cursor("chat-a")["jevInconclusiveVersion"],
        CLASSIFIER_VERSION
    );
    fx.core.shutdown().await;
}

#[tokio::test]
async fn jev_other_does_not_override_a_specific_heuristic_category() {
    let fx = Fixture::new();
    let jev = MockJev::says("other");
    fx.import_via(&fx.importer(&jev), "chat-a", "sess-a");
    assert_eq!(jev.calls(), 1);
    assert_eq!(fx.category("chat-a"), "debug");
    assert_eq!(fx.source("chat-a"), "heuristic");
    assert_eq!(
        fx.cursor("chat-a")["jevInconclusiveVersion"],
        CLASSIFIER_VERSION
    );
    fx.core.shutdown().await;
}

#[tokio::test]
async fn import_without_a_key_never_calls_jev() {
    let fx = Fixture::new();
    let jev = MockJev::keyless();
    fx.import_via(&fx.importer(&jev), "chat-a", "sess-a");
    assert_eq!(jev.calls(), 0);
    assert_eq!(fx.category("chat-a"), "debug");
    assert_eq!(fx.source("chat-a"), "heuristic");
    fx.core.shutdown().await;
}

#[tokio::test]
async fn heuristic_stamped_chat_with_key_present_is_stale_and_upgrades() {
    let fx = Fixture::new();
    fx.import("chat-a", "sess-a");
    assert_eq!(
        (fx.category("chat-a").as_str(), fx.source("chat-a").as_str()),
        ("debug", "heuristic")
    );

    // No key: same stamp version, heuristic source -> not stale, nothing touched.
    let keyless = MockJev::keyless();
    let before = std::fs::read(fx.cursor_path("chat-a")).unwrap();
    assert_eq!(
        fx.importer(&keyless)
            .reclassify_stale_classifier_version()
            .unwrap(),
        (0, 0)
    );
    assert_eq!(std::fs::read(fx.cursor_path("chat-a")).unwrap(), before);
    assert_eq!(keyless.calls(), 0);

    // Key present: stale by the source rule, upgraded to Jev's answer.
    let jev = MockJev::says("research");
    let report = fx
        .importer(&jev)
        .reclassify_stale_classifier_version_report()
        .unwrap();
    assert_eq!(
        (
            report.recategorized,
            report.restamped,
            report.jev_classified
        ),
        (1, 0, 1)
    );
    assert_eq!(jev.calls(), 1);
    assert_eq!(fx.category("chat-a"), "research");
    assert_eq!(fx.source("chat-a"), "jev");

    // Now jev-stamped: a further key-present pass is a no-op.
    assert_eq!(
        fx.importer(&jev)
            .reclassify_stale_classifier_version()
            .unwrap(),
        (0, 0)
    );
    assert_eq!(jev.calls(), 1);
    fx.core.shutdown().await;
}

#[tokio::test]
async fn upgrade_with_the_same_category_only_restamps_the_source() {
    let fx = Fixture::new();
    fx.import("chat-a", "sess-a");
    let jev = MockJev::says("debug");
    let report = fx
        .importer(&jev)
        .reclassify_stale_classifier_version_report()
        .unwrap();
    assert_eq!(
        (
            report.recategorized,
            report.restamped,
            report.jev_classified
        ),
        (0, 1, 1)
    );
    assert_eq!(fx.source("chat-a"), "jev");
    fx.core.shutdown().await;
}

#[tokio::test]
async fn keyless_boot_does_not_churn_jev_stamped_chats() {
    let fx = Fixture::new();
    let jev = MockJev::says("planning");
    fx.import_via(&fx.importer(&jev), "chat-a", "sess-a");
    assert_eq!(fx.source("chat-a"), "jev");

    let bytes = std::fs::read(fx.cursor_path("chat-a")).unwrap();
    let keyless = MockJev::keyless();
    assert!(
        fx.importer(&keyless)
            .reclassify_stale_classifier_version_report()
            .unwrap()
            .is_empty()
    );
    // The default importer (no classifier configured at all) is equally inert.
    assert_eq!(
        fx.core
            .external_import
            .reclassify_stale_classifier_version()
            .unwrap(),
        (0, 0)
    );
    assert_eq!(keyless.calls(), 0);
    assert_eq!(std::fs::read(fx.cursor_path("chat-a")).unwrap(), bytes);
    assert_eq!(fx.category("chat-a"), "planning");
    fx.core.shutdown().await;
}

#[tokio::test]
async fn jev_stamped_chat_with_key_present_is_left_alone() {
    let fx = Fixture::new();
    let jev = MockJev::says("planning");
    fx.import_via(&fx.importer(&jev), "chat-a", "sess-a");
    let calls_after_import = jev.calls();
    assert!(
        fx.importer(&jev)
            .reclassify_stale_classifier_version_report()
            .unwrap()
            .is_empty()
    );
    assert_eq!(jev.calls(), calls_after_import);
    fx.core.shutdown().await;
}

#[tokio::test]
async fn cap_limits_jev_reclassifications_per_pass() {
    let fx = Fixture::new();
    assert_eq!(JEV_RECLASSIFY_MAX_PER_PASS, 25);
    let ids = fx.heuristic_chats(26);
    let jev = MockJev::says("planning");
    let importer = fx.importer(&jev);

    let report = importer
        .reclassify_stale_classifier_version_report()
        .unwrap();
    assert_eq!(jev.calls(), 25);
    assert_eq!(report.jev_classified, 25);
    assert_eq!(report.jev_deferred, 1);
    assert_eq!(fx.count_with_source(&ids, "jev"), 25);
    assert_eq!(fx.count_with_source(&ids, "heuristic"), 1);
    // The deferred one is untouched (still its heuristic category), not garbled.
    let left = ids.iter().find(|id| fx.source(id) == "heuristic").unwrap();
    assert_eq!(fx.category(left), "debug");

    // Next pass picks up the remainder.
    let report = importer
        .reclassify_stale_classifier_version_report()
        .unwrap();
    assert_eq!((report.jev_classified, report.jev_deferred), (1, 0));
    assert_eq!(jev.calls(), 26);
    assert_eq!(fx.count_with_source(&ids, "jev"), 26);
    assert!(
        importer
            .reclassify_stale_classifier_version_report()
            .unwrap()
            .is_empty()
    );
    fx.core.shutdown().await;
}

#[tokio::test]
async fn version_stale_chats_beyond_the_cap_still_get_a_heuristic_restamp() {
    let fx = Fixture::new();
    let ids = fx.heuristic_chats(26);
    for id in &ids {
        fx.make_stale(id); // category "research", unstamped
    }
    let jev = MockJev::says("planning");
    let report = fx
        .importer(&jev)
        .reclassify_stale_classifier_version_report()
        .unwrap();
    assert_eq!(jev.calls(), 25);
    assert_eq!(report.jev_classified, 25);
    assert_eq!(report.jev_deferred, 0);
    assert_eq!(report.recategorized + report.restamped, 26);

    // Every chat was handled: none left on the stale "research" category or an old stamp.
    for id in &ids {
        assert_eq!(fx.cursor(id)["classifierVersion"], CLASSIFIER_VERSION);
    }
    assert_eq!(fx.count_with_source(&ids, "jev"), 25);
    let over_cap = ids.iter().find(|id| fx.source(id) == "heuristic").unwrap();
    assert_eq!(fx.category(over_cap), "debug"); // the current heuristic's answer

    // The heuristic-stamped straggler upgrades on the next keyed pass.
    let report = fx
        .importer(&jev)
        .reclassify_stale_classifier_version_report()
        .unwrap();
    assert_eq!(report.jev_classified, 1);
    assert_eq!(jev.calls(), 26);
    fx.core.shutdown().await;
}

#[tokio::test]
async fn keyless_pass_reclassifies_version_stale_chats_uncapped_with_the_heuristic() {
    let fx = Fixture::new();
    let ids = fx.heuristic_chats(30);
    for id in &ids {
        fx.make_stale(id);
    }
    let keyless = MockJev::keyless();
    let report = fx
        .importer(&keyless)
        .reclassify_stale_classifier_version_report()
        .unwrap();
    assert_eq!(report.recategorized, 30);
    assert_eq!(report.jev_classified, 0);
    assert_eq!(keyless.calls(), 0);
    assert_eq!(fx.count_with_source(&ids, "heuristic"), 30);
    assert!(ids.iter().all(|id| fx.category(id) == "debug"));
    fx.core.shutdown().await;
}

#[tokio::test]
async fn failed_jev_in_the_pass_falls_back_counts_against_the_cap_and_retries_later() {
    let fx = Fixture::new();
    let ids = fx.heuristic_chats(3);
    let jev = MockJev::new(true, JevOutcome::Failed);
    let importer = fx.importer(&jev);

    let report = importer
        .reclassify_stale_classifier_version_report()
        .unwrap();
    assert_eq!(jev.calls(), 3);
    assert_eq!(report.jev_classified, 0);
    assert_eq!(fx.count_with_source(&ids, "heuristic"), 3);
    assert!(ids.iter().all(|id| fx.category(id) == "debug"));

    // Transient failure: still key-present-stale, so the next pass tries again...
    *jev.outcome.lock().unwrap() = JevOutcome::Category("research".into());
    let report = importer
        .reclassify_stale_classifier_version_report()
        .unwrap();
    assert_eq!(report.jev_classified, 3);
    assert_eq!(jev.calls(), 6);
    assert!(
        ids.iter()
            .all(|id| fx.category(id) == "research" && fx.source(id) == "jev")
    );
    fx.core.shutdown().await;
}

#[tokio::test]
async fn inconclusive_jev_is_not_retried_every_boot() {
    let fx = Fixture::new();
    fx.import("chat-a", "sess-a");
    let jev = MockJev::new(true, JevOutcome::Inconclusive);
    let importer = fx.importer(&jev);

    let report = importer
        .reclassify_stale_classifier_version_report()
        .unwrap();
    assert_eq!(jev.calls(), 1);
    assert_eq!(report.jev_classified, 0);
    assert_eq!(fx.source("chat-a"), "heuristic");
    assert_eq!(fx.category("chat-a"), "debug");
    assert_eq!(
        fx.cursor("chat-a")["jevInconclusiveVersion"],
        CLASSIFIER_VERSION
    );

    // Deterministic outcome: later passes leave it alone (and spend nothing).
    assert!(
        importer
            .reclassify_stale_classifier_version_report()
            .unwrap()
            .is_empty()
    );
    assert_eq!(jev.calls(), 1);
    fx.core.shutdown().await;
}

#[tokio::test]
async fn old_stamp_without_a_source_reads_as_heuristic() {
    let fx = Fixture::new();
    fx.import("chat-a", "sess-a");
    // A v2-era cursor: numeric stamp, no classifierSource field at all.
    fx.edit_cursor("chat-a", |j| {
        j["classifierVersion"] = 2.into();
        j.as_object_mut().unwrap().remove("classifierSource");
        j.as_object_mut().unwrap().remove("jevInconclusiveVersion");
    });
    assert_eq!(fx.source("chat-a"), "<absent>");
    let (recategorized, restamped) = fx
        .core
        .external_import
        .reclassify_stale_classifier_version()
        .unwrap();
    assert_eq!(recategorized + restamped, 1);
    assert_eq!(fx.cursor("chat-a")["classifierVersion"], CLASSIFIER_VERSION);
    assert_eq!(fx.source("chat-a"), "heuristic");
    fx.core.shutdown().await;
}
