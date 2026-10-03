//! `ExternalSessionImporter::reclassify_other_chats`: the user-triggered
//! "Reclassify other" action. Re-runs the Jev-first classification on chats
//! filed under `other`, ignoring version/source stamps and the boot pass's
//! `jevInconclusiveVersion` park, capped at `JEV_RECLASSIFY_MAX_MANUAL` calls.
//!
//! A canned [`MockJev`] stands in for the classifier: no network, no key.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use zeron_engine::external_import::{
    CLASSIFIER_VERSION, ExternalSessionImporter, JEV_RECLASSIFY_MAX_MANUAL, ReclassifyOtherReport,
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

    fn category(&self, chat_id: &str) -> String {
        self.core
            .external_import
            .classification_for(chat_id)
            .expect("read classification")
            .expect("classification present")
            .0
    }
}

/// Canned classifier: fixed outcome, switchable availability ("is the key
/// present"), and a call counter.
struct MockJev {
    available: AtomicBool,
    outcome: Mutex<JevOutcome>,
    calls: AtomicUsize,
    inputs: Mutex<Vec<ChatClassificationInput>>,
    /// Flips the breaker open after this many calls (`usize::MAX` = never).
    trip_after: AtomicUsize,
}

impl MockJev {
    fn new(available: bool, outcome: JevOutcome) -> Arc<Self> {
        Arc::new(Self {
            available: AtomicBool::new(available),
            outcome: Mutex::new(outcome),
            calls: AtomicUsize::new(0),
            inputs: Mutex::new(Vec::new()),
            trip_after: AtomicUsize::new(usize::MAX),
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
    fn circuit_open(&self) -> bool {
        self.calls.load(Ordering::SeqCst) >= self.trip_after.load(Ordering::SeqCst)
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

    fn source(&self, chat_id: &str) -> String {
        self.cursor(chat_id)["classifierSource"]
            .as_str()
            .unwrap_or("<absent>")
            .to_string()
    }
}

impl Fixture {
    /// `n` imported chats, all filed under `other` with a current heuristic stamp.
    fn other_chats(&self, n: usize) -> Vec<String> {
        (0..n)
            .map(|i| {
                let id = format!("chat-{i:02}");
                self.import(&id, &format!("sess-{i:02}"));
                self.edit_cursor(&id, |j| j["category"] = "other".into());
                id
            })
            .collect()
    }

    fn reclassify(&self, jev: &Arc<MockJev>) -> ReclassifyOtherReport {
        self.importer(jev)
            .reclassify_other_chats()
            .expect("reclassify")
    }

    fn parked(&self, chat_id: &str) -> bool {
        self.cursor(chat_id)
            .get("jevInconclusiveVersion")
            .is_some_and(|v| !v.is_null())
    }
}

fn report(
    examined: usize,
    reclassified: usize,
    unchanged: usize,
    jev_calls: usize,
    deferred: usize,
) -> ReclassifyOtherReport {
    ReclassifyOtherReport {
        examined,
        reclassified,
        unchanged,
        jev_calls,
        deferred,
        ..Default::default()
    }
}

#[tokio::test]
async fn reclassifies_an_other_chat_when_jev_answers_a_category() {
    let fx = Fixture::new();
    let ids = fx.other_chats(1);
    assert_eq!(fx.category(&ids[0]), "other");

    let jev = MockJev::says("planning");
    assert_eq!(fx.reclassify(&jev), report(1, 1, 0, 1, 0));
    assert_eq!(fx.category(&ids[0]), "planning");
    assert_eq!(fx.source(&ids[0]), "jev");
    assert_eq!(fx.cursor(&ids[0])["classifierVersion"], CLASSIFIER_VERSION);
    assert!(!fx.parked(&ids[0]));
    // Unrelated cursor state survives.
    assert_eq!(fx.cursor(&ids[0])["externalSessionId"], "sess-00");

    // Only `other` chats are examined: it is now `planning`, so a rerun is empty.
    assert_eq!(fx.reclassify(&jev), report(0, 0, 0, 0, 0));
    assert_eq!(jev.calls(), 1);
    fx.core.shutdown().await;
}

#[tokio::test]
async fn non_other_chats_are_never_touched_or_offered_to_jev() {
    let fx = Fixture::new();
    fx.import("chat-debug", "sess-d"); // heuristic: debug
    let ids = fx.other_chats(1);
    let before = std::fs::read(fx.cursor_path("chat-debug")).unwrap();
    let jev = MockJev::says("research");
    assert_eq!(fx.reclassify(&jev), report(1, 1, 0, 1, 0));
    assert_eq!(std::fs::read(fx.cursor_path("chat-debug")).unwrap(), before);
    assert_eq!(fx.category("chat-debug"), "debug");
    assert_eq!(fx.category(&ids[0]), "research");
    fx.core.shutdown().await;
}

#[tokio::test]
async fn an_empty_category_counts_as_other() {
    let fx = Fixture::new();
    let ids = fx.other_chats(1);
    fx.edit_cursor(&ids[0], |j| j["category"] = "".into());
    let jev = MockJev::says("research");
    assert_eq!(fx.reclassify(&jev), report(1, 1, 0, 1, 0));
    assert_eq!(fx.category(&ids[0]), "research");
    fx.core.shutdown().await;
}

#[tokio::test]
async fn jev_keeping_other_is_unchanged_but_restamped() {
    let fx = Fixture::new();
    let ids = fx.other_chats(1);
    let jev = MockJev::says("other");
    assert_eq!(
        fx.reclassify(&jev),
        ReclassifyOtherReport {
            confirmed_other: 1,
            ..report(1, 0, 1, 1, 0)
        }
    );
    assert_eq!(fx.category(&ids[0]), "other");
    assert_eq!(fx.source(&ids[0]), "jev");
    fx.core.shutdown().await;
}

#[tokio::test]
async fn inconclusive_leaves_other_and_does_not_block_the_next_manual_run() {
    let fx = Fixture::new();
    let ids = fx.other_chats(1);
    let jev = MockJev::new(true, JevOutcome::Inconclusive);
    assert_eq!(
        fx.reclassify(&jev),
        ReclassifyOtherReport {
            inconclusive: 1,
            ..report(1, 0, 1, 1, 0)
        }
    );
    assert_eq!(fx.category(&ids[0]), "other");
    // Parked for the BOOT pass (it must not spend budget on it)...
    assert!(fx.parked(&ids[0]));
    assert!(
        fx.importer(&jev)
            .reclassify_stale_classifier_version_report()
            .unwrap()
            .is_empty(),
        "boot pass skips the parked chat"
    );
    assert_eq!(jev.calls(), 1);

    // ...but a manual click retries it regardless, and success clears the park.
    *jev.outcome.lock().unwrap() = JevOutcome::Category("research".into());
    assert_eq!(fx.reclassify(&jev), report(1, 1, 0, 1, 0));
    assert_eq!(fx.category(&ids[0]), "research");
    assert!(!fx.parked(&ids[0]));
    fx.core.shutdown().await;
}

#[tokio::test]
async fn a_chat_parked_by_the_boot_pass_is_retried_by_a_manual_run() {
    let fx = Fixture::new();
    let ids = fx.other_chats(1);
    fx.edit_cursor(&ids[0], |j| {
        j["jevInconclusiveVersion"] = CLASSIFIER_VERSION.into()
    });
    let jev = MockJev::says("planning");
    assert_eq!(fx.reclassify(&jev), report(1, 1, 0, 1, 0));
    assert_eq!(fx.category(&ids[0]), "planning");
    assert!(!fx.parked(&ids[0]));
    fx.core.shutdown().await;
}

#[tokio::test]
async fn failed_jev_leaves_the_chat_untouched_and_retryable() {
    let fx = Fixture::new();
    let ids = fx.other_chats(2);
    let before = std::fs::read(fx.cursor_path(&ids[0])).unwrap();
    let jev = MockJev::new(true, JevOutcome::Failed);
    assert_eq!(
        fx.reclassify(&jev),
        ReclassifyOtherReport {
            failed: 2,
            ..report(2, 0, 2, 2, 0)
        }
    );
    assert_eq!(
        std::fs::read(fx.cursor_path(&ids[0])).unwrap(),
        before,
        "no write on a failed call"
    );
    assert!(!fx.parked(&ids[0]));

    *jev.outcome.lock().unwrap() = JevOutcome::Category("research".into());
    assert_eq!(fx.reclassify(&jev), report(2, 2, 0, 2, 0));
    fx.core.shutdown().await;
}

#[tokio::test]
async fn unavailable_jev_makes_no_calls_and_changes_nothing() {
    let fx = Fixture::new();
    fx.other_chats(3);
    let jev = MockJev::keyless();
    assert_eq!(fx.reclassify(&jev), report(3, 0, 3, 0, 0));
    assert_eq!(jev.calls(), 0);
    // The default importer (no classifier configured) is equally inert.
    assert_eq!(
        fx.core.external_import.reclassify_other_chats().unwrap(),
        report(3, 0, 3, 0, 0)
    );
    fx.core.shutdown().await;
}

#[tokio::test]
async fn cap_is_fifty_jev_calls_and_the_remainder_is_deferred() {
    let fx = Fixture::new();
    assert_eq!(JEV_RECLASSIFY_MAX_MANUAL, 50);
    let ids = fx.other_chats(51);
    let jev = MockJev::says("planning");

    assert_eq!(fx.reclassify(&jev), report(51, 50, 0, 50, 1));
    assert_eq!(jev.calls(), 50);
    let left: Vec<_> = ids.iter().filter(|id| fx.category(id) == "other").collect();
    assert_eq!(left.len(), 1, "exactly the deferred chat is still other");
    assert!(!fx.parked(left[0]), "deferred chats are untouched");

    // Running again picks up the remainder.
    assert_eq!(fx.reclassify(&jev), report(1, 1, 0, 1, 0));
    assert_eq!(jev.calls(), 51);
    assert!(ids.iter().all(|id| fx.category(id) == "planning"));
    fx.core.shutdown().await;
}

#[tokio::test]
async fn counts_add_up_across_mixed_outcomes() {
    let fx = Fixture::new();
    fx.other_chats(4);
    fx.import("chat-debug", "sess-d"); // not other: not examined
    // Jev answers per call in order: category, other, category, failed.
    struct Sequence(Mutex<Vec<JevOutcome>>, AtomicUsize);
    impl ChatCategoryClassifier for Sequence {
        fn available(&self) -> bool {
            true
        }
        fn classify(&self, _: &ChatClassificationInput) -> JevOutcome {
            self.1.fetch_add(1, Ordering::SeqCst);
            self.0.lock().unwrap().remove(0)
        }
    }
    let seq = Arc::new(Sequence(
        Mutex::new(vec![
            JevOutcome::Category("research".into()),
            JevOutcome::Category("other".into()),
            JevOutcome::Category("planning".into()),
            JevOutcome::Failed,
        ]),
        AtomicUsize::new(0),
    ));
    let got = fx
        .core
        .external_import
        .clone()
        .with_chat_category_classifier(seq.clone())
        .reclassify_other_chats()
        .unwrap();
    assert_eq!(got.examined, 4);
    assert_eq!(
        got.reclassified + got.unchanged + got.deferred,
        got.examined
    );
    assert_eq!(
        (got.reclassified, got.unchanged, got.jev_calls, got.deferred),
        (2, 2, 4, 0)
    );
    assert_eq!(
        (got.confirmed_other, got.inconclusive, got.failed),
        (1, 0, 1)
    );
    fx.core.shutdown().await;
}

#[tokio::test]
async fn an_open_circuit_breaker_stops_the_run_and_defers_the_rest() {
    let fx = Fixture::new();
    fx.other_chats(5);
    let jev = MockJev::new(true, JevOutcome::Failed);
    jev.trip_after.store(2, Ordering::SeqCst); // breaker opens after two failed calls
    assert_eq!(
        fx.reclassify(&jev),
        ReclassifyOtherReport {
            failed: 2,
            ..report(5, 0, 2, 2, 3)
        }
    );
    assert_eq!(jev.calls(), 2);
    fx.core.shutdown().await;
}

#[tokio::test]
async fn chats_with_a_missing_transcript_are_skipped() {
    let fx = Fixture::new();
    let ids = fx.other_chats(2);
    let cursor = fx.cursor(&ids[0]);
    std::fs::remove_file(cursor["transcriptPath"].as_str().unwrap()).unwrap();
    let jev = MockJev::says("planning");
    assert_eq!(fx.reclassify(&jev), report(1, 1, 0, 1, 0));
    assert_eq!(fx.category(&ids[0]), "other");
    fx.core.shutdown().await;
}
