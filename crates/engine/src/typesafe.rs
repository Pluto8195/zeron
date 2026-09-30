//! TypeSafe (Jev, System One) client for classifying an imported chat into one
//! of the seven task categories. Sits in front of the heuristic
//! (`external_import::classify_heuristic`): Jev first, heuristic on ANY
//! failure, exactly the contract `chat_workspace_plan::ask_jev_needs_worktree`
//! uses for its own judgment.
//!
//! ## Question type
//!
//! One `choice` question (TypeSafe's categorical primitive: pick one option
//! from a set, get the full probability distribution back), not seven `noul`
//! questions + argmax. The answer is read from `probabilities` (argmax over the
//! seven known categories), and a top probability under [`MIN_TOP_PROBABILITY`]
//! is treated as inconclusive so a coin-flip never overrides the heuristic.
//!
//! ## State
//!
//! The same inputs `classify_heuristic` consumes, plus the openers it scans for
//! debug intent: the first few human messages (truncated), skills loaded, the
//! per-tool call tally with edit/read/total rollups, and the turn count. See
//! [`ChatClassificationInput`].
//!
//! ## Why a trait
//!
//! [`ChatCategoryClassifier`] is the seam the importer calls through, so tests
//! inject a canned classifier and never spend the real key. The importer's
//! default is "no Jev" ([`NoJev`]); only the production engine assembly
//! installs [`LiveJevClassifier`].
//!
//! TODO: `chat_workspace_plan.rs` duplicates the endpoint/model/timeout
//! constants and the key read below; it should migrate to this module once
//! that file is no longer being edited elsewhere.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Same endpoint/model/auth convention as `chat_workspace_plan.rs` and
/// `session_canvas_server.py` (duplicated on purpose; see the module TODO).
const TYPESAFE_API_URL: &str = "https://api.typesafe.ai/v1/systemone";
const TYPESAFE_MODEL: &str = "jev-latest";
/// Hard deadline for one whole call (request + parse). Import and boot passes
/// must never stall on a slow or hung TypeSafe response.
const JEV_TIMEOUT: Duration = Duration::from_secs(3);

/// Per-message truncation before sending: the heuristic scans 4000 chars of a
/// message's head for debug words, but a classification judgment needs the gist,
/// not a pasted log.
const MESSAGE_TRUNCATE_CHARS: usize = 1000;
/// Leading human messages sent (mirrors `DEBUG_PROMPT_WINDOW` in
/// `external_import.rs`).
const MAX_MESSAGES: usize = 5;
const MAX_SKILLS: usize = 30;
const MAX_TOOLS: usize = 25;

/// Top probability below this is "the model is torn" -> inconclusive -> heuristic.
pub const MIN_TOP_PROBABILITY: f64 = 0.4;

const QUESTION_KEY: &str = "category";

/// The seven task categories, in tie-break order (earlier wins an exact tie).
pub const CHAT_CATEGORIES: [&str; 7] = [
    "implementing",
    "pr_review",
    "debug",
    "research",
    "planning",
    "quick_question",
    "other",
];

const QUESTION_INSTRUCTIONS: &str = "Which kind of work is this coding-agent chat? The state \
    holds the opening human messages of the chat, the skills it loaded, how many times each tool \
    was called (with edit/read/total rollups), and the number of turns. Pick the single category \
    that best describes what the chat is mostly about.";

/// What the classifier sees about a chat. Built from the same transcript tally
/// the heuristic runs on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChatClassificationInput {
    /// The chat's first few human-typed messages (synthetic prompts excluded).
    pub first_messages: Vec<String>,
    pub skills_loaded: Vec<String>,
    pub tool_counts: BTreeMap<String, usize>,
    pub turn_count: usize,
}

/// Result of one Jev classification attempt. Distinguishes "the service
/// answered but was torn" (deterministic, don't retry every boot) from "the
/// call failed" (transient, worth retrying later).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JevOutcome {
    /// One of [`CHAT_CATEGORIES`].
    Category(String),
    /// Valid response whose top probability is under [`MIN_TOP_PROBABILITY`].
    Inconclusive,
    /// No key, HTTP error, timeout, bad JSON, or an unusable response.
    Failed,
}

/// The importer's seam to Jev. Blocking (the importer's passes are).
pub trait ChatCategoryClassifier: Send + Sync {
    /// Whether classification is currently possible at all (for the live
    /// implementation: the API key is present). Drives the "heuristic-stamped
    /// chats upgrade once a key exists" staleness rule; a false here means
    /// nothing is stale for that reason, so a keyless boot never churns.
    fn available(&self) -> bool;
    fn classify(&self, input: &ChatClassificationInput) -> JevOutcome;
}

/// Default classifier: never available, never called. Keeps tests and any
/// embedder that didn't opt in off the network.
pub struct NoJev;

impl ChatCategoryClassifier for NoJev {
    fn available(&self) -> bool {
        false
    }
    fn classify(&self, _input: &ChatClassificationInput) -> JevOutcome {
        JevOutcome::Failed
    }
}

/// `TYPESAFE_API_KEY`, `None` when unset or empty.
pub fn api_key() -> Option<String> {
    std::env::var("TYPESAFE_API_KEY").ok().filter(|s| !s.is_empty())
}

/// One System One `choice` call classifying a chat into [`CHAT_CATEGORIES`].
/// `None` on ANY failure (no key, network error, timeout, bad JSON, torn
/// answer): every caller falls back to the heuristic.
pub async fn classify_with_jev(http: &reqwest::Client, input: &ChatClassificationInput) -> Option<String> {
    match classify_outcome(http, input).await {
        JevOutcome::Category(category) => Some(category),
        JevOutcome::Inconclusive | JevOutcome::Failed => None,
    }
}

/// [`classify_with_jev`], keeping the failed/inconclusive distinction.
pub async fn classify_outcome(http: &reqwest::Client, input: &ChatClassificationInput) -> JevOutcome {
    let Some(api_key) = api_key() else {
        return JevOutcome::Failed;
    };
    classify_at(http, TYPESAFE_API_URL, &api_key, JEV_TIMEOUT, input).await
}

/// Endpoint/key/timeout-injectable core, so tests can point it at a local
/// server.
async fn classify_at(
    http: &reqwest::Client,
    url: &str,
    api_key: &str,
    timeout: Duration,
    input: &ChatClassificationInput,
) -> JevOutcome {
    let payload = build_category_payload(input);
    let call = async {
        let response = http.post(url).bearer_auth(api_key).json(&payload).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        let body: serde_json::Value = response.json().await.ok()?;
        Some(parse_category_response(&body))
    };
    match tokio::time::timeout(timeout, call).await {
        Ok(Some(outcome)) => outcome,
        Ok(None) | Err(_) => JevOutcome::Failed,
    }
}

/// Builds the `POST /v1/systemone` body: the chat as a structured `state`
/// object and a single `choice` question over the seven categories.
pub fn build_category_payload(input: &ChatClassificationInput) -> serde_json::Value {
    let messages: Vec<String> = input
        .first_messages
        .iter()
        .take(MAX_MESSAGES)
        .map(|m| truncate_chars(m.trim(), MESSAGE_TRUNCATE_CHARS))
        .collect();
    let skills: Vec<&String> = input.skills_loaded.iter().take(MAX_SKILLS).collect();

    let sum = |names: &[&str]| -> usize { names.iter().map(|n| input.tool_counts.get(*n).copied().unwrap_or(0)).sum() };
    let total: usize = input.tool_counts.values().sum();
    // Most-called tools first; the long tail (MCP tools etc.) is dropped.
    let mut by_count: Vec<(&String, &usize)> = input.tool_counts.iter().collect();
    by_count.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let tool_calls: serde_json::Map<String, serde_json::Value> = by_count
        .into_iter()
        .take(MAX_TOOLS)
        .map(|(name, count)| (name.clone(), serde_json::json!(count)))
        .collect();

    let state = serde_json::json!({
        "first_human_messages": messages,
        "skills_loaded": skills,
        "tool_calls": tool_calls,
        "tool_call_totals": {
            "total": total,
            "edit_or_write": sum(&["Edit", "Write", "NotebookEdit"]),
            "read_or_search": sum(&["Read", "Grep", "Glob"]),
        },
        "turn_count": input.turn_count,
    });

    serde_json::json!({
        "state": state,
        "model": TYPESAFE_MODEL,
        "questions": {
            QUESTION_KEY: {
                "type": "choice",
                "instructions": QUESTION_INSTRUCTIONS,
                "criteria": category_criteria(),
            },
        },
    })
}

fn category_criteria() -> serde_json::Value {
    serde_json::json!({
        "implementing": {
            "what": "Building or changing code: new features, refactors, migrations, and \
                fixes that are mostly about writing the change. Typically several file edits.",
            "not_for": "Bug hunts and incident work (debug), reviewing someone else's change \
                (pr_review), or read-only exploration (research).",
        },
        "pr_review": {
            "what": "Reviewing a pull request or diff: code review, checking CI on a PR, \
                assessing risk of a change, stacking or opening PRs.",
            "not_for": "Writing the feature being reviewed (implementing).",
        },
        "debug": {
            "what": "Something is broken and being hunted or fixed: bugs, firefights, \
                incidents, production errors, crashes, regressions, failing behavior, and \
                Sentry / Datadog / log or metric deep-dives to find a root cause. This holds \
                even when the work reads and edits code like an implementation or a research \
                task.",
            "not_for": "Planned feature work with no fault being chased (implementing), or \
                neutral data/code exploration (research).",
        },
        "research": {
            "what": "Reading and exploring code, docs, the web, or data (queries, analytics, \
                metrics) to understand something, with little or no editing.",
            "not_for": "Anything where a fault is being chased (debug) or a change is being \
                built (implementing).",
        },
        "planning": {
            "what": "Designing an approach before building it: specs, tickets, architecture \
                decisions, task breakdowns, roadmaps.",
            "not_for": "Actually making the changes (implementing).",
        },
        "quick_question": {
            "what": "A short exchange, a few turns and at most a couple of tool calls: a \
                one-off question, a lookup, a small clarification.",
            "not_for": "Longer sessions, even on small topics.",
        },
        "other": {
            "what": "Anything that fits none of the above: chit-chat, admin, setup, \
                configuration, or work with no clear type.",
            "not_for": null,
        },
    })
}

/// Reads `answers.category.probabilities`, takes the argmax over the seven known
/// categories (exact ties go to the earlier entry of [`CHAT_CATEGORIES`]), and
/// applies the [`MIN_TOP_PROBABILITY`] floor. A missing/malformed answer, or a
/// distribution with no known category in it, is [`JevOutcome::Failed`].
pub fn parse_category_response(body: &serde_json::Value) -> JevOutcome {
    let Some(probabilities) = body
        .get("answers")
        .and_then(|a| a.get(QUESTION_KEY))
        .and_then(|a| a.get("probabilities"))
        .and_then(|p| p.as_object())
    else {
        return JevOutcome::Failed;
    };
    let mut best: Option<(&str, f64)> = None;
    for category in CHAT_CATEGORIES {
        let Some(p) = probabilities.get(category).and_then(serde_json::Value::as_f64) else {
            continue;
        };
        if !p.is_finite() {
            continue;
        }
        if best.is_none_or(|(_, top)| p > top) {
            best = Some((category, p));
        }
    }
    match best {
        None => JevOutcome::Failed,
        Some((_, p)) if p < MIN_TOP_PROBABILITY => JevOutcome::Inconclusive,
        Some((category, _)) => JevOutcome::Category(category.to_string()),
    }
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        text.chars().take(max).collect()
    }
}

/// Consecutive failures before the breaker opens.
const BREAKER_THRESHOLD: u32 = 3;
/// How long an open breaker short-circuits calls.
const BREAKER_COOLDOWN: Duration = Duration::from_secs(5 * 60);

#[derive(Default)]
struct Breaker {
    consecutive_failures: u32,
    open_until: Option<Instant>,
}

/// The production classifier: real TypeSafe calls, made from a synchronous
/// caller. Each call runs on its own short-lived thread with its own
/// current-thread runtime and a throwaway HTTP client, so it is safe from any
/// context (plain thread, `spawn_blocking`, or directly on a runtime worker)
/// and never depends on the caller's runtime.
///
/// A small circuit breaker keeps a hung or down TypeSafe from costing the
/// full timeout on every chat of an auto-adopt sweep: after
/// [`BREAKER_THRESHOLD`] consecutive failures calls short-circuit to
/// [`JevOutcome::Failed`] for [`BREAKER_COOLDOWN`].
#[derive(Default)]
pub struct LiveJevClassifier {
    breaker: Mutex<Breaker>,
}

impl LiveJevClassifier {
    pub fn new() -> Self {
        Self::default()
    }

    fn breaker_open(&self) -> bool {
        let mut breaker = self.breaker.lock().unwrap_or_else(|e| e.into_inner());
        match breaker.open_until {
            Some(until) if Instant::now() < until => true,
            Some(_) => {
                // Cooldown elapsed: half-open, allow probing again.
                breaker.open_until = None;
                breaker.consecutive_failures = BREAKER_THRESHOLD - 1;
                false
            }
            None => false,
        }
    }

    fn record(&self, outcome: &JevOutcome) {
        let mut breaker = self.breaker.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(outcome, JevOutcome::Failed) {
            breaker.consecutive_failures += 1;
            if breaker.consecutive_failures >= BREAKER_THRESHOLD {
                breaker.open_until = Some(Instant::now() + BREAKER_COOLDOWN);
            }
        } else {
            breaker.consecutive_failures = 0;
            breaker.open_until = None;
        }
    }
}

impl ChatCategoryClassifier for LiveJevClassifier {
    fn available(&self) -> bool {
        api_key().is_some()
    }

    fn classify(&self, input: &ChatClassificationInput) -> JevOutcome {
        if self.breaker_open() || !self.available() {
            return JevOutcome::Failed;
        }
        let input = input.clone();
        let outcome = std::thread::Builder::new()
            .name("jev-classify".into())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
                    return JevOutcome::Failed;
                };
                runtime.block_on(async {
                    let Ok(http) = reqwest::Client::builder().pool_max_idle_per_host(0).build() else {
                        return JevOutcome::Failed;
                    };
                    classify_outcome(&http, &input).await
                })
            })
            .ok()
            .and_then(|handle| handle.join().ok())
            .unwrap_or(JevOutcome::Failed);
        self.record(&outcome);
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn input() -> ChatClassificationInput {
        ChatClassificationInput {
            first_messages: vec!["sentry shows a crash in the health endpoint".into()],
            skills_loaded: vec!["sentry-api".into()],
            tool_counts: BTreeMap::from([("Read".to_string(), 7), ("Edit".to_string(), 2), ("Bash".to_string(), 1)]),
            turn_count: 9,
        }
    }

    // ── payload ─────────────────────────────────────────────────────────

    #[test]
    fn payload_shape() {
        let payload = build_category_payload(&input());
        assert_eq!(payload["model"], "jev-latest");
        let state = &payload["state"];
        assert_eq!(state["first_human_messages"][0], "sentry shows a crash in the health endpoint");
        assert_eq!(state["skills_loaded"][0], "sentry-api");
        assert_eq!(state["tool_calls"]["Read"], 7);
        assert_eq!(state["tool_call_totals"]["total"], 10);
        assert_eq!(state["tool_call_totals"]["edit_or_write"], 2);
        assert_eq!(state["tool_call_totals"]["read_or_search"], 7);
        assert_eq!(state["turn_count"], 9);

        let question = &payload["questions"]["category"];
        assert_eq!(question["type"], "choice");
        assert!(question["instructions"].is_string());
        let criteria = question["criteria"].as_object().unwrap();
        for category in CHAT_CATEGORIES {
            assert!(criteria.contains_key(category), "missing criterion for {category}");
        }
        assert_eq!(criteria.len(), CHAT_CATEGORIES.len());
        let debug = criteria["debug"]["what"].as_str().unwrap();
        for word in ["bugs", "firefights", "incidents", "Sentry"] {
            assert!(debug.contains(word), "debug criterion should mention {word}");
        }
    }

    #[test]
    fn payload_truncates_and_caps_inputs() {
        let mut big = input();
        big.first_messages = (0..9).map(|_| "a".repeat(5000)).collect();
        big.skills_loaded = (0..100).map(|i| format!("skill-{i}")).collect();
        big.tool_counts = (0..100).map(|i| (format!("tool-{i}"), i)).collect();
        let payload = build_category_payload(&big);
        let messages = payload["state"]["first_human_messages"].as_array().unwrap();
        assert_eq!(messages.len(), MAX_MESSAGES);
        assert!(messages.iter().all(|m| m.as_str().unwrap().chars().count() == MESSAGE_TRUNCATE_CHARS));
        assert_eq!(payload["state"]["skills_loaded"].as_array().unwrap().len(), MAX_SKILLS);
        let tools = payload["state"]["tool_calls"].as_object().unwrap();
        assert_eq!(tools.len(), MAX_TOOLS);
        // Totals still reflect every tool, not just the ones sent.
        assert_eq!(payload["state"]["tool_call_totals"]["total"], (0..100).sum::<usize>());
        assert!(tools.contains_key("tool-99"), "most-called tools are kept");
    }

    #[test]
    fn payload_truncation_is_char_safe() {
        let mut multibyte = input();
        multibyte.first_messages = vec!["\u{1f600}".repeat(2000)];
        let payload = build_category_payload(&multibyte);
        let message = payload["state"]["first_human_messages"][0].as_str().unwrap();
        assert_eq!(message.chars().count(), MESSAGE_TRUNCATE_CHARS);
    }

    // ── parse / argmax / floor ──────────────────────────────────────────

    fn response(probabilities: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "model": "jev-1.13.0",
            "answers": {"category": {"type": "choice", "choice": "ignored", "probabilities": probabilities, "confidence": 0.5}},
            "usage": {"input_tokens": 1, "output_tokens": 1},
        })
    }

    #[test]
    fn parse_takes_the_argmax() {
        let body = response(serde_json::json!({
            "implementing": 0.05, "pr_review": 0.0, "debug": 0.8, "research": 0.1,
            "planning": 0.0, "quick_question": 0.05, "other": 0.0,
        }));
        assert_eq!(parse_category_response(&body), JevOutcome::Category("debug".into()));
    }

    #[test]
    fn parse_uses_probabilities_not_the_choice_field() {
        let mut body = response(serde_json::json!({"research": 0.9, "other": 0.1}));
        body["answers"]["category"]["choice"] = "planning".into();
        assert_eq!(parse_category_response(&body), JevOutcome::Category("research".into()));
    }

    #[test]
    fn parse_below_the_floor_is_inconclusive() {
        let body = response(serde_json::json!({
            "implementing": 0.39, "pr_review": 0.0, "debug": 0.3, "research": 0.3,
            "planning": 0.0, "quick_question": 0.01, "other": 0.0,
        }));
        assert_eq!(parse_category_response(&body), JevOutcome::Inconclusive);
    }

    #[test]
    fn parse_at_the_floor_is_accepted() {
        let body = response(serde_json::json!({"implementing": 0.4, "debug": 0.3, "research": 0.3}));
        assert_eq!(parse_category_response(&body), JevOutcome::Category("implementing".into()));
    }

    #[test]
    fn parse_exact_tie_goes_to_the_earlier_category() {
        let body = response(serde_json::json!({"research": 0.5, "debug": 0.5}));
        assert_eq!(parse_category_response(&body), JevOutcome::Category("debug".into()));
    }

    #[test]
    fn parse_ignores_unknown_options() {
        let body = response(serde_json::json!({"mystery": 0.95, "other": 0.5}));
        assert_eq!(parse_category_response(&body), JevOutcome::Category("other".into()));
    }

    #[test]
    fn parse_malformed_is_failed() {
        assert_eq!(parse_category_response(&serde_json::json!({})), JevOutcome::Failed);
        assert_eq!(parse_category_response(&serde_json::json!({"answers": {}})), JevOutcome::Failed);
        assert_eq!(
            parse_category_response(&serde_json::json!({"answers": {"category": {"choice": "debug"}}})),
            JevOutcome::Failed
        );
        assert_eq!(parse_category_response(&response(serde_json::json!({"mystery": 1.0}))), JevOutcome::Failed);
        assert_eq!(parse_category_response(&response(serde_json::json!("nope"))), JevOutcome::Failed);
    }

    // ── HTTP failure contract (local server, no TypeSafe) ───────────────

    /// One-shot local HTTP server: answers the first request with `status`
    /// and `body` (or never answers when `hang`), returns its URL.
    async fn serve_once(status: u16, body: &'static str, hang: bool) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 65536];
            let _ = stream.read(&mut buf).await;
            if hang {
                tokio::time::sleep(Duration::from_secs(30)).await;
                return;
            }
            let reply = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(reply.as_bytes()).await;
        });
        url
    }

    const GOOD_BODY: &str = r#"{"model":"m","answers":{"category":{"type":"choice","choice":"debug","probabilities":{"debug":0.9,"other":0.1},"confidence":0.8}},"usage":{}}"#;

    async fn run(url: &str, timeout: Duration) -> JevOutcome {
        classify_at(&reqwest::Client::new(), url, "test-key", timeout, &input()).await
    }

    #[tokio::test]
    async fn http_success_yields_the_category() {
        let url = serve_once(200, GOOD_BODY, false).await;
        assert_eq!(run(&url, Duration::from_secs(5)).await, JevOutcome::Category("debug".into()));
    }

    #[tokio::test]
    async fn http_error_status_is_failed() {
        let url = serve_once(500, GOOD_BODY, false).await;
        assert_eq!(run(&url, Duration::from_secs(5)).await, JevOutcome::Failed);
        let url = serve_once(401, r#"{"error":"nope"}"#, false).await;
        assert_eq!(run(&url, Duration::from_secs(5)).await, JevOutcome::Failed);
    }

    #[tokio::test]
    async fn bad_json_is_failed() {
        let url = serve_once(200, "not json at all", false).await;
        assert_eq!(run(&url, Duration::from_secs(5)).await, JevOutcome::Failed);
    }

    #[tokio::test]
    async fn torn_answer_is_inconclusive_over_http() {
        let body = r#"{"answers":{"category":{"probabilities":{"debug":0.3,"research":0.3,"other":0.4,"planning":0.0}}}}"#;
        // 0.4 is exactly the floor, so accepted; drop it just under.
        let torn = body.replace("0.4", "0.35");
        let torn: &'static str = Box::leak(torn.into_boxed_str());
        let url = serve_once(200, torn, false).await;
        assert_eq!(run(&url, Duration::from_secs(5)).await, JevOutcome::Inconclusive);
    }

    #[tokio::test]
    async fn timeout_is_failed() {
        let url = serve_once(200, GOOD_BODY, true).await;
        let started = Instant::now();
        assert_eq!(run(&url, Duration::from_millis(150)).await, JevOutcome::Failed);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn connection_refused_is_failed() {
        // Bind then drop to get a port nothing listens on.
        let port = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}/v1/systemone");
        assert_eq!(run(&url, Duration::from_secs(5)).await, JevOutcome::Failed);
    }

    // ── breaker ─────────────────────────────────────────────────────────

    #[test]
    fn breaker_opens_after_consecutive_failures_and_closes_on_success() {
        let live = LiveJevClassifier::new();
        assert!(!live.breaker_open());
        for _ in 0..BREAKER_THRESHOLD {
            live.record(&JevOutcome::Failed);
        }
        assert!(live.breaker_open());

        let live = LiveJevClassifier::new();
        live.record(&JevOutcome::Failed);
        live.record(&JevOutcome::Failed);
        live.record(&JevOutcome::Inconclusive); // service answered: resets
        live.record(&JevOutcome::Failed);
        assert!(!live.breaker_open());
    }

    /// Live smoke test against the real endpoint; never runs in normal CI.
    #[test]
    #[ignore = "requires TYPESAFE_API_KEY and network access"]
    fn live_classify_with_jev_runs_end_to_end() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let outcome = classify_outcome(&reqwest::Client::new(), &input()).await;
            eprintln!("live outcome: {outcome:?}");
            assert!(matches!(outcome, JevOutcome::Category(_)), "expected a conclusive live answer");
        });
    }

    #[test]
    fn no_jev_is_never_available() {
        assert!(!NoJev.available());
        assert_eq!(NoJev.classify(&input()), JevOutcome::Failed);
    }
}
