//! `PLAN_CHAT_WORKSPACE` / `CREATE_CHAT_WORKTREE` — the "smart worktree for
//! new chats" feature: a TypeSafe/Jev judgment on a new chat's first message
//! decides whether it gets an isolated git worktree (a code-modifying task)
//! or runs in the base checkout (investigation), and a second RPC actually
//! materializes that worktree using the `agent-mode-tools/agent-mode.sh`
//! on-disk convention (not Zeron's own pre-existing `CreateWorktree`/`Repos`
//! worktree feature, which uses a different root and branch-naming scheme —
//! see [`crate::repos::Repos::create_worktree`]). This lets a chat's worktree
//! interoperate with a superproject/submodule monorepo the same way
//! `agent-mode.sh worktree-for`/`ensure_worktree` does.
//!
//! ## TypeSafe/Jev
//!
//! Ported from `agent-mode-tools/session_canvas_server.py`'s
//! `classify_with_typesafe`/`classify_validity_with_typesafe`
//! (~line 542-559, 875-963): same endpoint (`POST
//! https://api.typesafe.ai/v1/systemone`), same model (`jev-latest`), same
//! auth (`TYPESAFE_API_KEY` env var, `Authorization: Bearer <key>`), and the
//! same "silently fall back on any failure" contract — a missing key, a
//! network error, a timeout, or an unparseable response all just return
//! `None` here, never an error the caller has to handle. The question itself
//! is a `Noul` (yes/no probability, see `docs.typesafe.ai/primitives/noul`),
//! the same primitive `classify_validity_with_typesafe`'s `still_valid`
//! question uses — its probability comes back at `answers.<key>.noul`, a
//! flat 0..1 float, not the `choice`/`confidence` shape `classify_with_typesafe`
//! uses for its category question.
//!
//! ## Worktree placement
//!
//! Matches `agent-mode.sh`'s `ensure_worktree`/`worktrees_dir_for`/
//! `assert_worktree_outside_submodules` (as landed — see that script's header
//! comment "Worktree placement"): `<superproject-root>/.worktrees/<label>/<slug>`,
//! `<label>` being the submodule's own directory name when `repoPath` sits
//! inside one, else `"workspace"`; overridable via `WORKSPACE_WORKTREES_DIR`
//! (must be absolute and itself outside every submodule root); a plain-text
//! `.workspace-root` file (never a symlink — see that script's comment on the
//! recursive-filesystem-loop incident this avoids) recording the superproject
//! path; and a hard refusal to create anything nested under a submodule root.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::EngineError;
use crate::pr_ticket_cache::{extract_ticket_id, workspace_submodule_paths};

// ── PLAN_CHAT_WORKSPACE ─────────────────────────────────────────────────────

/// Same endpoint/model/auth convention as `session_canvas_server.py`'s
/// `TYPESAFE_API_URL`/`TYPESAFE_MODEL`/`TYPESAFE_API_KEY`
/// (session_canvas_server.py:542-544).
const TYPESAFE_API_URL: &str = "https://api.typesafe.ai/v1/systemone";
const TYPESAFE_MODEL: &str = "jev-latest";
/// Hard deadline for the whole call (request + parse) — a new chat must not
/// stall waiting on a slow or hung TypeSafe response; the heuristic always
/// answers immediately.
const JEV_TIMEOUT: Duration = Duration::from_secs(3);
/// First-message truncation before it's sent to Jev — a pasted log dump or
/// essay shouldn't blow the judgment's input budget.
const MESSAGE_TRUNCATE_CHARS: usize = 2000;

const NEEDS_WORKTREE_QUESTION_KEY: &str = "needs_worktree";
const NEEDS_WORKTREE_INSTRUCTIONS: &str = "Will completing this task require modifying files or \
    code in this git repository — implementing, fixing, adding, refactoring, building, updating, \
    migrating, or otherwise writing changes — as opposed to pure investigation, reading existing \
    code, or answering a question without changing anything?";

/// Imperative code words → `needsWorktree: true` in the fallback heuristic.
/// Ticket brief's own list, not ported from the reference tool.
const IMPERATIVE_CODE_WORDS: &[&str] = &[
    "implement", "fix", "add", "refactor", "build", "update", "migrate", "write",
];
/// Investigation words → `needsWorktree: false`, but only when no code word
/// also matched (code words win ties — see [`heuristic_needs_worktree`]).
const INVESTIGATION_WORDS: &[&str] = &[
    "why", "how", "investigate", "explain", "find", "look", "research", "what",
];

/// Where a `PLAN_CHAT_WORKSPACE` judgment came from — the RPC's `source`
/// field. Wire spelling is lowercase (`"jev"` / `"heuristic"`), pinned by
/// [`tests::plan_chat_workspace_wire_shape_is_camel_case_with_lowercase_source`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlanSource {
    Jev,
    Heuristic,
}

/// `PLAN_CHAT_WORKSPACE`'s response: `{needsWorktree, probability, source}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspacePlan {
    pub needs_worktree: bool,
    /// The Jev `noul` probability that drove `needsWorktree`, `null` when the
    /// heuristic answered instead.
    pub probability: Option<f64>,
    pub source: PlanSource,
}

/// Judge whether `message` — a new chat's first message — needs an isolated
/// worktree. `cwd` is accepted for parity with the RPC's params and as a hook
/// for a future cwd-aware refinement (e.g. "already inside a worktree"); the
/// judgment itself is message-only today, same as it would be phrased to
/// Jev, and the heuristic never inspects `cwd` either.
pub async fn plan_chat_workspace(http: &reqwest::Client, message: &str, _cwd: &str) -> WorkspacePlan {
    if let Some(probability) = ask_jev_needs_worktree(http, message).await {
        return WorkspacePlan {
            needs_worktree: probability >= 0.5,
            probability: Some(probability),
            source: PlanSource::Jev,
        };
    }
    WorkspacePlan {
        needs_worktree: heuristic_needs_worktree(message),
        probability: None,
        source: PlanSource::Heuristic,
    }
}

/// `None` on ANY failure (no key, network error, timeout, bad JSON, missing
/// field) — every caller falls back to the heuristic, matching the reference
/// tool's `classify_with_typesafe`/`classify_validity_with_typesafe` contract
/// of never raising past a failed TypeSafe call.
async fn ask_jev_needs_worktree(http: &reqwest::Client, message: &str) -> Option<f64> {
    let api_key = std::env::var("TYPESAFE_API_KEY").ok().filter(|s| !s.is_empty())?;
    let payload = build_jev_payload(message);
    let call = async {
        let response = http
            .post(TYPESAFE_API_URL)
            .bearer_auth(&api_key)
            .json(&payload)
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let body: serde_json::Value = response.json().await.ok()?;
        parse_jev_probability(&body)
    };
    tokio::time::timeout(JEV_TIMEOUT, call).await.ok().flatten()
}

/// Builds the `POST /v1/systemone` body: a single `Noul` question, `state`
/// JSON-encoded to a string (matching the reference implementation's own
/// `"state": json.dumps(state)` — TypeSafe's API also accepts a raw object,
/// but this keeps parity with the ported source).
fn build_jev_payload(message: &str) -> serde_json::Value {
    let truncated = truncate_message(message);
    let state = serde_json::json!({ "first_message": truncated }).to_string();
    serde_json::json!({
        "state": state,
        "model": TYPESAFE_MODEL,
        "questions": {
            NEEDS_WORKTREE_QUESTION_KEY: {
                "type": "noul",
                "instructions": NEEDS_WORKTREE_INSTRUCTIONS,
                "criteria": {
                    "true": "Completing this task will require modifying files or code in the \
                        repository.",
                    "false": "This is pure investigation, reading, research, or answering a \
                        question — no repository changes are needed.",
                },
            },
        },
    })
}

fn truncate_message(message: &str) -> String {
    if message.chars().count() <= MESSAGE_TRUNCATE_CHARS {
        message.to_string()
    } else {
        message.chars().take(MESSAGE_TRUNCATE_CHARS).collect()
    }
}

/// `answers.needs_worktree.noul` — a flat 0..1 float (see module docs).
fn parse_jev_probability(body: &serde_json::Value) -> Option<f64> {
    let probability = body
        .get("answers")?
        .get(NEEDS_WORKTREE_QUESTION_KEY)?
        .get("noul")?
        .as_f64()?;
    Some(probability.clamp(0.0, 1.0))
}

/// Fallback heuristic (ticket brief, not ported from the reference tool):
/// a ticket-id hit or an imperative code word means "code-modifying" (code
/// words win ties over investigation words); an investigation word with no
/// code word means "investigation"; anything else defaults to `true` — safer
/// to isolate an ambiguous task in its own worktree than to let it touch the
/// base checkout.
pub fn heuristic_needs_worktree(message: &str) -> bool {
    if extract_ticket_id(message).is_some() {
        return true;
    }
    let lower = message.to_lowercase();
    if contains_any_word(&lower, IMPERATIVE_CODE_WORDS) {
        return true;
    }
    if contains_any_word(&lower, INVESTIGATION_WORDS) {
        return false;
    }
    true
}

fn contains_any_word(haystack_lower: &str, words: &[&str]) -> bool {
    let tokens: Vec<&str> = haystack_lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    words.iter().any(|w| tokens.contains(w))
}

// ── CREATE_CHAT_WORKTREE ────────────────────────────────────────────────────

/// `CREATE_CHAT_WORKTREE`'s response: `{worktreePath, branch}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChatWorktree {
    pub worktree_path: String,
    pub branch: String,
}

/// Create a chat worktree at
/// `<superproject-root>/.worktrees/<label>/<slug>` (see module docs) and
/// return where it landed and the branch it was created on. Purely
/// synchronous/blocking (shells out to `git`) — callers run this on a
/// blocking-safe thread (`tokio::task::spawn_blocking`), matching
/// [`crate::pr_ticket_cache`]'s shell-out convention.
pub fn create_chat_worktree(repo_path: &Path, name: &str) -> Result<ChatWorktree, EngineError> {
    let slug = compute_slug(name);
    if slug.is_empty() {
        return Err(EngineError::Other(
            "worktree name sanitizes to an empty slug".into(),
        ));
    }

    let repo_toplevel = git_toplevel(repo_path)
        .ok_or_else(|| EngineError::Other(format!("not a git repository: {}", repo_path.display())))?;
    let (superproject_root, label) = match superproject_working_tree(repo_path) {
        Some(super_root) => {
            let label = repo_toplevel
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "repo".to_string());
            (super_root, label)
        }
        None => (repo_toplevel.clone(), "workspace".to_string()),
    };

    let worktrees_base = worktrees_base_dir(&superproject_root)?;
    assert_outside_submodules(&worktrees_base, &superproject_root, "WORKSPACE_WORKTREES_DIR")?;

    let candidate = normalize_path(&worktrees_base.join(&label).join(&slug));
    assert_outside_submodules(&candidate, &superproject_root, "worktree target")?;
    if candidate.exists() {
        return Err(EngineError::Other(format!(
            "worktree path already exists: {}",
            candidate.display()
        )));
    }

    let default_branch = default_branch(&repo_toplevel)
        .ok_or_else(|| EngineError::Other("could not resolve the repository's default branch".into()))?;

    if let Some(parent) = candidate.parent() {
        std::fs::create_dir_all(parent)?;
    }
    run_git(
        &repo_toplevel,
        &[
            "worktree",
            "add",
            "-b",
            &slug,
            &candidate.to_string_lossy(),
            &default_branch,
        ],
    )
    .map_err(EngineError::Other)?;

    // Plain text, never a symlink (see module docs) — newline-terminated,
    // matching `agent-mode.sh`'s `printf '%s\n' "$REPO_ROOT" > .workspace-root`.
    let mut contents = superproject_root.to_string_lossy().into_owned();
    contents.push('\n');
    std::fs::write(candidate.join(".workspace-root"), contents)?;

    Ok(ChatWorktree {
        worktree_path: candidate.to_string_lossy().into_owned(),
        branch: slug,
    })
}

/// `name` sanitized to a kebab-case slug — the ticket id when `name` itself
/// contains one that `extract_ticket_id` recognizes (preferred over
/// sanitizing the whole free-text name), else a plain kebab-case of `name`.
fn compute_slug(name: &str) -> String {
    if let Some(ticket) = extract_ticket_id(name) {
        return ticket.to_lowercase();
    }
    kebab_case(name)
}

fn kebab_case(input: &str) -> String {
    let mut out = String::new();
    let mut pending_sep = false;
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_sep && !out.is_empty() {
                out.push('-');
            }
            pending_sep = false;
            out.push(ch.to_ascii_lowercase());
        } else {
            pending_sep = true;
        }
    }
    out
}

/// `git rev-parse --show-toplevel` — the git repo (or submodule) `path`
/// itself belongs to. `None` when `path` isn't inside any git working tree.
fn git_toplevel(path: &Path) -> Option<PathBuf> {
    run_git(path, &["rev-parse", "--show-toplevel"])
        .ok()
        .map(|out| PathBuf::from(out.trim()))
}

/// `git rev-parse --show-superproject-working-tree` — non-empty only when
/// `path` sits inside a submodule of some superproject, per the ticket's
/// detection convention.
fn superproject_working_tree(path: &Path) -> Option<PathBuf> {
    run_git(path, &["rev-parse", "--show-superproject-working-tree"])
        .ok()
        .map(|out| out.trim().to_string())
        .filter(|out| !out.is_empty())
        .map(PathBuf::from)
}

/// The repo's current default branch: `origin/HEAD`'s target, else whatever
/// is currently checked out — same fallback order as
/// [`crate::repos::Repos::branches`].
fn default_branch(repo_path: &Path) -> Option<String> {
    if let Ok(short) = run_git(
        repo_path,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    ) {
        if let Some((_, branch)) = short.trim().split_once('/') {
            return Some(branch.to_string());
        }
    }
    run_git(repo_path, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()
        .map(|out| out.trim().to_string())
        .filter(|out| !out.is_empty())
}

/// `WORKSPACE_WORKTREES_DIR` override (same env var `agent-mode.sh` reads —
/// one override covers both tools) or `<superproject_root>/.worktrees`. The
/// override must be an absolute path.
fn worktrees_base_dir(superproject_root: &Path) -> Result<PathBuf, EngineError> {
    match std::env::var("WORKSPACE_WORKTREES_DIR") {
        Ok(dir) if !dir.is_empty() => {
            let dir = PathBuf::from(dir);
            if !dir.is_absolute() {
                return Err(EngineError::Other(
                    "WORKSPACE_WORKTREES_DIR must be an absolute path".into(),
                ));
            }
            Ok(normalize_path(&dir))
        }
        _ => Ok(normalize_path(&superproject_root.join(".worktrees"))),
    }
}

/// Refuse (with a descriptive error) any `candidate` nested under a submodule
/// root read from `superproject_root`'s own `.gitmodules` — the exact guard
/// `agent-mode.sh`'s `assert_worktree_outside_submodules` runs, ported here
/// so `CREATE_CHAT_WORKTREE` can never recreate the recursive-filesystem-loop
/// incident that fix addressed. `what` names the candidate in the error
/// message (the worktree target itself, or an overridden worktrees base).
fn assert_outside_submodules(
    candidate: &Path,
    superproject_root: &Path,
    what: &str,
) -> Result<(), EngineError> {
    for rel in workspace_submodule_paths(superproject_root) {
        let sub_root = normalize_path(&superproject_root.join(&rel));
        if candidate == sub_root || candidate.starts_with(&sub_root) {
            return Err(EngineError::Other(format!(
                "refusing to create a worktree under a submodule root: {} is nested under \
                submodule {} ({what})",
                candidate.display(),
                sub_root.display()
            )));
        }
    }
    Ok(())
}

/// Lexical (string-only) path normalization — resolves `.`/`..` components
/// without touching the filesystem, so it works on a path that doesn't exist
/// yet (the worktree target, before `git worktree add` creates it). Mirrors
/// `agent-mode.sh`'s own `abspath` (also string-normalization-only, for the
/// same reason).
fn normalize_path(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                if !matches!(out.components().next_back(), None | Some(Component::RootDir)) {
                    out.pop();
                }
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("failed to spawn git: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── heuristic classifier ────────────────────────────────────────────

    #[test]
    fn heuristic_true_on_ticket_id_hit() {
        assert!(heuristic_needs_worktree("investigate why eng-2715 is failing"));
    }

    #[test]
    fn heuristic_true_on_imperative_code_words() {
        assert!(heuristic_needs_worktree("please implement the new login flow"));
        assert!(heuristic_needs_worktree("fix the flaky test"));
        assert!(heuristic_needs_worktree("add a retry to the uploader"));
        assert!(heuristic_needs_worktree("refactor the session cache"));
        assert!(heuristic_needs_worktree("build a new settings page"));
        assert!(heuristic_needs_worktree("update the README"));
        assert!(heuristic_needs_worktree("migrate the users table"));
        assert!(heuristic_needs_worktree("write a script to backfill data"));
    }

    #[test]
    fn heuristic_false_on_investigation_words_with_no_code_word() {
        // Deliberately avoids "build" here — it's dual-listed (also an
        // imperative code word), and code words win ties by design; see
        // `heuristic_code_words_win_over_investigation_words`.
        assert!(!heuristic_needs_worktree("why is this endpoint slow?"));
        assert!(!heuristic_needs_worktree("how does the auth flow work"));
        assert!(!heuristic_needs_worktree("investigate the memory leak"));
        assert!(!heuristic_needs_worktree("explain this error message"));
        assert!(!heuristic_needs_worktree("find where this constant is defined"));
        assert!(!heuristic_needs_worktree("look at the logs for this request"));
        assert!(!heuristic_needs_worktree("research alternatives to this library"));
        assert!(!heuristic_needs_worktree("what does this function return"));
    }

    #[test]
    fn heuristic_code_words_win_over_investigation_words() {
        assert!(heuristic_needs_worktree(
            "investigate why this is slow and fix it"
        ));
    }

    #[test]
    fn heuristic_defaults_true_when_ambiguous() {
        assert!(heuristic_needs_worktree("let's talk about the roadmap"));
        assert!(heuristic_needs_worktree(""));
    }

    #[test]
    fn heuristic_word_matching_is_whole_word_not_substring() {
        // "addendum" must not trigger the "add" code word; "howling" must not
        // trigger the "how" investigation word. Checked directly against
        // `contains_any_word` rather than through `heuristic_needs_worktree`,
        // since a message with neither a code nor an investigation word hit
        // falls through to that function's own "default true" branch, which
        // would mask a false-positive substring match here.
        let lower = "addendum to the howling wolves story".to_lowercase();
        assert!(!contains_any_word(&lower, IMPERATIVE_CODE_WORDS));
        assert!(!contains_any_word(&lower, INVESTIGATION_WORDS));

        // A genuine whole-word investigation hit ("explain") still fires
        // alongside the same decoys.
        assert!(!heuristic_needs_worktree(
            "explain the addendum and howling backstory"
        ));
    }

    // ── Jev request/response ────────────────────────────────────────────

    #[test]
    fn build_jev_payload_shape() {
        let payload = build_jev_payload("fix the login bug");
        assert_eq!(payload["model"], serde_json::json!("jev-latest"));
        let state: serde_json::Value =
            serde_json::from_str(payload["state"].as_str().unwrap()).unwrap();
        assert_eq!(state["first_message"], serde_json::json!("fix the login bug"));
        let question = &payload["questions"]["needs_worktree"];
        assert_eq!(question["type"], serde_json::json!("noul"));
        assert!(question["instructions"].as_str().unwrap().contains("modifying files"));
        assert!(question["criteria"]["true"].is_string());
        assert!(question["criteria"]["false"].is_string());
    }

    #[test]
    fn build_jev_payload_truncates_long_messages() {
        let long = "a".repeat(5000);
        let payload = build_jev_payload(&long);
        let state: serde_json::Value =
            serde_json::from_str(payload["state"].as_str().unwrap()).unwrap();
        assert_eq!(
            state["first_message"].as_str().unwrap().len(),
            MESSAGE_TRUNCATE_CHARS
        );
    }

    #[test]
    fn parse_jev_probability_from_canned_response() {
        let body = serde_json::json!({
            "model": "jev-latest",
            "answers": {
                "needs_worktree": {"type": "noul", "noul": 0.87}
            },
            "usage": {"input_tokens": 42, "output_tokens": 3}
        });
        assert_eq!(parse_jev_probability(&body), Some(0.87));
    }

    #[test]
    fn parse_jev_probability_missing_field_is_none() {
        assert_eq!(parse_jev_probability(&serde_json::json!({})), None);
        assert_eq!(
            parse_jev_probability(&serde_json::json!({"answers": {}})),
            None
        );
        assert_eq!(
            parse_jev_probability(&serde_json::json!({"answers": {"needs_worktree": {}}})),
            None
        );
    }

    #[test]
    fn parse_jev_probability_clamps_out_of_range_values() {
        let body = serde_json::json!({"answers": {"needs_worktree": {"noul": 1.5}}});
        assert_eq!(parse_jev_probability(&body), Some(1.0));
        let body = serde_json::json!({"answers": {"needs_worktree": {"noul": -0.5}}});
        assert_eq!(parse_jev_probability(&body), Some(0.0));
    }

    /// Live smoke test against the real TypeSafe endpoint — only runs when a
    /// key is available on this machine. `#[ignore]`: never run in normal CI.
    #[test]
    #[ignore = "requires TYPESAFE_API_KEY and network access"]
    fn live_ask_jev_needs_worktree_runs_end_to_end() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let http = reqwest::Client::new();
            let probability = ask_jev_needs_worktree(&http, "implement a retry queue").await;
            assert!(probability.is_some(), "expected a live TypeSafe answer");
            let p = probability.unwrap();
            assert!((0.0..=1.0).contains(&p));
        });
    }

    // ── wire-shape pins ──────────────────────────────────────────────────

    #[test]
    fn plan_chat_workspace_wire_shape_is_camel_case_with_lowercase_source() {
        let plan = WorkspacePlan {
            needs_worktree: true,
            probability: Some(0.91),
            source: PlanSource::Jev,
        };
        let value = serde_json::to_value(&plan).unwrap();
        assert_eq!(value["needsWorktree"], serde_json::json!(true));
        assert_eq!(value["probability"], serde_json::json!(0.91));
        assert_eq!(value["source"], serde_json::json!("jev"));

        let plan = WorkspacePlan {
            needs_worktree: false,
            probability: None,
            source: PlanSource::Heuristic,
        };
        let value = serde_json::to_value(&plan).unwrap();
        assert_eq!(value["needsWorktree"], serde_json::json!(false));
        assert_eq!(value["probability"], serde_json::Value::Null);
        assert_eq!(value["source"], serde_json::json!("heuristic"));
    }

    #[test]
    fn chat_worktree_wire_shape_is_camel_case() {
        let outcome = ChatWorktree {
            worktree_path: "/tmp/repo/.worktrees/workspace/eng-2715".into(),
            branch: "eng-2715".into(),
        };
        let value = serde_json::to_value(&outcome).unwrap();
        assert_eq!(
            value["worktreePath"],
            serde_json::json!("/tmp/repo/.worktrees/workspace/eng-2715")
        );
        assert_eq!(value["branch"], serde_json::json!("eng-2715"));
    }

    // ── slug sanitization ────────────────────────────────────────────────

    #[test]
    fn kebab_case_sanitizes_free_text() {
        assert_eq!(kebab_case("Fix the Login Bug!!"), "fix-the-login-bug");
        assert_eq!(kebab_case("  leading and trailing  "), "leading-and-trailing");
        assert_eq!(kebab_case("snake_case_name"), "snake-case-name");
        assert_eq!(kebab_case("already-kebab-case"), "already-kebab-case");
        assert_eq!(kebab_case("Multiple   Spaces"), "multiple-spaces");
        assert_eq!(kebab_case(""), "");
        assert_eq!(kebab_case("!!!"), "");
    }

    #[test]
    fn compute_slug_prefers_ticket_id_over_full_name() {
        assert_eq!(compute_slug("ENG-2715: fix the login bug"), "eng-2715");
        assert_eq!(compute_slug("eng-2715-fix-thing"), "eng-2715");
    }

    #[test]
    fn compute_slug_falls_back_to_kebab_case_without_a_ticket_id() {
        assert_eq!(compute_slug("Fix the login bug"), "fix-the-login-bug");
    }

    // ── path normalization ───────────────────────────────────────────────

    #[test]
    fn normalize_path_resolves_dot_and_dotdot_lexically() {
        assert_eq!(
            normalize_path(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
        assert_eq!(normalize_path(Path::new("/a/../../b")), PathBuf::from("/b"));
        assert_eq!(normalize_path(Path::new("/a/b/c")), PathBuf::from("/a/b/c"));
    }

    // ── guard: refuse a target nested under a submodule ─────────────────

    fn init_repo(dir: &Path) {
        run_git(dir, &["init", "-q", "-b", "main"]).unwrap();
        run_git(dir, &["config", "user.email", "t@example.com"]).unwrap();
        run_git(dir, &["config", "user.name", "Test"]).unwrap();
        std::fs::write(dir.join("README.md"), "hello\n").unwrap();
        run_git(dir, &["add", "."]).unwrap();
        run_git(dir, &["commit", "-q", "-m", "init"]).unwrap();
    }

    /// Builds a fake superproject with a real (non-submodule, `.gitmodules`
    /// declared directly) submodule directory — enough to exercise the
    /// guard without a real `git submodule add` network/registration dance.
    fn fake_superproject_with_submodule(root: &Path) -> PathBuf {
        std::fs::create_dir_all(root).unwrap();
        init_repo(root);
        let sub = root.join("ui");
        std::fs::create_dir_all(&sub).unwrap();
        init_repo(&sub);
        std::fs::write(
            root.join(".gitmodules"),
            "[submodule \"ui\"]\n\tpath = ui\n\turl = https://example.invalid/ui.git\n",
        )
        .unwrap();
        sub
    }

    #[test]
    fn guard_rejects_a_target_nested_under_a_submodule_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fake_superproject_with_submodule(&root);

        let nested = root.join("ui").join("nested-worktree");
        let err = assert_outside_submodules(&nested, &root, "worktree target").unwrap_err();
        assert!(err.to_string().contains("submodule"));

        // A sibling of the submodule (not nested under it) is fine.
        assert!(assert_outside_submodules(&root.join(".worktrees").join("workspace").join("x"), &root, "worktree target").is_ok());
    }

    /// `WORKSPACE_WORKTREES_DIR` is process-global state, and Rust runs
    /// `#[test]`s in parallel by default — every test that calls
    /// `create_chat_worktree` (whether or not it itself sets the var) must
    /// hold this lock for its duration, or one test's override can leak into
    /// another's "no override" expectation mid-run (confirmed: this raced
    /// and failed before the lock was added).
    static WORKSPACE_WORKTREES_DIR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn create_chat_worktree_lands_at_root_dot_worktrees_for_a_plain_repo() {
        let _guard = WORKSPACE_WORKTREES_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        unsafe { std::env::remove_var("WORKSPACE_WORKTREES_DIR") };
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(&root).unwrap();
        init_repo(&root);

        let outcome = create_chat_worktree(&root, "Fix the login bug").expect("worktree created");
        assert_eq!(outcome.branch, "fix-the-login-bug");
        let expected = root.join(".worktrees").join("workspace").join("fix-the-login-bug");
        assert_eq!(PathBuf::from(&outcome.worktree_path), expected);
        assert!(expected.join(".git").exists());
        let workspace_root = std::fs::read_to_string(expected.join(".workspace-root")).unwrap();
        assert_eq!(workspace_root.trim_end(), root.to_string_lossy());
        // Never a symlink.
        assert!(!expected.join(".workspace-root").symlink_metadata().unwrap().file_type().is_symlink());
    }

    #[test]
    fn create_chat_worktree_refuses_a_path_that_already_exists() {
        let _guard = WORKSPACE_WORKTREES_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        unsafe { std::env::remove_var("WORKSPACE_WORKTREES_DIR") };
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(&root).unwrap();
        init_repo(&root);
        std::fs::create_dir_all(root.join(".worktrees").join("workspace").join("taken")).unwrap();

        let err = create_chat_worktree(&root, "taken").unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn create_chat_worktree_honors_workspace_worktrees_dir_override() {
        let _guard = WORKSPACE_WORKTREES_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(&root).unwrap();
        init_repo(&root);
        let override_dir = tmp.path().join("elsewhere");

        // SAFETY: test-only env mutation, serialized by this test's own scope
        // (no other test in this module reads this var concurrently in a way
        // that would race observably — matches `worktree_on_run.rs`'s own
        // `ZERON_WORKTREES_DIR` precedent).
        unsafe { std::env::set_var("WORKSPACE_WORKTREES_DIR", &override_dir) };
        let result = create_chat_worktree(&root, "eng-42-do-a-thing");
        unsafe { std::env::remove_var("WORKSPACE_WORKTREES_DIR") };

        let outcome = result.expect("worktree created under override dir");
        assert!(PathBuf::from(&outcome.worktree_path).starts_with(&override_dir));
    }
}
