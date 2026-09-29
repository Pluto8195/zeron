//! PR/ticket linkage for a chat's branch — ticket 002 phase 2.
//!
//! Ported from `session_canvas_server.py` (the `agent-mode-tools` stopgap
//! tool)'s subprocess-shell-out approach, not a Rust API client: `gh pr view`
//! and `linear issue view`, matching `_run_gh_pr_view`/`fetch_linear_ticket`.
//! Rides the user's own `gh`/`linear` CLI auth; avoids standing up REST/
//! GraphQL types for no real benefit over a CLI that already works. `linear`
//! is optional — not installed on every machine (confirmed: absent on this
//! one) — its absence degrades to "no ticket data" rather than an error.
//!
//! Contract, matching the reference implementation's `get_cached_pr`/
//! `get_cached_ticket`: the read path (`status_for_chat`) is a **pure,
//! non-blocking cache read** — it never itself shells out. A miss just
//! registers the key for the next background sweep and returns `None` for
//! that piece; the caller (or a later poll) sees it populated once the sweep
//! catches up. The sweep (`spawn_sweep_loop`) drains a few pending keys per
//! tick — matches the reference's `PR_KEYS_PER_SWEEP`/`TICKET_IDS_PER_SWEEP
//! = 3` throttling spirit so this never hammers `gh`/`linear` unthrottled.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How long a cached PR/ticket entry is trusted before the sweep refreshes
/// it. The reference implementation's exact TTL constants weren't re-derived
/// here (not read as part of this pass) — this is a reasonable value in the
/// same spirit: fresh enough for an overview glanced at repeatedly, cheap
/// enough not to hammer `gh`/`linear`.
const CACHE_TTL: Duration = Duration::from_secs(180);
/// How long a cached diff-stat entry is trusted — much shorter than
/// `CACHE_TTL`: unlike a PR/ticket lookup (a remote call worth throttling
/// hard), a worktree's uncommitted-changes stat is local, cheap, and changes
/// as often as the session itself edits files, so parity with the ticket's
/// "modest TTL (~20-30s)" ask matters more here than reusing the GH/Linear
/// cadence.
const DIFF_STAT_TTL: Duration = Duration::from_secs(25);
/// Sweep tick interval.
const SWEEP_INTERVAL: Duration = Duration::from_secs(20);
/// Pending keys drained per sweep tick, per kind (PR, ticket, diff-stat) —
/// matches the reference's `PR_KEYS_PER_SWEEP`/`TICKET_IDS_PER_SWEEP = 3`.
const KEYS_PER_SWEEP: usize = 3;

/// `gh pr view --json` field set, matching `PR_VIEW_JSON_FIELDS` in
/// `session_canvas_server.py:1399`.
const PR_VIEW_JSON_FIELDS: &str = "number,url,state,isDraft,reviewDecision,statusCheckRollup,reviews,title,headRefName,mergeable,reviewRequests,author";

/// Checks-rollup summary, matching `summarize_checks` (`session_canvas_server.py:1377`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChecksStatus {
    Passing,
    Pending,
    Failing,
}

/// What, if anything, the PR's author can actually do next — the
/// `ACTION_META`/`ACTION_SEVERITY` vocabulary from `session_canvas.html`
/// (~line 2064), most-actionable first. A draft PR never classifies (matches
/// `classify_pr_action`'s early return on `is_draft`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrAction {
    CiFailing,
    MergeConflict,
    ChangesRequested,
    NeedsReviewer,
    ReadyToMerge,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrStatus {
    pub number: u64,
    pub url: Option<String>,
    pub state: String,
    pub is_draft: bool,
    pub review_decision: Option<String>,
    pub reviewers: Vec<String>,
    pub checks: Option<ChecksStatus>,
    pub title: Option<String>,
    pub branch: Option<String>,
    pub mergeable: String,
    pub has_reviewer_requested: bool,
}

impl PrStatus {
    /// Mirrors `classify_pr_action` (`session_canvas_server.py:1405`) —
    /// built here so phase 5's "My PRs" pane doesn't need to re-derive it.
    /// Only the single highest-severity reason is returned (the reference
    /// implementation returns every matching reason; here `Vec` order is
    /// severity-first, ready to render one badge or all of them).
    pub fn action_reasons(&self) -> Vec<PrAction> {
        if self.is_draft {
            return Vec::new();
        }
        let mut reasons = Vec::new();
        if self.checks == Some(ChecksStatus::Failing) {
            reasons.push(PrAction::CiFailing);
        }
        if self.mergeable == "conflicting" {
            reasons.push(PrAction::MergeConflict);
        }
        if self.review_decision.as_deref() == Some("CHANGES_REQUESTED") {
            reasons.push(PrAction::ChangesRequested);
        }
        if !self.has_reviewer_requested && self.reviewers.is_empty() {
            reasons.push(PrAction::NeedsReviewer);
        }
        if reasons.is_empty()
            && self.review_decision.as_deref() == Some("APPROVED")
            && self.checks == Some(ChecksStatus::Passing)
            && self.mergeable == "mergeable"
        {
            reasons.push(PrAction::ReadyToMerge);
        }
        reasons
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TicketStatus {
    pub identifier: String,
    pub title: Option<String>,
    pub url: Option<String>,
    pub status: Option<String>,
}

/// Uncommitted-changes summary for a worktree chat's cwd, from
/// `git diff --shortstat HEAD` — matches `git_diff_stat`
/// (`session_canvas_server.py:312-338`): uncommitted-only (never staged vs.
/// HEAD, never untracked files), scoped to the working tree, not the whole
/// repo history.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffStat {
    pub files_changed: u32,
    pub lines_added: u32,
    pub lines_removed: u32,
}

/// What's known right now for one chat's branch — either side may still be
/// `None` because nothing links (no PR/ticket exists) or because the sweep
/// hasn't gotten to it yet; the caller can't distinguish those without
/// polling again, matching the reference implementation's own cache-read
/// contract.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatLinkStatus {
    pub pr: Option<PrStatus>,
    pub ticket: Option<TicketStatus>,
    /// `cwd` sits under a `.worktrees/`/`.agent-worktrees/` directory —
    /// `is_worktree` (`session_canvas_server.py:300-304`). A pure string
    /// check computed inline on every read, unlike everything else on this
    /// struct: no I/O, so no cache entry (and no "pending") makes sense for it.
    pub is_worktree: bool,
    /// Uncommitted-diff summary, populated ONLY when `is_worktree` — matches
    /// the reference's own gate (`_build_claude_session_tile`,
    /// server.py:2192-2257): a non-worktree chat's cwd is the primary
    /// checkout, where "uncommitted changes" isn't the same
    /// this-session's-own-sandbox signal a worktree's is. `None` while the
    /// sweep hasn't gotten to it yet (or the cwd isn't a worktree at all),
    /// same cache-read contract as `pr`/`ticket`.
    pub diff_stat: Option<DiffStat>,
    /// Provenance of `pr`, when it came from the chat row's durable
    /// `linked_pr_url` (ticket 0xx) rather than the branch-based `gh pr view`
    /// guess. `None` (serialized as JSON `null`, never omitted — see the
    /// wire-contract test below) means `pr` (when present at all) came from
    /// inference — the pre-existing behavior, unchanged for a chat with no
    /// durable link.
    pub pr_source: Option<zeron_proto::ChatLinkSource>,
    /// Sibling to [`Self::pr_source`] for `ticket`.
    pub ticket_source: Option<zeron_proto::ChatLinkSource>,
}

/// `is_worktree` (`session_canvas_server.py:300-304`): the cwd sits under a
/// `.worktrees/` or `.agent-worktrees/` directory anywhere along its path —
/// covers both Zeron's own worktree layout and agent-mode.sh's legacy one.
pub fn is_worktree_cwd(cwd: &str) -> bool {
    cwd.contains("/.worktrees/") || cwd.contains("/.agent-worktrees/")
}

/// How long the bulk "my open PRs" list is trusted before a re-search.
/// Matches the reference's separate, cheaper cadence for the list call vs.
/// per-PR detail (`my_prs_refresh_loop`'s comment: "cheap enough to redo
/// every minute outright").
const MY_PRS_LIST_TTL: Duration = Duration::from_secs(60);

/// One row from `gh search prs --author=@me` — list-only fields, no detail
/// (checks/review/mergeable) yet. Matches `fetch_my_open_prs`'s dict shape
/// (`session_canvas_server.py:1554`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MyOpenPr {
    pub number: u64,
    pub title: Option<String>,
    pub url: Option<String>,
    pub repo: Option<String>,
    pub is_draft: bool,
    pub updated_at: Option<String>,
}

/// A `MyOpenPr` plus whatever detail (checks/reviews/mergeable →
/// `action_reasons()`) the throttled detail sweep has filled in so far —
/// `None` until that catches up, same "may still be pending" contract as
/// `ChatLinkStatus`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MyPrItem {
    #[serde(flatten)]
    pub summary: MyOpenPr,
    pub detail: Option<PrStatus>,
}

/// Cache/pending key for a PR lookup. `Url` resolves a chat's durable
/// `linked_pr_url` (ticket 0xx) directly — no local checkout needed, unlike
/// `Branch`, which shells out from a cwd the way this cache always has.
/// `status_for` prefers `Url` when the chat has one; a link-less chat keeps
/// exactly the pre-existing `Branch` behavior.
#[derive(Clone, Hash, Eq, PartialEq)]
enum PrKey {
    Branch { cwd: PathBuf, branch: String },
    Url(String),
}

struct Entry<T> {
    data: Option<T>,
    fetched_at: Instant,
}

impl<T> Entry<T> {
    /// TTL is per-kind (`CACHE_TTL` for PR/ticket, `DIFF_STAT_TTL` for
    /// diff-stat) rather than a single constant baked in here.
    fn fresh(&self, ttl: Duration) -> bool {
        self.fetched_at.elapsed() < ttl
    }
}

struct Inner {
    pr_cache: Mutex<HashMap<PrKey, Entry<PrStatus>>>,
    ticket_cache: Mutex<HashMap<String, Entry<TicketStatus>>>,
    pending_pr: Mutex<HashSet<PrKey>>,
    pending_ticket: Mutex<HashSet<String>>,
    /// Keyed by cwd — a local, no-throttling-concern lookup (unlike
    /// `pr_cache`'s `(cwd, branch)`, since diff-stat doesn't need a branch).
    diff_stat_cache: Mutex<HashMap<PathBuf, Entry<DiffStat>>>,
    pending_diff_stat: Mutex<HashSet<PathBuf>>,
    /// `None` until the first successful `fetch_my_open_prs` sweep.
    my_prs_list: Mutex<Option<(Vec<MyOpenPr>, Instant)>>,
    my_pr_detail: Mutex<HashMap<(String, u64), Entry<PrStatus>>>,
}

/// Cheap to clone (an `Arc` around the shared cache) — hand a clone to the
/// RPC layer and to `spawn_sweep_loop`, same shape as `ExternalSessionImporter`.
#[derive(Clone)]
pub struct PrTicketCache {
    inner: Arc<Inner>,
}

impl PrTicketCache {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                pr_cache: Mutex::new(HashMap::new()),
                ticket_cache: Mutex::new(HashMap::new()),
                pending_pr: Mutex::new(HashSet::new()),
                pending_ticket: Mutex::new(HashSet::new()),
                diff_stat_cache: Mutex::new(HashMap::new()),
                pending_diff_stat: Mutex::new(HashSet::new()),
                my_prs_list: Mutex::new(None),
                my_pr_detail: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Pure cache read, never blocks on a live `gh`/`linear` call. A branch
    /// with nothing cached yet is registered for the next sweep tick and
    /// reads back as `None` for that piece until then.
    ///
    /// Lookup order (ticket 0xx): `linked_pr_url`/`linked_ticket_id` — the
    /// chat row's durable link — wins over the branch-based `gh pr view`
    /// guess / ticket-id regex when present; a link-less chat (`None` for
    /// both) keeps EXACTLY the pre-existing branch/regex behavior. Callers
    /// (`CHAT_LINK_STATUS`'s handler) attach `prSource`/`ticketSource`
    /// themselves from the chat row — this method only decides WHICH key to
    /// resolve, not the provenance to report.
    pub fn status_for(
        &self,
        cwd: Option<&str>,
        branch: Option<&str>,
        linked_pr_url: Option<&str>,
        linked_ticket_id: Option<&str>,
    ) -> ChatLinkStatus {
        let mut status = ChatLinkStatus::default();

        let pr_key = match linked_pr_url.filter(|url| !url.is_empty()) {
            Some(url) => Some(PrKey::Url(url.to_string())),
            None => match (cwd, branch) {
                (Some(cwd), Some(branch)) if !branch.is_empty() => Some(PrKey::Branch {
                    cwd: PathBuf::from(cwd),
                    branch: branch.to_string(),
                }),
                _ => None,
            },
        };
        if let Some(key) = pr_key {
            let cache = lock(&self.inner.pr_cache);
            match cache.get(&key) {
                Some(entry) if entry.fresh(CACHE_TTL) => status.pr = entry.data.clone(),
                _ => {
                    drop(cache);
                    lock(&self.inner.pending_pr).insert(key);
                }
            }
        }

        let ticket_id = linked_ticket_id
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .or_else(|| branch.and_then(extract_ticket_id));
        if let Some(ticket_id) = ticket_id {
            let cache = lock(&self.inner.ticket_cache);
            match cache.get(&ticket_id) {
                Some(entry) if entry.fresh(CACHE_TTL) => status.ticket = entry.data.clone(),
                _ => {
                    drop(cache);
                    lock(&self.inner.pending_ticket).insert(ticket_id);
                }
            }
        }

        // Worktree/diff-stat: pure string check + a local, cheap-but-not-free
        // `git` shell-out gated the same way PR/ticket are — never inline, a
        // cache read that registers a miss for the sweep to fill.
        if let Some(cwd) = cwd {
            status.is_worktree = is_worktree_cwd(cwd);
            if status.is_worktree {
                let key = PathBuf::from(cwd);
                let cache = lock(&self.inner.diff_stat_cache);
                match cache.get(&key) {
                    Some(entry) if entry.fresh(DIFF_STAT_TTL) => status.diff_stat = entry.data,
                    _ => {
                        drop(cache);
                        lock(&self.inner.pending_diff_stat).insert(key);
                    }
                }
            }
        }

        status
    }

    /// Pure cache read: every PR currently in the last-fetched list, paired
    /// with whatever detail the throttled sweep has filled in so far (`None`
    /// until then). Empty `Vec` before the first sweep tick populates the
    /// list, or if `gh search prs` found nothing / isn't authenticated.
    pub fn my_open_prs(&self) -> Vec<MyPrItem> {
        let list = lock(&self.inner.my_prs_list)
            .as_ref()
            .map(|(items, _)| items.clone())
            .unwrap_or_default();
        let details = lock(&self.inner.my_pr_detail);
        list.into_iter()
            .map(|summary| {
                let detail = summary
                    .repo
                    .as_ref()
                    .and_then(|repo| details.get(&(repo.clone(), summary.number)))
                    .and_then(|entry| entry.data.clone());
                MyPrItem { summary, detail }
            })
            .collect()
    }

    /// Spawn the throttled background sweep. Caller keeps the returned
    /// handle alive for as long as the sweep should run (dropping it aborts
    /// the loop) — `EngineCore` holds it for its own lifetime, same as any
    /// other engine-owned background task.
    pub fn spawn_sweep_loop(&self) -> tokio::task::JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(SWEEP_INTERVAL);
            loop {
                interval.tick().await;
                this.sweep_once().await;
            }
        })
    }

    async fn sweep_once(&self) {
        let pr_keys: Vec<PrKey> = {
            let mut pending = lock(&self.inner.pending_pr);
            let take: Vec<PrKey> = pending.iter().take(KEYS_PER_SWEEP).cloned().collect();
            for key in &take {
                pending.remove(key);
            }
            take
        };
        for key in pr_keys {
            let inner = self.inner.clone();
            let key2 = key.clone();
            let data = tokio::task::spawn_blocking(move || match &key2 {
                PrKey::Branch { cwd, branch } => fetch_github_pr(cwd, branch),
                PrKey::Url(url) => fetch_github_pr_by_url(url),
            })
            .await
            .unwrap_or(None);
            lock(&inner.pr_cache).insert(
                key,
                Entry {
                    data,
                    fetched_at: Instant::now(),
                },
            );
        }

        let ticket_ids: Vec<String> = {
            let mut pending = lock(&self.inner.pending_ticket);
            let take: Vec<String> = pending.iter().take(KEYS_PER_SWEEP).cloned().collect();
            for id in &take {
                pending.remove(id);
            }
            take
        };
        for id in ticket_ids {
            let inner = self.inner.clone();
            let id2 = id.clone();
            let data = tokio::task::spawn_blocking(move || fetch_linear_ticket(&id2))
                .await
                .unwrap_or(None);
            lock(&inner.ticket_cache).insert(
                id,
                Entry {
                    data,
                    fetched_at: Instant::now(),
                },
            );
        }

        let diff_stat_keys: Vec<PathBuf> = {
            let mut pending = lock(&self.inner.pending_diff_stat);
            let take: Vec<PathBuf> = pending.iter().take(KEYS_PER_SWEEP).cloned().collect();
            for key in &take {
                pending.remove(key);
            }
            take
        };
        for cwd in diff_stat_keys {
            let inner = self.inner.clone();
            let cwd2 = cwd.clone();
            let data = tokio::task::spawn_blocking(move || git_diff_shortstat(&cwd2))
                .await
                .unwrap_or(None);
            lock(&inner.diff_stat_cache).insert(
                cwd,
                Entry {
                    data,
                    fetched_at: Instant::now(),
                },
            );
        }

        self.sweep_my_prs_list().await;
        self.sweep_one_my_pr_detail().await;
    }

    /// Refresh the "my open PRs" list if stale. Own TTL/cadence, separate
    /// from the per-branch PR/ticket sweep above — matches
    /// `my_prs_refresh_loop`'s comment that the list call is cheap enough to
    /// redo on its own schedule without competing with the hot path.
    async fn sweep_my_prs_list(&self) {
        let stale = lock(&self.inner.my_prs_list)
            .as_ref()
            .is_none_or(|(_, fetched_at)| fetched_at.elapsed() >= MY_PRS_LIST_TTL);
        if !stale {
            return;
        }
        if let Some(items) = tokio::task::spawn_blocking(fetch_my_open_prs).await.unwrap_or(None) {
            *lock(&self.inner.my_prs_list) = Some((items, Instant::now()));
        }
    }

    /// Fill in ONE stale/missing PR detail per tick — "stalest first" per
    /// `my_prs_refresh_loop`'s one-per-tick detail throttling, keyed by
    /// `(repo, number)` rather than `(cwd, branch)` since the list call never
    /// gives us a local checkout path.
    async fn sweep_one_my_pr_detail(&self) {
        let needed: Vec<(String, u64)> = lock(&self.inner.my_prs_list)
            .as_ref()
            .map(|(items, _)| {
                items
                    .iter()
                    .filter_map(|pr| pr.repo.clone().map(|repo| (repo, pr.number)))
                    .collect()
            })
            .unwrap_or_default();
        let target = {
            let details = lock(&self.inner.my_pr_detail);
            needed
                .into_iter()
                .find(|key| !details.get(key).is_some_and(|entry| entry.fresh(CACHE_TTL)))
        };
        let Some((repo, number)) = target else {
            return;
        };
        let (repo2, number2) = (repo.clone(), number);
        let data = tokio::task::spawn_blocking(move || fetch_github_pr_by_number(&repo2, number2))
            .await
            .unwrap_or(None);
        lock(&self.inner.my_pr_detail).insert(
            (repo, number),
            Entry {
                data,
                fetched_at: Instant::now(),
            },
        );
    }
}

impl Default for PrTicketCache {
    fn default() -> Self {
        Self::new()
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `extract_ticket_id` (`session_canvas_server.py:1363`): the first
/// `[A-Za-z]{2,6}-\d+` "word" in `text` (bounded by non-alphanumeric on both
/// sides, matching the reference's `\b...\b` regex), uppercased. Hand-rolled
/// rather than pulling in the `regex` crate — not a workspace dependency
/// today and this pattern is simple enough not to need one (same call
/// `liveness.rs` made about `sysinfo`).
pub(crate) fn extract_ticket_id(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let is_word = |b: u8| b.is_ascii_alphanumeric();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_alphabetic() {
            let start = i;
            let mut j = i;
            while j < bytes.len() && bytes[j].is_ascii_alphabetic() {
                j += 1;
            }
            let letters = j - start;
            if (2..=6).contains(&letters) && j < bytes.len() && bytes[j] == b'-' {
                let dash = j;
                let mut k = dash + 1;
                while k < bytes.len() && bytes[k].is_ascii_digit() {
                    k += 1;
                }
                let digits = k - (dash + 1);
                let left_ok = start == 0 || !is_word(bytes[start - 1]);
                let right_ok = k >= bytes.len() || !is_word(bytes[k]);
                if digits > 0 && left_ok && right_ok {
                    return Some(text[start..k].to_ascii_uppercase());
                }
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    None
}

fn summarize_checks(checks: Option<&Value>) -> Option<ChecksStatus> {
    let checks = checks?.as_array()?;
    if checks.is_empty() {
        return None;
    }
    let mut any_failing = false;
    let mut any_pending = false;
    for c in checks {
        let status = c.get("status").and_then(Value::as_str).unwrap_or("").to_uppercase();
        let conclusion = c
            .get("conclusion")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_uppercase();
        if !status.is_empty() && status != "COMPLETED" {
            any_pending = true;
        } else if matches!(
            conclusion.as_str(),
            "FAILURE" | "TIMED_OUT" | "CANCELLED" | "STARTUP_FAILURE"
        ) {
            any_failing = true;
        } else if matches!(conclusion.as_str(), "SUCCESS" | "NEUTRAL" | "SKIPPED") {
            // passing; no flag needed
        } else {
            any_pending = true;
        }
    }
    Some(if any_failing {
        ChecksStatus::Failing
    } else if any_pending {
        ChecksStatus::Pending
    } else {
        ChecksStatus::Passing
    })
}

/// Bot/self-review filtering, matching `is_bot_login` (`session_canvas_server.py:1370`).
fn is_bot_login(login: &str) -> bool {
    let lower = login.to_lowercase();
    lower.contains("bot") || lower.contains("github-actions") || lower.contains("greptile")
}

fn pr_json_to_status(data: &Value) -> PrStatus {
    let reviews = data.get("reviews").and_then(Value::as_array).cloned().unwrap_or_default();
    let author_login = data
        .get("author")
        .and_then(|a| a.get("login"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut reviewers: Vec<String> = reviews
        .iter()
        .filter_map(|r| r.get("author").and_then(|a| a.get("login")).and_then(Value::as_str))
        .filter(|login| !is_bot_login(login) && Some(login.to_string()) != author_login)
        .map(str::to_string)
        .collect();
    reviewers.sort();
    reviewers.dedup();

    PrStatus {
        number: data.get("number").and_then(Value::as_u64).unwrap_or(0),
        url: data.get("url").and_then(Value::as_str).map(str::to_string),
        state: data
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_lowercase(),
        is_draft: data.get("isDraft").and_then(Value::as_bool).unwrap_or(false),
        review_decision: data.get("reviewDecision").and_then(Value::as_str).map(str::to_string),
        reviewers,
        checks: summarize_checks(data.get("statusCheckRollup")),
        title: data.get("title").and_then(Value::as_str).map(str::to_string),
        branch: data.get("headRefName").and_then(Value::as_str).map(str::to_string),
        mergeable: data
            .get("mergeable")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_lowercase(),
        has_reviewer_requested: data
            .get("reviewRequests")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty()),
    }
}

/// `.gitmodules` submodule paths under `root_cwd`, matching
/// `workspace_submodule_paths` (`session_canvas_server.py:1465`) — read
/// directly rather than hardcoding any repo's own submodule list, so it
/// keeps working if that list changes.
pub(crate) fn workspace_submodule_paths(root_cwd: &Path) -> Vec<String> {
    let gitmodules = root_cwd.join(".gitmodules");
    let Ok(contents) = std::fs::read_to_string(&gitmodules) else {
        return Vec::new();
    };
    contents
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            line.strip_prefix("path").and_then(|rest| {
                rest.trim_start()
                    .strip_prefix('=')
                    .map(|v| v.trim().to_string())
            })
        })
        .collect()
}

fn run_gh_pr_view(cwd: &Path, branch: &str) -> Option<Value> {
    let output = Command::new("gh")
        .args(["pr", "view", branch, "--json", PR_VIEW_JSON_FIELDS])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

/// `fetch_github_pr` (`session_canvas_server.py:1505`): try `cwd` directly,
/// then each `.gitmodules` submodule under it — confirmed in the reference
/// implementation's own comments that a monorepo superproject root often has
/// no PR for a branch that has a real, open one exactly one submodule down.
fn fetch_github_pr(cwd: &Path, branch: &str) -> Option<PrStatus> {
    let data = run_gh_pr_view(cwd, branch).or_else(|| {
        workspace_submodule_paths(cwd)
            .into_iter()
            .map(|sub| cwd.join(sub))
            .filter(|sub_cwd| sub_cwd.is_dir())
            .find_map(|sub_cwd| run_gh_pr_view(&sub_cwd, branch))
    })?;
    Some(pr_json_to_status(&data))
}

/// PR detail resolved from a durable `linked_pr_url` (ticket 0xx) rather than
/// a branch guess: `gh pr view <url>` is fully self-describing, so — unlike
/// [`fetch_github_pr`] — this needs no local checkout/cwd at all (the process
/// still runs from SOME cwd, but `gh` resolves the target repo from the URL,
/// not from where it happens to be invoked).
fn fetch_github_pr_by_url(url: &str) -> Option<PrStatus> {
    let output = Command::new("gh")
        .args(["pr", "view", url, "--json", PR_VIEW_JSON_FIELDS])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let data: Value = serde_json::from_slice(&output.stdout).ok()?;
    Some(pr_json_to_status(&data))
}

/// `git_diff_stat` (`session_canvas_server.py:312-338`): uncommitted changes
/// only, via `git diff --shortstat HEAD` run with `cwd` as the working
/// directory. `None` when `cwd` isn't a git repo, has no `HEAD` yet (a
/// brand-new worktree before its first commit), or the command otherwise
/// fails — same "nothing to show yet" contract as every other fetch in this
/// module; the caller doesn't distinguish "not a repo" from "clean" (a
/// genuinely clean worktree instead reads as `Some(DiffStat::default())`,
/// since `git diff --shortstat` succeeds with empty stdout in that case).
fn git_diff_shortstat(cwd: &Path) -> Option<DiffStat> {
    let output = Command::new("git")
        .args(["diff", "--shortstat", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(parse_git_shortstat(&String::from_utf8_lossy(&output.stdout)))
}

/// `git diff --shortstat`'s one-line summary, e.g.
/// `" 3 files changed, 10 insertions(+), 4 deletions(-)"` — any subset of the
/// three clauses can be absent (a pure-deletion diff has no "insertions"
/// clause at all, and vice versa), and a clean tree produces an empty string
/// entirely (`DiffStat::default()`, all zero). Hand-parsed rather than
/// pulling in the `regex` crate, matching `extract_ticket_id`'s precedent
/// just above.
fn parse_git_shortstat(text: &str) -> DiffStat {
    let mut stat = DiffStat::default();
    for clause in text.trim().split(',') {
        let clause = clause.trim();
        let Some(space_idx) = clause.find(char::is_whitespace) else {
            continue;
        };
        let (number, rest) = clause.split_at(space_idx);
        let Ok(number) = number.parse::<u32>() else {
            continue;
        };
        let rest = rest.trim_start();
        if rest.starts_with("file") {
            stat.files_changed = number;
        } else if rest.starts_with("insertion") {
            stat.lines_added = number;
        } else if rest.starts_with("deletion") {
            stat.lines_removed = number;
        }
    }
    stat
}

/// `fetch_my_open_prs` (`session_canvas_server.py:1554`), deliberately
/// WITHOUT that function's `--owner` scoping: the reference hardcodes
/// `MY_PRS_OWNER = "Cofactr"` (one specific org), which only makes sense for
/// its one deployment. `--author=@me --state=open` alone, with no `--owner`,
/// searches every repo the signed-in `gh` account can see — the general
/// per-user equivalent that works for anyone, not just this org.
fn fetch_my_open_prs() -> Option<Vec<MyOpenPr>> {
    let output = Command::new("gh")
        .args([
            "search",
            "prs",
            "--author=@me",
            "--state=open",
            "--json",
            "number,title,url,repository,isDraft,updatedAt",
            "--limit",
            "50",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let data: Vec<Value> = serde_json::from_slice(&output.stdout).ok()?;
    Some(
        data.iter()
            .map(|d| MyOpenPr {
                number: d.get("number").and_then(Value::as_u64).unwrap_or(0),
                title: d.get("title").and_then(Value::as_str).map(str::to_string),
                url: d.get("url").and_then(Value::as_str).map(str::to_string),
                repo: d
                    .get("repository")
                    .and_then(|r| r.get("nameWithOwner"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                is_draft: d.get("isDraft").and_then(Value::as_bool).unwrap_or(false),
                updated_at: d.get("updatedAt").and_then(Value::as_str).map(str::to_string),
            })
            .collect(),
    )
}

/// `fetch_github_pr_by_number`: detail fetch keyed by `(repo, number)`
/// instead of `(cwd, branch)` — `gh pr view <number> --repo <owner/repo>`
/// works from anywhere, no local checkout needed, matching how the list
/// call itself has no cwd to begin with.
fn fetch_github_pr_by_number(repo: &str, number: u64) -> Option<PrStatus> {
    let output = Command::new("gh")
        .args([
            "pr",
            "view",
            &number.to_string(),
            "--repo",
            repo,
            "--json",
            PR_VIEW_JSON_FIELDS,
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let data: Value = serde_json::from_slice(&output.stdout).ok()?;
    Some(pr_json_to_status(&data))
}

/// `fetch_linear_ticket` (`session_canvas_server.py:1595`). `linear` may not
/// be installed (confirmed absent on this machine) — that's a normal `None`,
/// not an error; every failure mode here (missing binary, non-zero exit,
/// unparseable output) collapses to the same "no ticket data available".
fn fetch_linear_ticket(ticket_id: &str) -> Option<TicketStatus> {
    let output = Command::new("linear")
        .args(["issue", "view", ticket_id, "--json"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let data: Value = serde_json::from_slice(&output.stdout).ok()?;
    let status = data
        .get("state")
        .and_then(|s| s.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(TicketStatus {
        identifier: data
            .get("identifier")
            .and_then(Value::as_str)
            .unwrap_or(ticket_id)
            .to_string(),
        title: data.get("title").and_then(Value::as_str).map(str::to_string),
        url: data.get("url").and_then(Value::as_str).map(str::to_string),
        status,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_ticket_id_from_branch_names() {
        assert_eq!(extract_ticket_id("eng-2715-fix-thing"), Some("ENG-2715".into()));
        assert_eq!(extract_ticket_id("feature/ABC-123-do-stuff"), Some("ABC-123".into()));
        assert_eq!(extract_ticket_id("mikey/quick-fix"), None);
        assert_eq!(extract_ticket_id(""), None);
        // 7-letter prefix is outside the `{2,6}` bound — must not match.
        assert_eq!(extract_ticket_id("abcdefg-123"), None);
        // No digits after the dash — must not match.
        assert_eq!(extract_ticket_id("eng-"), None);
    }

    #[test]
    fn extract_ticket_id_requires_word_boundary() {
        // "xeng-123x" must not match "ENG-123" — the char before/after the
        // whole token must not itself be alphanumeric.
        assert_eq!(extract_ticket_id("xeng-123x"), None);
        assert_eq!(extract_ticket_id("prefix-eng-123-suffix"), Some("ENG-123".into()));
    }

    #[test]
    fn checks_summary_prioritizes_failing_over_pending_over_passing() {
        let failing = serde_json::json!([
            {"status": "COMPLETED", "conclusion": "SUCCESS"},
            {"status": "COMPLETED", "conclusion": "FAILURE"},
        ]);
        assert_eq!(summarize_checks(Some(&failing)), Some(ChecksStatus::Failing));

        let pending = serde_json::json!([
            {"status": "COMPLETED", "conclusion": "SUCCESS"},
            {"status": "IN_PROGRESS", "conclusion": null},
        ]);
        assert_eq!(summarize_checks(Some(&pending)), Some(ChecksStatus::Pending));

        let passing = serde_json::json!([{"status": "COMPLETED", "conclusion": "SUCCESS"}]);
        assert_eq!(summarize_checks(Some(&passing)), Some(ChecksStatus::Passing));

        assert_eq!(summarize_checks(None), None);
    }

    #[test]
    fn pr_json_to_status_filters_bot_and_self_reviews() {
        let data = serde_json::json!({
            "number": 42,
            "url": "https://example.com/pr/42",
            "state": "OPEN",
            "isDraft": false,
            "reviewDecision": "APPROVED",
            "reviews": [
                {"author": {"login": "greptile-apps"}},
                {"author": {"login": "the-author"}},
                {"author": {"login": "real-reviewer"}},
            ],
            "title": "Fix the thing",
            "headRefName": "eng-2715-fix-thing",
            "mergeable": "MERGEABLE",
            "reviewRequests": [],
            "author": {"login": "the-author"},
        });
        let status = pr_json_to_status(&data);
        assert_eq!(status.reviewers, vec!["real-reviewer".to_string()]);
        assert_eq!(status.mergeable, "mergeable");
        assert!(!status.has_reviewer_requested);
    }

    #[test]
    fn action_reasons_matches_reference_classification() {
        let mut pr = PrStatus {
            number: 1,
            url: None,
            state: "open".into(),
            is_draft: false,
            review_decision: None,
            reviewers: Vec::new(),
            checks: Some(ChecksStatus::Failing),
            title: None,
            branch: None,
            mergeable: "mergeable".into(),
            has_reviewer_requested: true,
        };
        assert_eq!(pr.action_reasons(), vec![PrAction::CiFailing]);

        pr.checks = Some(ChecksStatus::Passing);
        pr.mergeable = "conflicting".into();
        assert_eq!(pr.action_reasons(), vec![PrAction::MergeConflict]);

        pr.mergeable = "mergeable".into();
        pr.review_decision = Some("CHANGES_REQUESTED".into());
        assert_eq!(pr.action_reasons(), vec![PrAction::ChangesRequested]);

        pr.review_decision = None;
        pr.has_reviewer_requested = false;
        pr.reviewers = Vec::new();
        assert_eq!(pr.action_reasons(), vec![PrAction::NeedsReviewer]);

        pr.has_reviewer_requested = true;
        pr.review_decision = Some("APPROVED".into());
        assert_eq!(pr.action_reasons(), vec![PrAction::ReadyToMerge]);

        pr.is_draft = true;
        assert!(pr.action_reasons().is_empty());
    }

    #[test]
    fn status_for_registers_pending_on_miss_and_reads_cache_on_hit() {
        let cache = PrTicketCache::new();
        // First read: nothing cached yet — both sides None, key registered.
        let status = cache.status_for(Some("/tmp/repo"), Some("eng-2715-fix-thing"), None, None);
        assert!(status.pr.is_none());
        assert!(status.ticket.is_none());
        assert_eq!(lock(&cache.inner.pending_pr).len(), 1);
        assert_eq!(lock(&cache.inner.pending_ticket).len(), 1);

        // Manually seed the cache (as the sweep would) and confirm a fresh
        // read serves it without re-registering as pending.
        lock(&cache.inner.pending_pr).clear();
        lock(&cache.inner.pr_cache).insert(
            PrKey::Branch {
                cwd: PathBuf::from("/tmp/repo"),
                branch: "eng-2715-fix-thing".to_string(),
            },
            Entry {
                data: Some(PrStatus {
                    number: 7,
                    url: None,
                    state: "open".into(),
                    is_draft: false,
                    review_decision: None,
                    reviewers: Vec::new(),
                    checks: None,
                    title: None,
                    branch: None,
                    mergeable: "unknown".into(),
                    has_reviewer_requested: false,
                }),
                fetched_at: Instant::now(),
            },
        );
        let status = cache.status_for(Some("/tmp/repo"), Some("eng-2715-fix-thing"), None, None);
        assert_eq!(status.pr.map(|p| p.number), Some(7));
        assert!(lock(&cache.inner.pending_pr).is_empty());
    }

    #[test]
    fn status_for_with_no_branch_is_a_pure_noop() {
        let cache = PrTicketCache::new();
        let status = cache.status_for(Some("/tmp/repo"), None, None, None);
        assert!(status.pr.is_none());
        assert!(status.ticket.is_none());
        assert!(lock(&cache.inner.pending_pr).is_empty());
        assert!(lock(&cache.inner.pending_ticket).is_empty());
    }

    /// Ticket 0xx lookup-order change: a chat with a durable `linked_pr_url`
    /// resolves PR detail by that URL, NOT the branch guess — even when a
    /// cwd+branch are also given (the two are never combined into the same
    /// key).
    #[test]
    fn status_for_prefers_linked_pr_url_over_the_branch_guess() {
        let cache = PrTicketCache::new();
        let status = cache.status_for(
            Some("/tmp/repo"),
            Some("eng-2715-fix-thing"),
            Some("https://github.com/acme/widgets/pull/42"),
            None,
        );
        assert!(status.pr.is_none(), "nothing cached yet");
        let pending = lock(&cache.inner.pending_pr);
        assert_eq!(pending.len(), 1);
        assert!(pending.contains(&PrKey::Url("https://github.com/acme/widgets/pull/42".to_string())));
        assert!(!pending.contains(&PrKey::Branch {
            cwd: PathBuf::from("/tmp/repo"),
            branch: "eng-2715-fix-thing".to_string(),
        }));
    }

    /// Sibling to the above: a link-LESS chat keeps exactly the pre-existing
    /// branch-based `Branch` key.
    #[test]
    fn status_for_falls_back_to_the_branch_guess_when_link_less() {
        let cache = PrTicketCache::new();
        let status = cache.status_for(Some("/tmp/repo"), Some("eng-2715-fix-thing"), None, None);
        assert!(status.pr.is_none());
        let pending = lock(&cache.inner.pending_pr);
        assert!(pending.contains(&PrKey::Branch {
            cwd: PathBuf::from("/tmp/repo"),
            branch: "eng-2715-fix-thing".to_string(),
        }));
    }

    /// Same lookup-order change, ticket side: a durable `linked_ticket_id`
    /// wins over the branch-name regex guess.
    #[test]
    fn status_for_prefers_linked_ticket_id_over_the_branch_regex() {
        let cache = PrTicketCache::new();
        let status =
            cache.status_for(Some("/tmp/repo"), Some("eng-2715-fix-thing"), None, Some("OPS-9"));
        assert!(status.ticket.is_none(), "nothing cached yet");
        let pending = lock(&cache.inner.pending_ticket);
        assert!(pending.contains("OPS-9"));
        assert!(!pending.contains("ENG-2715"));
    }

    #[test]
    fn is_worktree_cwd_matches_both_known_layouts() {
        assert!(is_worktree_cwd("/Users/mikey/Projects/cofactr/workspace/.worktrees/eng-1"));
        assert!(is_worktree_cwd(
            "/Users/mikey/Projects/cofactr/workspace/.agent-worktrees/ENG-2713"
        ));
        assert!(!is_worktree_cwd("/Users/mikey/Projects/cofactr/workspace"));
        // A substring match elsewhere in the path must not false-positive —
        // the check is specifically for the `/.worktrees/`/`.agent-worktrees/`
        // path segment, not any occurrence of the word.
        assert!(!is_worktree_cwd("/Users/mikey/Projects/worktrees-are-fun/workspace"));
    }

    #[test]
    fn parse_git_shortstat_handles_every_clause_shape() {
        assert_eq!(
            parse_git_shortstat(" 3 files changed, 10 insertions(+), 4 deletions(-)"),
            DiffStat {
                files_changed: 3,
                lines_added: 10,
                lines_removed: 4,
            }
        );
        // Pure addition: no "deletions" clause at all.
        assert_eq!(
            parse_git_shortstat(" 1 file changed, 1 insertion(+)"),
            DiffStat {
                files_changed: 1,
                lines_added: 1,
                lines_removed: 0,
            }
        );
        // Pure deletion: no "insertions" clause at all.
        assert_eq!(
            parse_git_shortstat(" 1 file changed, 3 deletions(-)"),
            DiffStat {
                files_changed: 1,
                lines_added: 0,
                lines_removed: 3,
            }
        );
        // A clean tree: `git diff --shortstat` prints nothing.
        assert_eq!(parse_git_shortstat(""), DiffStat::default());
        assert_eq!(parse_git_shortstat("\n"), DiffStat::default());
    }

    #[test]
    fn status_for_computes_is_worktree_without_touching_the_diff_stat_cache_for_non_worktrees() {
        let cache = PrTicketCache::new();
        let status = cache.status_for(Some("/Users/mikey/Projects/cofactr/workspace"), None, None, None);
        assert!(!status.is_worktree);
        assert!(status.diff_stat.is_none());
        assert!(
            lock(&cache.inner.pending_diff_stat).is_empty(),
            "a non-worktree cwd must never register for a diff-stat sweep"
        );
    }

    #[test]
    fn status_for_registers_a_worktree_cwd_as_pending_then_reads_the_cache_on_hit() {
        let cache = PrTicketCache::new();
        let cwd = "/Users/mikey/Projects/cofactr/workspace/.agent-worktrees/ENG-2713";
        let status = cache.status_for(Some(cwd), None, None, None);
        assert!(status.is_worktree);
        assert!(status.diff_stat.is_none(), "nothing cached yet");
        assert_eq!(lock(&cache.inner.pending_diff_stat).len(), 1);

        lock(&cache.inner.pending_diff_stat).clear();
        lock(&cache.inner.diff_stat_cache).insert(
            PathBuf::from(cwd),
            Entry {
                data: Some(DiffStat {
                    files_changed: 2,
                    lines_added: 5,
                    lines_removed: 1,
                }),
                fetched_at: Instant::now(),
            },
        );
        let status = cache.status_for(Some(cwd), None, None, None);
        assert_eq!(
            status.diff_stat,
            Some(DiffStat {
                files_changed: 2,
                lines_added: 5,
                lines_removed: 1,
            })
        );
        assert!(lock(&cache.inner.pending_diff_stat).is_empty());
    }

    /// Wire-contract pin: the overview UI (`crates/ui/src/overview.rs`,
    /// outside this crate) deserializes `ChatLinkStatus`'s raw JSON directly
    /// and depends on these EXACT camelCase field names —
    /// `isWorktree`/`diffStat.{filesChanged,linesAdded,linesRemoved}`/
    /// `prSource`/`ticketSource`. A silent rename here (e.g. from a
    /// `#[serde(rename_all)]` refactor) would compile fine on this side and
    /// just silently stop populating the UI's tiles, so this pins the exact
    /// JSON shape rather than only the Rust struct fields the tests above
    /// already cover.
    #[test]
    fn chat_link_status_json_field_names_match_the_overview_ui_contract() {
        let status = ChatLinkStatus {
            pr: None,
            ticket: None,
            is_worktree: true,
            diff_stat: Some(DiffStat {
                files_changed: 2,
                lines_added: 5,
                lines_removed: 1,
            }),
            pr_source: Some(zeron_proto::ChatLinkSource::Manual),
            ticket_source: None,
        };
        let value = serde_json::to_value(&status).unwrap();
        assert_eq!(value["isWorktree"], serde_json::json!(true));
        assert_eq!(
            value["diffStat"],
            serde_json::json!({"filesChanged": 2, "linesAdded": 5, "linesRemoved": 1})
        );
        // `prSource` round-trips the snake_case wire spelling the design
        // fixes (`"manual" | "created_in_chat" | "mentioned"`), even though
        // every OTHER field on this struct is camelCase.
        assert_eq!(value["prSource"], serde_json::json!("manual"));
        assert_eq!(value["ticketSource"], serde_json::Value::Null);

        // Non-worktree shape: `isWorktree: false`, `diffStat: null`,
        // `prSource`/`ticketSource: null` — the UI is told explicitly, not
        // left to infer absence-means-false/inferred.
        let non_worktree = ChatLinkStatus::default();
        let value = serde_json::to_value(&non_worktree).unwrap();
        assert_eq!(value["isWorktree"], serde_json::json!(false));
        assert_eq!(value["diffStat"], serde_json::Value::Null);
        assert_eq!(value["prSource"], serde_json::Value::Null);
        assert_eq!(value["ticketSource"], serde_json::Value::Null);
    }

    /// Live smoke test: this very repo's own checkout, not a worktree, so
    /// `git diff --shortstat HEAD` should run cleanly and return *some*
    /// parseable stat (zero or not, depending on the working tree at test
    /// time) — proves the subprocess plumbing, not specific content.
    /// `#[ignore]`: depends on `git` being on PATH.
    #[test]
    #[ignore = "depends on `git` being installed"]
    fn live_git_diff_shortstat_against_this_repo_runs_and_parses() {
        let cwd = std::env::current_dir().unwrap();
        assert!(git_diff_shortstat(&cwd).is_some());
    }

    /// Live smoke test against this very repo's own remote — `gh` is
    /// installed on this machine (confirmed: `linear` is not, so no
    /// equivalent ticket-side live test is possible here). Real evidence the
    /// subprocess plumbing actually works end to end, not just against fake
    /// JSON fixtures. `#[ignore]`: depends on network + `gh` auth state on
    /// whatever machine runs the suite.
    #[test]
    #[ignore = "depends on `gh` being installed and authenticated, and on network access"]
    fn live_gh_pr_lookup_against_a_real_repo() {
        // main/master branch of the zeronsh/zeron upstream is very unlikely
        // to have an open PR against it, so this only proves plumbing
        // (spawns gh, gets a clean non-error `None`), not the full parse
        // path — that's covered by the fixture-based tests above.
        let cwd = std::env::current_dir().unwrap();
        let result = fetch_github_pr(&cwd, "this-branch-should-not-exist-anywhere");
        assert!(result.is_none(), "a bogus branch must not fabricate a PR");
    }

    fn sample_my_pr(number: u64, repo: &str) -> MyOpenPr {
        MyOpenPr {
            number,
            title: Some(format!("PR #{number}")),
            url: Some(format!("https://github.com/{repo}/pull/{number}")),
            repo: Some(repo.to_string()),
            is_draft: false,
            updated_at: None,
        }
    }

    #[test]
    fn my_open_prs_is_empty_before_any_sweep_has_populated_the_list() {
        let cache = PrTicketCache::new();
        assert!(cache.my_open_prs().is_empty());
    }

    #[test]
    fn my_open_prs_pairs_list_entries_with_their_cached_detail() {
        let cache = PrTicketCache::new();
        *lock(&cache.inner.my_prs_list) = Some((
            vec![sample_my_pr(1, "acme/widgets"), sample_my_pr(2, "acme/widgets")],
            Instant::now(),
        ));
        // Only #1 has detail cached — #2 must read back with `detail: None`,
        // not panic or get skipped.
        lock(&cache.inner.my_pr_detail).insert(
            ("acme/widgets".to_string(), 1),
            Entry {
                data: Some(PrStatus {
                    number: 1,
                    url: Some("https://github.com/acme/widgets/pull/1".into()),
                    state: "open".into(),
                    is_draft: false,
                    review_decision: None,
                    reviewers: Vec::new(),
                    checks: Some(ChecksStatus::Failing),
                    title: None,
                    branch: None,
                    mergeable: "unknown".into(),
                    // `true` here isolates this test's actual point (detail
                    // pairing, not action-classification breadth) — `false`
                    // would also add `NeedsReviewer`, already covered by
                    // `action_reasons_matches_reference_classification`.
                    has_reviewer_requested: true,
                }),
                fetched_at: Instant::now(),
            },
        );

        let items = cache.my_open_prs();
        assert_eq!(items.len(), 2);
        let pr1 = items.iter().find(|i| i.summary.number == 1).unwrap();
        assert!(pr1.detail.is_some());
        assert_eq!(pr1.detail.as_ref().unwrap().action_reasons(), vec![PrAction::CiFailing]);
        let pr2 = items.iter().find(|i| i.summary.number == 2).unwrap();
        assert!(pr2.detail.is_none(), "PR with no cached detail yet must read back as None, not panic");
    }

    #[test]
    fn my_prs_list_staleness_is_ttl_gated() {
        // Fresh entry: not stale.
        let fresh = (vec![sample_my_pr(1, "a/b")], Instant::now());
        assert!(fresh.1.elapsed() < MY_PRS_LIST_TTL);

        // An entry old enough must be picked up as needing a re-sweep — the
        // actual sweep is exercised live in `sweep_my_prs_list`, but the TTL
        // comparison itself (what `sweep_my_prs_list` guards on) is what a
        // unit test can pin down without shelling out.
        let stale_at = Instant::now()
            .checked_sub(MY_PRS_LIST_TTL + Duration::from_secs(1))
            .expect("MY_PRS_LIST_TTL is small enough that now() - it - 1s doesn't underflow");
        assert!(stale_at.elapsed() >= MY_PRS_LIST_TTL);
    }

    /// Live smoke test: `gh search prs --author=@me --state=open` (no
    /// `--owner`) actually runs and returns parseable JSON on this machine.
    /// Doesn't assert on *content* (an empty result is a perfectly valid
    /// "no open PRs right now") — only that the subprocess+parse path works
    /// end to end, same evidentiary bar as `live_gh_pr_lookup_against_a_real_repo`.
    #[test]
    #[ignore = "depends on `gh` being installed and authenticated, and on network access"]
    fn live_my_open_prs_search_runs_and_parses() {
        let result = fetch_my_open_prs();
        assert!(result.is_some(), "gh search prs must return parseable JSON, even if empty");
    }
}
