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

use std::collections::{HashSet, VecDeque};
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
    "implement",
    "fix",
    "add",
    "refactor",
    "build",
    "update",
    "migrate",
    "write",
];
/// Investigation words → `needsWorktree: false`, but only when no code word
/// also matched (code words win ties — see [`heuristic_needs_worktree`]).
const INVESTIGATION_WORDS: &[&str] = &[
    "why",
    "how",
    "investigate",
    "explain",
    "find",
    "look",
    "research",
    "what",
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
pub async fn plan_chat_workspace(
    http: &reqwest::Client,
    message: &str,
    _cwd: &str,
) -> WorkspacePlan {
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
    let api_key = std::env::var("TYPESAFE_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())?;
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

    let repo_toplevel = git_toplevel(repo_path).ok_or_else(|| {
        EngineError::Other(format!("not a git repository: {}", repo_path.display()))
    })?;
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
    assert_outside_submodules(
        &worktrees_base,
        &superproject_root,
        "WORKSPACE_WORKTREES_DIR",
    )?;

    let candidate = normalize_path(&worktrees_base.join(&label).join(&slug));
    assert_outside_submodules(&candidate, &superproject_root, "worktree target")?;
    if candidate.exists() {
        return Err(EngineError::Other(format!(
            "worktree path already exists: {}",
            candidate.display()
        )));
    }

    let default_branch = default_branch(&repo_toplevel).ok_or_else(|| {
        EngineError::Other("could not resolve the repository's default branch".into())
    })?;

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

    // `git worktree add` only materializes TRACKED files, so untracked project
    // config (e.g. `.claude/skills/*`) is missing from the new worktree and
    // Claude Code — which resolves project skills from the worktree's own
    // root — can't see it. Best-effort: a worktree without skills is still
    // usable, so a failure is logged, never propagated.
    let seed_failures = seed_claude_config(&repo_toplevel, &candidate);
    if seed_failures > 0 {
        tracing::warn!(
            failures = seed_failures,
            worktree = %candidate.display(),
            "create_chat_worktree: some .claude entries could not be copied into the new worktree"
        );
    }

    Ok(ChatWorktree {
        worktree_path: candidate.to_string_lossy().into_owned(),
        branch: slug,
    })
}

/// Merge-copy `<src_root>/.claude` into `<dst_root>/.claude`, returning how
/// many entries failed to copy (0 = clean; a missing source `.claude` is a
/// no-op, not a failure). Mirrors `agent-mode.sh`'s `seed_claude_config`:
///
/// - An entry is copied only if it is MISSING in the worktree, recursing into
///   directories that exist on both sides (dirs may be partially tracked), so
///   anything git already materialized (tracked files) always wins.
/// - Plain file copies, never symlinks (same reasoning as `.workspace-root`:
///   a link back into the superproject can make Vite/Vitest loop). Symlinked
///   files are copied by content; symlinked directories are skipped.
/// - Any entry whose name matches `*.local.*` (e.g. `settings.local.json`),
///   at any depth, is skipped: local settings can carry machine- or
///   checkout-specific permission grants that shouldn't multiply into every
///   worktree. Skills, agents, commands and `settings.json` are wanted.
fn seed_claude_config(src_root: &Path, dst_root: &Path) -> usize {
    let src = src_root.join(".claude");
    if !src.is_dir() {
        return 0;
    }
    let mut failures = 0;
    merge_claude_dir(&src, &dst_root.join(".claude"), &mut failures);
    failures
}

fn merge_claude_dir(src: &Path, dst: &Path, failures: &mut usize) {
    if let Err(err) = std::fs::create_dir_all(dst) {
        tracing::warn!(path = %dst.display(), error = %err, "seed .claude: cannot create directory");
        *failures += 1;
        return;
    }
    let entries = match std::fs::read_dir(src) {
        Ok(entries) => entries,
        Err(err) => {
            tracing::warn!(path = %src.display(), error = %err, "seed .claude: cannot read directory");
            *failures += 1;
            return;
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().contains(".local.") {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        // Follows symlinks: a symlinked file is copied by content.
        let Ok(meta) = std::fs::metadata(&from) else {
            continue; // dangling symlink
        };
        let is_link = entry.file_type().map(|t| t.is_symlink()).unwrap_or(false);
        if meta.is_dir() {
            if is_link {
                continue; // never follow directory symlinks (loop risk)
            }
            // A tracked non-directory at this path wins; don't touch it.
            if to.symlink_metadata().is_ok() && !to.is_dir() {
                continue;
            }
            merge_claude_dir(&from, &to, failures);
        } else if meta.is_file() {
            if to.symlink_metadata().is_ok() {
                continue; // already materialized by git: never overwrite
            }
            if let Err(err) = std::fs::copy(&from, &to) {
                tracing::warn!(path = %from.display(), error = %err, "seed .claude: copy failed");
                *failures += 1;
            }
        }
    }
}

// ── PLAN_CHAT_CLOSEOUT / CLOSE_CHAT_WORKTREE ────────────────────────────────

/// Cap on `PLAN_CHAT_CLOSEOUT`'s `dirtyFiles` list — the UI shows a preview,
/// not an exhaustive status; `dirty` itself is exact.
const DIRTY_FILES_CAP: usize = 20;
/// Close-out scans embedded repositories as well as the outer checkout. Keep
/// recursive repository discovery bounded, but never return a partial clean
/// result if that bound is reached.
const NESTED_REPO_SCAN_MAX_DEPTH: usize = 12;
const NESTED_REPO_SCAN_MAX_REPOS: usize = 128;
/// The plain-text marker `create_chat_worktree` writes into Zeron-created
/// worktrees. It is not required for close-out: Repo Map also exposes linked
/// worktrees created by other tools. Being untracked by construction, the
/// marker is never counted as "dirty" and is deleted before a non-forced
/// `git worktree remove` (which would otherwise refuse on it).
const WORKSPACE_ROOT_MARKER: &str = ".workspace-root";

/// `PLAN_CHAT_CLOSEOUT`'s response. Wire field names are pinned by
/// [`tests::closeout_plan_wire_shape_is_camel_case`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CloseoutPlan {
    /// `cwd` is a registered linked git worktree (never a repo's main working
    /// tree). When `false` the remaining fields are empty/null/0.
    pub is_worktree: bool,
    pub worktree_path: String,
    /// Checked-out branch, `null` when detached.
    pub branch: Option<String>,
    pub chat_live: bool,
    pub dirty: bool,
    /// Up to [`DIRTY_FILES_CAP`] `git status --porcelain` entries.
    pub dirty_files: Vec<String>,
    /// Non-null when Git could not completely inspect the outer worktree or
    /// one of its initialized nested repositories. Close-out fails closed in
    /// this state, even when `force` is requested.
    pub dirty_inspection_error: Option<String>,
    pub unmerged_commits: u32,
    /// Non-null when reachability against the default branch could not be
    /// established. A non-forced close is refused in this state.
    pub merge_inspection_error: Option<String>,
    pub default_branch: Option<String>,
}

/// `CLOSE_CHAT_WORKTREE`'s response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CloseoutOutcome {
    pub removed: bool,
    pub branch_deleted: bool,
    pub archived: bool,
}

/// Everything `plan_chat_closeout`/`close_chat_worktree` learn about a
/// candidate worktree in one pass.
struct Inspection {
    plan: CloseoutPlan,
    /// Working tree of the repo that owns this worktree — where every
    /// mutating git command must run. `None` when not a closeable worktree.
    repo: Option<PathBuf>,
    /// Canonicalized worktree path (for comparisons against git's output).
    canonical: Option<PathBuf>,
    /// Exact uncommitted-entry count (`plan.dirty_files` is capped).
    dirty_count: usize,
    /// Set when any dirty-status command or nested-repository discovery
    /// failed. This is a hard refusal: force may acknowledge known dirty
    /// files, but it cannot safely acknowledge files we failed to inspect.
    dirty_inspection_error: Option<String>,
    /// Set when `rev-list` failed although a branch and default both exist —
    /// `close_chat_worktree` fails closed on this; the plan reports 0.
    unmerged_error: Option<String>,
    /// Why `plan.is_worktree` is false (for close's error message).
    reject_reason: Option<String>,
}

fn not_a_worktree(worktree_path: String, chat_live: bool, reason: String) -> Inspection {
    Inspection {
        plan: CloseoutPlan {
            is_worktree: false,
            worktree_path,
            branch: None,
            chat_live,
            dirty: false,
            dirty_files: Vec::new(),
            dirty_inspection_error: None,
            unmerged_commits: 0,
            merge_inspection_error: None,
            default_branch: None,
        },
        repo: None,
        canonical: None,
        dirty_count: 0,
        dirty_inspection_error: None,
        unmerged_error: None,
        reject_reason: Some(reason),
    }
}

/// Read-only inspection of `cwd` for chat close-out. Blocking (shells out to
/// `git`) — run on a blocking-safe thread. `chat_live` is supplied by the
/// caller (the engine's session/process liveness lives outside this module).
pub fn plan_chat_closeout(cwd: &Path, chat_live: bool) -> CloseoutPlan {
    inspect_worktree(cwd, chat_live).plan
}

fn inspect_worktree(cwd: &Path, chat_live: bool) -> Inspection {
    let worktree_path = normalize_path(cwd).to_string_lossy().into_owned();
    let reject =
        |reason: &str| not_a_worktree(worktree_path.clone(), chat_live, reason.to_string());

    let Ok(canonical) = cwd.canonicalize() else {
        return reject("the path cannot be resolved");
    };
    // `cwd` must itself be the worktree root, not a subdirectory of one.
    match git_toplevel(&canonical).and_then(|t| t.canonicalize().ok()) {
        Some(top) if top == canonical => {}
        _ => return reject("it is not the root of a git working tree"),
    }
    let (Some(git_dir), Some(common_dir)) = (
        git_path(&canonical, "--git-dir"),
        git_path(&canonical, "--git-common-dir"),
    ) else {
        return reject("its git directory could not be resolved");
    };
    if git_dir == common_dir {
        return reject("it is a repository's main working tree, not a linked worktree");
    }
    // `<repo>/.git` → `<repo>`; a bare repo's common dir is the repo itself.
    let repo = if common_dir.file_name().is_some_and(|n| n == ".git") {
        common_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or(common_dir.clone())
    } else {
        common_dir.clone()
    };
    let registered = run_git(&repo, &["worktree", "list", "--porcelain"])
        .map(|out| parse_worktree_list(&out))
        .unwrap_or_default();
    // First entry is the main working tree; it must never be closeable.
    let is_registered_linked = registered
        .iter()
        .skip(1)
        .filter_map(|p| p.canonicalize().ok())
        .any(|p| p == canonical);
    if !is_registered_linked {
        return reject("it is not a registered linked git worktree");
    }

    let branch = run_git(&canonical, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()
        .map(|out| out.trim().to_string())
        .filter(|out| !out.is_empty());
    // Resolved from the owning repo — from inside the linked worktree the
    // `HEAD` fallback would just return the worktree's own branch.
    let default = default_branch(&repo);

    let (dirty_entries, dirty_inspection_error) = match inspect_dirty_repositories(&canonical) {
        Ok(entries) => (entries, None),
        Err(error) => (Vec::new(), Some(error)),
    };
    let dirty_count = dirty_entries.len();

    let (unmerged_commits, unmerged_error) = match (&branch, &default) {
        (Some(b), Some(d)) if b != d => match count_unmerged(&canonical, d, b) {
            Ok(n) => (n, None),
            Err(e) => (0, Some(e)),
        },
        // A detached HEAD can still contain commits that are reachable from
        // no branch. Compare that exact worktree HEAD against the default;
        // using `repo` here would inspect the main checkout's HEAD instead.
        (None, Some(d)) => match count_unmerged(&canonical, d, "HEAD") {
            Ok(n) => (n, None),
            Err(e) => (0, Some(e)),
        },
        (Some(branch), None) => (
            0,
            Some(format!(
                "could not resolve the default branch to verify `{branch}` is merged"
            )),
        ),
        (None, None) => (
            0,
            Some("could not resolve the default branch to verify detached HEAD is merged".into()),
        ),
        _ => (0, None),
    };

    Inspection {
        plan: CloseoutPlan {
            is_worktree: true,
            worktree_path,
            branch,
            chat_live,
            dirty: dirty_count > 0,
            dirty_files: dirty_entries.into_iter().take(DIRTY_FILES_CAP).collect(),
            dirty_inspection_error: dirty_inspection_error.clone(),
            unmerged_commits,
            merge_inspection_error: unmerged_error.clone(),
            default_branch: default,
        },
        repo: Some(repo),
        canonical: Some(canonical),
        dirty_count,
        dirty_inspection_error,
        unmerged_error,
        reject_reason: None,
    }
}

/// Collect dirty paths from the outer checkout and every initialized nested
/// Git repository beneath it. Git normally collapses a dirty submodule to a
/// single gitlink entry (for example `flagship`); expanding the nested status
/// here gives the close-out warning the actual paths that would be deleted.
fn inspect_dirty_repositories(root: &Path) -> Result<Vec<String>, String> {
    let nested = discover_nested_git_repositories(root)?;
    let mut nested_dirty_roots = HashSet::new();
    let mut nested_entries = Vec::new();

    // Deepest first is not required for correctness, but makes the result
    // deterministic when repositories themselves contain repositories.
    let mut nested = nested;
    nested.sort_by(|left, right| {
        right
            .components()
            .count()
            .cmp(&left.components().count())
            .then_with(|| left.cmp(right))
    });
    for repo in nested {
        let relative = repo
            .strip_prefix(root)
            .map_err(|_| format!("nested repository escaped the worktree: {}", repo.display()))?;
        let entries = dirty_entries_for_repo(&repo, false)?;
        if !entries.is_empty() {
            nested_dirty_roots.insert(relative.to_path_buf());
        }
        nested_entries.extend(
            entries
                .into_iter()
                .map(|path| relative.join(path).to_string_lossy().into_owned()),
        );
    }

    let mut entries = dirty_entries_for_repo(root, true)?;
    // When a nested repository is dirty, replace Git's opaque parent gitlink
    // marker with its expanded paths. A clean nested repo whose checked-out
    // commit differs from the recorded gitlink remains visible at the parent.
    entries.retain(|entry| {
        let normalized = entry.trim_end_matches('/');
        !nested_dirty_roots
            .iter()
            .any(|path| path.to_string_lossy() == normalized)
    });
    entries.extend(nested_entries);
    entries.sort();
    entries.dedup();
    Ok(entries)
}

fn dirty_entries_for_repo(
    repo: &Path,
    ignore_workspace_marker: bool,
) -> Result<Vec<String>, String> {
    let output = run_git(repo, &["status", "--porcelain", "-uall"]).map_err(|error| {
        format!(
            "could not inspect dirty files in {}: {error}",
            repo.display()
        )
    })?;
    Ok(output
        .lines()
        .filter_map(|line| line.get(3..))
        .filter(|path| !(ignore_workspace_marker && *path == WORKSPACE_ROOT_MARKER))
        // Untracked `.claude/` content is (mostly) what `seed_claude_config`
        // copied in at creation; counting it would make every fresh worktree
        // read as dirty. Apply the same convention in nested repos.
        .filter(|path| !path.starts_with(".claude/"))
        .map(str::to_string)
        .collect())
}

/// Repository-aware discovery for initialized embedded repositories. Declared
/// submodules are followed recursively. Ordinary embedded repositories are
/// found from Git's own status entries (Git reports an untracked embedded repo
/// as one directory), avoiding a full source-tree walk on large monorepos.
fn discover_nested_git_repositories(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut found = Vec::new();
    let mut queue = VecDeque::from([(root.to_path_buf(), 0usize)]);
    let mut seen = HashSet::from([root.to_path_buf()]);

    while let Some((directory, depth)) = queue.pop_front() {
        if depth >= NESTED_REPO_SCAN_MAX_DEPTH {
            return Err(format!(
                "nested repository scan exceeded depth {NESTED_REPO_SCAN_MAX_DEPTH}"
            ));
        }

        let status = run_git(&directory, &["status", "--porcelain", "-uall"]).map_err(|error| {
            format!(
                "could not inspect nested repositories in {}: {error}",
                directory.display()
            )
        })?;
        let mut candidates: Vec<PathBuf> = workspace_submodule_paths(&directory)
            .into_iter()
            .map(|relative| directory.join(relative))
            .collect();
        candidates.extend(
            status
                .lines()
                .filter_map(|line| line.get(3..))
                .map(|path| directory.join(path.trim_end_matches('/'))),
        );

        for candidate in candidates {
            if std::fs::symlink_metadata(candidate.join(".git")).is_err() {
                continue;
            }
            let canonical = candidate.canonicalize().map_err(|error| {
                format!(
                    "could not resolve nested repository {}: {error}",
                    candidate.display()
                )
            })?;
            if !canonical.starts_with(root) {
                return Err(format!(
                    "nested repository resolves outside the worktree: {}",
                    candidate.display()
                ));
            }
            if !seen.insert(canonical.clone()) {
                continue;
            }
            if found.len() >= NESTED_REPO_SCAN_MAX_REPOS {
                return Err(format!(
                    "nested repository scan exceeded {NESTED_REPO_SCAN_MAX_REPOS} repositories"
                ));
            }
            found.push(canonical.clone());
            queue.push_back((canonical, depth + 1));
        }
    }
    Ok(found)
}

/// `git rev-list --count <default>..<branch>`.
fn count_unmerged(repo: &Path, default: &str, branch: &str) -> Result<u32, String> {
    let range = format!("{default}..{branch}");
    run_git(repo, &["rev-list", "--count", &range])?
        .trim()
        .parse()
        .map_err(|e| format!("could not parse commit count for {range}: {e}"))
}

/// Absolute, canonical form of a `git rev-parse <flag>` path (git prints
/// these relative to the cwd in some layouts).
fn git_path(cwd: &Path, flag: &str) -> Option<PathBuf> {
    let out = run_git(cwd, &["rev-parse", flag]).ok()?;
    let p = PathBuf::from(out.trim());
    let p = if p.is_absolute() { p } else { cwd.join(p) };
    p.canonicalize().ok()
}

/// Paths from `git worktree list --porcelain`, main working tree first.
fn parse_worktree_list(porcelain: &str) -> Vec<PathBuf> {
    porcelain
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .map(PathBuf::from)
        .collect()
}

fn plural(n: usize, singular: &str) -> String {
    format!("{n} {singular}{}", if n == 1 { "" } else { "s" })
}

fn validate_closeout_inspection(inspection: &Inspection, force: bool) -> Result<(), EngineError> {
    let refuse = |msg: String| Err(EngineError::Other(msg));
    let (Some(repo), Some(canonical)) = (&inspection.repo, &inspection.canonical) else {
        return refuse(format!(
            "not a closeable linked worktree: {}: {}",
            inspection.plan.worktree_path,
            inspection
                .reject_reason
                .as_deref()
                .unwrap_or("unknown reason")
        ));
    };
    if canonical == repo {
        return refuse(format!(
            "refusing to remove a repository's main working tree: {}",
            canonical.display()
        ));
    }
    let plan = &inspection.plan;
    if let (Some(branch), Some(default)) = (&plan.branch, &plan.default_branch) {
        if branch == default {
            return refuse(format!(
                "refusing to close out: worktree is on the repository's default branch `{default}`"
            ));
        }
    }
    if let Some(error) = &inspection.dirty_inspection_error {
        return refuse(format!(
            "refusing to close out because dirty files could not be inspected: {error}"
        ));
    }

    if !force {
        if inspection.dirty_count > 0 {
            return refuse(format!(
                "{} in the worktree",
                plural(inspection.dirty_count, "uncommitted file")
            ));
        }
        if let Some(err) = &inspection.unmerged_error {
            return refuse(format!("could not verify the branch is merged: {err}"));
        }
        if plan.unmerged_commits > 0 {
            return refuse(format!(
                "{} on {}",
                plural(plan.unmerged_commits as usize, "unmerged commit"),
                plan.branch.as_deref().unwrap_or("HEAD")
            ));
        }
    }
    Ok(())
}

/// Tear down a linked worktree: `git worktree remove`, then delete the
/// worktree's own branch. `force` overrides only the SOFT refusals (dirty
/// tree, unmerged commits); the HARD ones (live chat, not a closeable
/// worktree, default branch, main working tree) always error. Blocking — run
/// on a blocking-safe thread. The returned outcome has `archived: false`;
/// archiving is the RPC layer's job (it owns the workspace doc).
pub fn close_chat_worktree(
    cwd: &Path,
    force: bool,
    chat_live: bool,
) -> Result<CloseoutOutcome, EngineError> {
    if chat_live {
        return Err(EngineError::Other(
            "chat is live; stop its session before closing out the worktree".into(),
        ));
    }
    let first_inspection = inspect_worktree(cwd, chat_live);
    validate_closeout_inspection(&first_inspection, force)?;

    // Planning and confirmation can race with an agent or editor. Inspect a
    // second time at the destructive boundary and authorize removal only from
    // this fresh result. Even forced removal cannot bypass an unknown status.
    let inspection = inspect_worktree(cwd, chat_live);
    validate_closeout_inspection(&inspection, force)?;
    if first_inspection.canonical != inspection.canonical
        || first_inspection.repo != inspection.repo
    {
        return Err(EngineError::Other(
            "refusing to close out because the worktree identity changed during inspection".into(),
        ));
    }
    let repo = inspection.repo.as_ref().expect("validated repo");
    let canonical = inspection.canonical.as_ref().expect("validated worktree");
    let plan = &inspection.plan;

    // The marker is untracked, so git would refuse to remove a worktree
    // holding it; take it out first (restored below if the removal fails).
    let marker = canonical.join(WORKSPACE_ROOT_MARKER);
    let marker_contents = std::fs::read(&marker).ok();
    if !force {
        let _ = std::fs::remove_file(&marker);
    }
    let path_arg = canonical.to_string_lossy().into_owned();
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&path_arg);
    if let Err(err) = run_git(repo, &args) {
        if let (false, Some(bytes)) = (force, marker_contents) {
            let _ = std::fs::write(&marker, bytes);
        }
        return Err(EngineError::Other(err));
    }

    // Only the worktree's own checked-out branch, and never the default
    // (guarded above, re-checked here so the invariant is local).
    let mut branch_deleted = false;
    if let Some(branch) = plan.branch.as_deref() {
        if plan.default_branch.as_deref() != Some(branch) {
            let flag = if force { "-D" } else { "-d" };
            match run_git(repo, &["branch", flag, branch]) {
                Ok(_) => branch_deleted = true,
                // The worktree is already gone; don't fail the whole close-out
                // over a branch git wants kept (e.g. `-d` not merged into the
                // main repo's current HEAD). Reported via `branchDeleted`.
                Err(err) => tracing::warn!(branch, error = %err, "close-out: branch not deleted"),
            }
        }
    }

    Ok(CloseoutOutcome {
        removed: true,
        branch_deleted,
        archived: false,
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
                if !matches!(
                    out.components().next_back(),
                    None | Some(Component::RootDir)
                ) {
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
        assert!(heuristic_needs_worktree(
            "investigate why eng-2715 is failing"
        ));
    }

    #[test]
    fn heuristic_true_on_imperative_code_words() {
        assert!(heuristic_needs_worktree(
            "please implement the new login flow"
        ));
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
        assert!(!heuristic_needs_worktree(
            "find where this constant is defined"
        ));
        assert!(!heuristic_needs_worktree(
            "look at the logs for this request"
        ));
        assert!(!heuristic_needs_worktree(
            "research alternatives to this library"
        ));
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
        assert_eq!(
            state["first_message"],
            serde_json::json!("fix the login bug")
        );
        let question = &payload["questions"]["needs_worktree"];
        assert_eq!(question["type"], serde_json::json!("noul"));
        assert!(
            question["instructions"]
                .as_str()
                .unwrap()
                .contains("modifying files")
        );
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
        assert_eq!(
            kebab_case("  leading and trailing  "),
            "leading-and-trailing"
        );
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
        assert!(
            assert_outside_submodules(
                &root.join(".worktrees").join("workspace").join("x"),
                &root,
                "worktree target"
            )
            .is_ok()
        );
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
        let expected = root
            .join(".worktrees")
            .join("workspace")
            .join("fix-the-login-bug");
        assert_eq!(PathBuf::from(&outcome.worktree_path), expected);
        assert!(expected.join(".git").exists());
        let workspace_root = std::fs::read_to_string(expected.join(".workspace-root")).unwrap();
        assert_eq!(workspace_root.trim_end(), root.to_string_lossy());
        // Never a symlink.
        assert!(
            !expected
                .join(".workspace-root")
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    /// Plain repo (as `init_repo` builds) plus: tracked `.claude/agents/x.md`,
    /// tracked `.claude/skills/tracked/SKILL.md`, then untracked
    /// `.claude/skills/y/SKILL.md`, an untracked sibling inside the tracked
    /// skills dir, and local settings at two depths.
    fn repo_with_claude_config(root: &Path) {
        init_repo(root);
        let claude = root.join(".claude");
        std::fs::create_dir_all(claude.join("agents")).unwrap();
        std::fs::create_dir_all(claude.join("skills/tracked")).unwrap();
        std::fs::write(claude.join("agents/x.md"), "git agent\n").unwrap();
        std::fs::write(claude.join("skills/tracked/SKILL.md"), "git skill\n").unwrap();
        run_git(root, &["add", ".claude"]).unwrap();
        run_git(root, &["commit", "-q", "-m", "claude config"]).unwrap();
        // Dirty the working copy of a tracked file: git's version must win.
        std::fs::write(claude.join("agents/x.md"), "locally edited agent\n").unwrap();
        std::fs::create_dir_all(claude.join("skills/y")).unwrap();
        std::fs::write(claude.join("skills/y/SKILL.md"), "untracked skill\n").unwrap();
        std::fs::write(
            claude.join("skills/tracked/extra.md"),
            "untracked sibling\n",
        )
        .unwrap();
        std::fs::write(claude.join("settings.local.json"), "{}\n").unwrap();
        std::fs::write(claude.join("skills/y/notes.local.md"), "local\n").unwrap();
    }

    #[test]
    fn create_chat_worktree_seeds_untracked_claude_config() {
        let _guard = WORKSPACE_WORKTREES_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        unsafe { std::env::remove_var("WORKSPACE_WORKTREES_DIR") };
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        repo_with_claude_config(&root);

        let outcome = create_chat_worktree(&root, "seed claude").expect("worktree created");
        let wt = PathBuf::from(&outcome.worktree_path);
        let claude = wt.join(".claude");

        // Tracked files: git's content, not the source's dirty working copy.
        assert_eq!(
            std::fs::read_to_string(claude.join("agents/x.md")).unwrap(),
            "git agent\n"
        );
        assert_eq!(
            std::fs::read_to_string(claude.join("skills/tracked/SKILL.md")).unwrap(),
            "git skill\n"
        );
        // Untracked skills arrive, including into a partially tracked dir.
        assert_eq!(
            std::fs::read_to_string(claude.join("skills/y/SKILL.md")).unwrap(),
            "untracked skill\n"
        );
        assert_eq!(
            std::fs::read_to_string(claude.join("skills/tracked/extra.md")).unwrap(),
            "untracked sibling\n"
        );
        // `*.local.*` excluded at any depth.
        assert!(!claude.join("settings.local.json").exists());
        assert!(!claude.join("skills/y/notes.local.md").exists());
        // Plain files, never symlinks.
        assert!(
            !claude
                .join("skills/y/SKILL.md")
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        // The seeded (untracked) config doesn't make the fresh worktree dirty.
        assert!(!plan_chat_closeout(&wt, false).dirty);
    }

    #[test]
    fn create_chat_worktree_succeeds_without_a_source_claude_dir() {
        let _guard = WORKSPACE_WORKTREES_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        unsafe { std::env::remove_var("WORKSPACE_WORKTREES_DIR") };
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        init_repo(&root);

        let outcome = create_chat_worktree(&root, "no claude").expect("worktree created");
        assert!(
            !PathBuf::from(outcome.worktree_path)
                .join(".claude")
                .exists()
        );
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

    // ── PLAN_CHAT_CLOSEOUT / CLOSE_CHAT_WORKTREE ─────────────────────────

    #[test]
    fn closeout_plan_wire_shape_is_camel_case() {
        let plan = CloseoutPlan {
            is_worktree: true,
            worktree_path: "/r/.worktrees/workspace/eng-1".into(),
            branch: Some("eng-1".into()),
            chat_live: false,
            dirty: true,
            dirty_files: vec!["a.txt".into()],
            dirty_inspection_error: None,
            unmerged_commits: 3,
            merge_inspection_error: None,
            default_branch: Some("main".into()),
        };
        let value = serde_json::to_value(&plan).unwrap();
        let obj = value.as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "branch",
                "chatLive",
                "defaultBranch",
                "dirty",
                "dirtyFiles",
                "dirtyInspectionError",
                "isWorktree",
                "mergeInspectionError",
                "unmergedCommits",
                "worktreePath"
            ]
        );
        assert_eq!(value["isWorktree"], serde_json::json!(true));
        assert_eq!(
            value["worktreePath"],
            serde_json::json!("/r/.worktrees/workspace/eng-1")
        );
        assert_eq!(value["branch"], serde_json::json!("eng-1"));
        assert_eq!(value["chatLive"], serde_json::json!(false));
        assert_eq!(value["dirty"], serde_json::json!(true));
        assert_eq!(value["dirtyFiles"], serde_json::json!(["a.txt"]));
        assert_eq!(value["dirtyInspectionError"], serde_json::Value::Null);
        assert_eq!(value["unmergedCommits"], serde_json::json!(3));
        assert_eq!(value["mergeInspectionError"], serde_json::Value::Null);
        assert_eq!(value["defaultBranch"], serde_json::json!("main"));

        let detached = CloseoutPlan {
            branch: None,
            default_branch: None,
            ..plan
        };
        let value = serde_json::to_value(&detached).unwrap();
        assert_eq!(value["branch"], serde_json::Value::Null);
        assert_eq!(value["defaultBranch"], serde_json::Value::Null);
    }

    #[test]
    fn closeout_outcome_wire_shape_is_camel_case() {
        let value = serde_json::to_value(CloseoutOutcome {
            removed: true,
            branch_deleted: true,
            archived: false,
        })
        .unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["archived", "branchDeleted", "removed"]);
        assert_eq!(value["removed"], serde_json::json!(true));
        assert_eq!(value["branchDeleted"], serde_json::json!(true));
        assert_eq!(value["archived"], serde_json::json!(false));
    }

    /// Fresh plain repo + one chat worktree (`name` → branch/slug). Holds the
    /// env lock for the duration of the creation only; the returned tempdir
    /// keeps everything alive.
    fn repo_with_worktree(name: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let _guard = WORKSPACE_WORKTREES_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        unsafe { std::env::remove_var("WORKSPACE_WORKTREES_DIR") };
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        init_repo(&root);
        let wt = create_chat_worktree(&root, name).expect("worktree created");
        (tmp, root, PathBuf::from(wt.worktree_path))
    }

    fn commit_file(dir: &Path, file: &str) {
        std::fs::write(dir.join(file), "x\n").unwrap();
        run_git(dir, &["add", file]).unwrap();
        run_git(
            dir,
            &[
                "-c",
                "user.email=t@example.com",
                "-c",
                "user.name=Test",
                "commit",
                "-q",
                "-m",
                file,
            ],
        )
        .unwrap();
    }

    #[test]
    fn plan_clean_merged_worktree_is_closeable() {
        let (_tmp, _root, wt) = repo_with_worktree("eng-7-clean");
        let plan = plan_chat_closeout(&wt, false);
        assert!(plan.is_worktree);
        assert_eq!(plan.worktree_path, wt.to_string_lossy());
        assert_eq!(plan.branch.as_deref(), Some("eng-7"));
        assert_eq!(plan.default_branch.as_deref(), Some("main"));
        assert!(!plan.chat_live);
        // The `.workspace-root` marker is untracked but must not read as dirty.
        assert!(!plan.dirty);
        assert!(plan.dirty_files.is_empty());
        assert_eq!(plan.unmerged_commits, 0);
    }

    #[test]
    fn plan_reports_chat_live_as_given() {
        let (_tmp, _root, wt) = repo_with_worktree("live-flag");
        assert!(plan_chat_closeout(&wt, true).chat_live);
    }

    #[test]
    fn plan_dirty_worktree_lists_files_capped() {
        let (_tmp, _root, wt) = repo_with_worktree("dirty-one");
        std::fs::write(wt.join("README.md"), "changed\n").unwrap();
        std::fs::write(wt.join("new.txt"), "n\n").unwrap();
        let plan = plan_chat_closeout(&wt, false);
        assert!(plan.dirty);
        assert_eq!(plan.dirty_files.len(), 2);
        assert!(plan.dirty_files.contains(&"README.md".to_string()));
        assert!(plan.dirty_files.contains(&"new.txt".to_string()));

        for i in 0..30 {
            std::fs::write(wt.join(format!("bulk-{i:02}.txt")), "b\n").unwrap();
        }
        let plan = plan_chat_closeout(&wt, false);
        assert_eq!(plan.dirty_files.len(), DIRTY_FILES_CAP);
    }

    fn repo_with_initialized_submodule_worktree(
        name: &str,
    ) -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let _guard = WORKSPACE_WORKTREES_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        unsafe { std::env::remove_var("WORKSPACE_WORKTREES_DIR") };
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("flagship-source");
        std::fs::create_dir_all(&source).unwrap();
        init_repo(&source);

        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        init_repo(&root);
        run_git(
            &root,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                source.to_str().unwrap(),
                "flagship",
            ],
        )
        .unwrap();
        run_git(&root, &["commit", "-q", "-am", "add flagship"]).unwrap();

        let outcome = create_chat_worktree(&root, name).expect("worktree created");
        let wt = PathBuf::from(outcome.worktree_path);
        run_git(
            &wt,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "-q",
                "--init",
                "flagship",
            ],
        )
        .unwrap();
        let nested = wt.join("flagship");
        (tmp, root, wt, nested)
    }

    #[test]
    fn plan_expands_dirty_submodule_to_prefixed_files() {
        let (_tmp, _root, wt, nested) = repo_with_initialized_submodule_worktree("nested-dirty");
        std::fs::write(nested.join("README.md"), "changed\n").unwrap();
        std::fs::write(nested.join("new.rs"), "new\n").unwrap();

        let plan = plan_chat_closeout(&wt, false);
        assert!(plan.dirty);
        assert_eq!(plan.dirty_inspection_error, None);
        assert!(plan.dirty_files.contains(&"flagship/README.md".to_string()));
        assert!(plan.dirty_files.contains(&"flagship/new.rs".to_string()));
        assert!(
            !plan.dirty_files.contains(&"flagship".to_string()),
            "opaque gitlink marker should be replaced by actual nested paths: {:?}",
            plan.dirty_files
        );

        let err = close_err(&wt, false);
        assert!(err.contains("2 uncommitted files"), "{err}");
        assert!(wt.exists());
    }

    #[test]
    fn dirty_inspection_failure_is_reported_and_force_cannot_bypass_it() {
        let (_tmp, _root, wt) = repo_with_worktree("broken-index");
        let index = run_git(&wt, &["rev-parse", "--git-path", "index"]).unwrap();
        let index = PathBuf::from(index.trim());
        std::fs::write(&index, "not a git index\n").unwrap();

        let plan = plan_chat_closeout(&wt, false);
        assert!(plan.is_worktree);
        assert!(plan.dirty_inspection_error.is_some(), "{plan:?}");
        let err = close_err(&wt, true);
        assert!(err.contains("dirty files could not be inspected"), "{err}");
        assert!(wt.exists(), "unknown dirty state must never be removed");
        assert!(wt.join(WORKSPACE_ROOT_MARKER).exists());
    }

    #[test]
    fn plan_counts_unmerged_commits() {
        let (_tmp, _root, wt) = repo_with_worktree("ahead");
        commit_file(&wt, "a.txt");
        commit_file(&wt, "b.txt");
        let plan = plan_chat_closeout(&wt, false);
        assert!(plan.is_worktree);
        assert!(!plan.dirty);
        assert_eq!(plan.unmerged_commits, 2);
    }

    #[test]
    fn plan_detached_worktree_counts_commits_unique_from_default() {
        let (_tmp, _root, wt) = repo_with_worktree("detach-me");
        run_git(&wt, &["checkout", "-q", "--detach"]).unwrap();
        commit_file(&wt, "a.txt");
        let plan = plan_chat_closeout(&wt, false);
        assert!(plan.is_worktree);
        assert_eq!(plan.branch, None);
        assert_eq!(plan.unmerged_commits, 1);
        assert_eq!(plan.merge_inspection_error, None);
        let err = close_err(&wt, false);
        assert!(err.contains("1 unmerged commit on HEAD"), "{err}");
        assert!(wt.exists());
    }

    #[test]
    fn detached_worktree_without_resolvable_default_requires_force() {
        let (_tmp, root, wt) = repo_with_worktree("detached-unknown-default");
        run_git(&root, &["checkout", "-q", "--detach"]).unwrap();
        run_git(&wt, &["checkout", "-q", "--detach"]).unwrap();

        let plan = plan_chat_closeout(&wt, false);
        assert_eq!(plan.branch, None);
        assert_eq!(plan.default_branch, None);
        assert!(plan.merge_inspection_error.is_some(), "{plan:?}");
        let err = close_err(&wt, false);
        assert!(
            err.contains("could not verify the branch is merged"),
            "{err}"
        );
        assert!(wt.exists());
    }

    #[test]
    fn plan_non_worktree_cwd_is_not_a_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let plain = tmp.path().canonicalize().unwrap().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let plan = plan_chat_closeout(&plain, false);
        assert!(!plan.is_worktree);
        assert!(plan.dirty_files.is_empty());
        assert_eq!(plan.unmerged_commits, 0);
        assert_eq!(plan.branch, None);

        // Missing path entirely.
        assert!(!plan_chat_closeout(&plain.join("nope"), false).is_worktree);
    }

    #[test]
    fn plan_main_working_tree_is_not_a_worktree_even_with_marker() {
        let (_tmp, root, _wt) = repo_with_worktree("has-sibling");
        // A stray marker in the main checkout must not make it closeable.
        std::fs::write(root.join(".workspace-root"), "x\n").unwrap();
        assert!(!plan_chat_closeout(&root, false).is_worktree);
    }

    #[test]
    fn plan_linked_worktree_without_marker_is_closeable() {
        let (_tmp, _root, wt) = repo_with_worktree("no-marker");
        std::fs::remove_file(wt.join(".workspace-root")).unwrap();
        let plan = plan_chat_closeout(&wt, false);
        assert!(plan.is_worktree);
        assert!(!plan.dirty);
    }

    #[test]
    fn plan_subdirectory_of_a_worktree_is_not_a_worktree() {
        let (_tmp, _root, wt) = repo_with_worktree("subdir");
        let sub = wt.join("nested");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join(".workspace-root"), "x\n").unwrap();
        assert!(!plan_chat_closeout(&sub, false).is_worktree);
    }

    fn close_err(cwd: &Path, force: bool) -> String {
        close_chat_worktree(cwd, force, false)
            .unwrap_err()
            .to_string()
    }

    fn branch_exists(repo: &Path, branch: &str) -> bool {
        run_git(
            repo,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
        )
        .is_ok()
    }

    #[test]
    fn close_happy_path_removes_worktree_and_branch() {
        let (_tmp, root, wt) = repo_with_worktree("happy-path");
        let outcome = close_chat_worktree(&wt, false, false).expect("closed");
        assert!(outcome.removed);
        assert!(outcome.branch_deleted);
        assert!(!outcome.archived, "archiving is the RPC layer's job");
        assert!(!wt.exists());
        assert!(!branch_exists(&root, "happy-path"));
        assert!(branch_exists(&root, "main"));
        // The main checkout is untouched.
        assert!(root.join("README.md").exists());
    }

    #[test]
    fn close_refuses_dirty_without_force_and_succeeds_with_force() {
        let (_tmp, root, wt) = repo_with_worktree("dirty-close");
        std::fs::write(wt.join("README.md"), "changed\n").unwrap();
        std::fs::write(wt.join("a.txt"), "a\n").unwrap();
        std::fs::write(wt.join("b.txt"), "b\n").unwrap();

        let err = close_err(&wt, false);
        assert!(err.contains("3 uncommitted files"), "{err}");
        assert!(wt.exists(), "a refused close must leave the worktree alone");
        assert!(
            wt.join(".workspace-root").exists(),
            "marker survives a refusal"
        );
        assert!(branch_exists(&root, "dirty-close"));

        let outcome = close_chat_worktree(&wt, true, false).expect("forced close");
        assert!(outcome.removed);
        assert!(outcome.branch_deleted);
        assert!(!wt.exists());
        assert!(!branch_exists(&root, "dirty-close"));
    }

    #[test]
    fn close_refuses_unmerged_without_force_and_force_deletes_the_branch() {
        let (_tmp, root, wt) = repo_with_worktree("ahead-close");
        commit_file(&wt, "a.txt");
        commit_file(&wt, "b.txt");

        let err = close_err(&wt, false);
        assert!(err.contains("2 unmerged commits on ahead-close"), "{err}");
        assert!(wt.exists());
        assert!(branch_exists(&root, "ahead-close"));

        let outcome = close_chat_worktree(&wt, true, false).expect("forced close");
        assert!(outcome.removed && outcome.branch_deleted);
        assert!(!branch_exists(&root, "ahead-close"));
    }

    #[test]
    fn close_singular_wording() {
        let (_tmp, _root, wt) = repo_with_worktree("one-ahead");
        commit_file(&wt, "a.txt");
        assert!(close_err(&wt, false).contains("1 unmerged commit on one-ahead"));
        std::fs::write(wt.join("z.txt"), "z\n").unwrap();
        assert!(close_err(&wt, false).contains("1 uncommitted file in"));
    }

    #[test]
    fn close_refuses_a_live_chat_even_with_force() {
        let (_tmp, _root, wt) = repo_with_worktree("live-close");
        let err = close_chat_worktree(&wt, true, true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("live"), "{err}");
        assert!(wt.exists());
    }

    #[test]
    fn close_refuses_a_non_worktree_and_the_main_working_tree_even_with_force() {
        let (_tmp, root, _wt) = repo_with_worktree("sibling");
        std::fs::write(root.join(".workspace-root"), "x\n").unwrap();
        let err = close_err(&root, true);
        assert!(err.contains("not a closeable linked worktree"), "{err}");
        assert!(root.join(".git").exists());
        assert!(branch_exists(&root, "main"));

        let tmp2 = tempfile::tempdir().unwrap();
        let err = close_err(tmp2.path(), true);
        assert!(err.contains("not a closeable linked worktree"), "{err}");
    }

    #[test]
    fn close_never_deletes_the_default_branch() {
        let (_tmp, root, wt) = repo_with_worktree("make-room");
        // Make `main` the declared default (origin/HEAD) while the owning repo
        // sits on another branch, then put the linked worktree on `main`.
        run_git(&root, &["update-ref", "refs/remotes/origin/main", "main"]).unwrap();
        run_git(
            &root,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        )
        .unwrap();
        run_git(&root, &["checkout", "-q", "-b", "elsewhere"]).unwrap();
        run_git(&wt, &["checkout", "-q", "main"]).unwrap();

        let plan = plan_chat_closeout(&wt, false);
        assert_eq!(plan.branch.as_deref(), Some("main"));
        assert_eq!(plan.default_branch.as_deref(), Some("main"));

        for force in [false, true] {
            let err = close_err(&wt, force);
            assert!(err.contains("default branch"), "{err}");
            assert!(wt.exists());
            assert!(branch_exists(&root, "main"));
        }
    }

    #[test]
    fn close_detached_worktree_removes_it_without_touching_any_branch() {
        let (_tmp, root, wt) = repo_with_worktree("detached-close");
        run_git(&wt, &["checkout", "-q", "--detach"]).unwrap();
        let outcome = close_chat_worktree(&wt, false, false).expect("closed");
        assert!(outcome.removed);
        assert!(!outcome.branch_deleted);
        assert!(!wt.exists());
        assert!(branch_exists(&root, "main"));
        assert!(
            branch_exists(&root, "detached-close"),
            "not the worktree's checked-out branch"
        );
    }
}
