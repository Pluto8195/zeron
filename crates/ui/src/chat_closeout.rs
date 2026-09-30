//! Chat close-out: tear down a chat's agent worktree (and its branch) from
//! the UI, then let the engine archive the chat.
//!
//! Two engine calls, both IPC-only:
//! - [`PLAN_CHAT_CLOSEOUT`] (read-only) feeds the confirmation dialog.
//! - [`CLOSE_CHAT_WORKTREE`] (destructive) runs on confirm; `force` is sent
//!   only from the "Force close out" button, which the dialog offers only
//!   when the plan reports soft blockers (dirty tree / unmerged commits).
//!
//! The action is offered on chats whose cwd holds a `.workspace-root` marker
//! ([`has_workspace_root`], the same signal the overview's `repo_key` uses).
//! Everything here is pure so the decode/format/state logic is unit-testable
//! without a gpui context; the dialog itself lives in `shell/closeout_ui.rs`.

use serde_json::Value;

/// `{chatId, cwd}` → `{isWorktree, worktreePath, branch, chatLive, dirty,
/// dirtyFiles, unmergedCommits, defaultBranch}`.
pub use zeron_rpc::methods::PLAN_CHAT_CLOSEOUT;
/// `{chatId, cwd, force}` → `{removed, branchDeleted, archived}`; errors carry
/// human-readable refusal reasons.
pub use zeron_rpc::methods::CLOSE_CHAT_WORKTREE;

/// The plan runs a handful of `git` commands plus a process scan.
pub const PLAN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// `git worktree remove` on a large tree can take a while.
pub const CLOSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The engine caps `dirtyFiles` at this many entries.
pub const DIRTY_FILES_CAP: usize = 20;
/// How many dirty filenames the dialog lists before "+N more".
pub const DIRTY_FILES_SHOWN: usize = 5;

/// Cheap visibility predicate for the "Close out worktree…" action: the
/// chat's cwd has a `.workspace-root` file. The engine does the real check
/// (marker AND registered linked worktree) in the plan.
pub fn has_workspace_root(cwd: Option<&str>) -> bool {
    cwd.filter(|c| !c.trim().is_empty())
        .is_some_and(|c| std::path::Path::new(c).join(".workspace-root").is_file())
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CloseoutPlan {
    pub is_worktree: bool,
    pub worktree_path: String,
    pub branch: Option<String>,
    pub chat_live: bool,
    pub dirty: bool,
    pub dirty_files: Vec<String>,
    pub unmerged_commits: u32,
    pub default_branch: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloseoutOutcome {
    pub removed: bool,
    pub branch_deleted: bool,
    pub archived: bool,
}

/// Decode a `PLAN_CHAT_CLOSEOUT` reply. `isWorktree` and `chatLive` are
/// required (they gate the destructive button); the rest default to
/// empty/null/0 like the engine's own "not a worktree" shape.
pub fn decode_plan(value: &Value) -> Result<CloseoutPlan, String> {
    let flag = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_bool)
            .ok_or_else(|| format!("close-out plan missing {key}"))
    };
    let opt_str = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let dirty_files: Vec<String> = value
        .get("dirtyFiles")
        .and_then(Value::as_array)
        .map(|files| {
            files
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Ok(CloseoutPlan {
        is_worktree: flag("isWorktree")?,
        chat_live: flag("chatLive")?,
        worktree_path: opt_str("worktreePath").unwrap_or_default(),
        branch: opt_str("branch"),
        // A non-empty file list is dirty even if the flag were missing.
        dirty: value.get("dirty").and_then(Value::as_bool).unwrap_or(false)
            || !dirty_files.is_empty(),
        dirty_files,
        unmerged_commits: value
            .get("unmergedCommits")
            .and_then(Value::as_u64)
            .map(|n| n.min(u32::MAX as u64) as u32)
            .unwrap_or(0),
        default_branch: opt_str("defaultBranch"),
    })
}

/// Decode a `CLOSE_CHAT_WORKTREE` reply; `removed` is required.
pub fn decode_outcome(value: &Value) -> Result<CloseoutOutcome, String> {
    let removed = value
        .get("removed")
        .and_then(Value::as_bool)
        .ok_or_else(|| "close-out reply missing removed".to_string())?;
    let flag = |key: &str| value.get(key).and_then(Value::as_bool).unwrap_or(false);
    Ok(CloseoutOutcome {
        removed,
        branch_deleted: flag("branchDeleted"),
        archived: flag("archived"),
    })
}

/// What the dialog offers for a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseoutVerdict {
    /// Not a closeable worktree — notice only, no proceed button.
    NotWorktree,
    /// The chat's session is live (possibly just warm/idle between turns) —
    /// the engine refuses even with force, so no proceed button.
    Live,
    /// Checked out on the repo's default branch — a hard refusal.
    OnDefaultBranch,
    /// Nothing to lose — a normal destructive "Close out" (`force: false`).
    Clean,
    /// Dirty and/or unmerged (or unverifiable) — only "Force close out"
    /// (`force: true`).
    NeedsForce,
}

impl CloseoutVerdict {
    /// Whether any proceed button is shown.
    pub fn can_proceed(self) -> bool {
        matches!(self, Self::Clean | Self::NeedsForce)
    }

    /// The `force` flag the proceed button sends.
    pub fn force(self) -> bool {
        self == Self::NeedsForce
    }

    pub fn button_label(self) -> Option<&'static str> {
        match self {
            Self::Clean => Some("Close out"),
            Self::NeedsForce => Some("Force close out"),
            _ => None,
        }
    }
}

/// The engine refuses a non-forced close when it can't resolve the default
/// branch to verify a checked-out branch is merged — so that case needs force.
fn merge_unverifiable(plan: &CloseoutPlan) -> bool {
    plan.branch.is_some() && plan.default_branch.is_none()
}

/// Precedence mirrors the engine's own refusal order: live, not-a-worktree,
/// default branch (all hard), then the soft blockers.
pub fn verdict(plan: &CloseoutPlan) -> CloseoutVerdict {
    if plan.chat_live {
        CloseoutVerdict::Live
    } else if !plan.is_worktree {
        CloseoutVerdict::NotWorktree
    } else if plan.branch.is_some() && plan.branch == plan.default_branch {
        CloseoutVerdict::OnDefaultBranch
    } else if plan.dirty || plan.unmerged_commits > 0 || merge_unverifiable(plan) {
        CloseoutVerdict::NeedsForce
    } else {
        CloseoutVerdict::Clean
    }
}

fn plural(n: usize, singular: &str) -> String {
    format!("{n} {singular}{}", if n == 1 { "" } else { "s" })
}

/// `"12 uncommitted files"`; `"20+ uncommitted files"` when the engine's
/// capped list is full (the real count may be higher).
pub fn dirty_count_label(plan: &CloseoutPlan) -> String {
    let n = plan.dirty_files.len();
    if n == 0 {
        // Flag set with no entries — still worth saying.
        "Uncommitted changes".to_string()
    } else if n >= DIRTY_FILES_CAP {
        format!("{DIRTY_FILES_CAP}+ uncommitted files")
    } else {
        plural(n, "uncommitted file")
    }
}

/// The first [`DIRTY_FILES_SHOWN`] entries (trimmed porcelain lines) plus a
/// `"+N more"` tail when there are more (`"+N more"` becomes `"+N+ more"`
/// when the engine's cap truncated the list).
pub fn dirty_file_lines(plan: &CloseoutPlan) -> Vec<String> {
    let files = &plan.dirty_files;
    let mut lines: Vec<String> = files
        .iter()
        .take(DIRTY_FILES_SHOWN)
        .map(|f| f.trim().to_string())
        .collect();
    if files.len() > DIRTY_FILES_SHOWN {
        let more = files.len() - DIRTY_FILES_SHOWN;
        let capped = if files.len() >= DIRTY_FILES_CAP { "+" } else { "" };
        lines.push(format!("+{more}{capped} more"));
    }
    lines
}

/// `"3 unmerged commits on zeron/eng-1 (vs main)"`.
pub fn unmerged_label(plan: &CloseoutPlan) -> Option<String> {
    if plan.unmerged_commits == 0 {
        return None;
    }
    let branch = plan.branch.as_deref().unwrap_or("HEAD");
    let base = plural(plan.unmerged_commits as usize, "unmerged commit");
    Some(match plan.default_branch.as_deref() {
        Some(default) => format!("{base} on {branch} (vs {default})"),
        None => format!("{base} on {branch}"),
    })
}

/// Warning shown when merge status can't be verified (no default branch).
pub fn unverifiable_label(plan: &CloseoutPlan) -> Option<String> {
    (merge_unverifiable(plan) && plan.unmerged_commits == 0).then(|| {
        format!(
            "Couldn't resolve the default branch to verify {} is merged",
            plan.branch.as_deref().unwrap_or("HEAD")
        )
    })
}

/// `"<path> · <branch>"` (or `"… · detached HEAD"`).
pub fn summary_line(plan: &CloseoutPlan) -> String {
    let branch = plan.branch.as_deref().unwrap_or("detached HEAD");
    format!("{} \u{00B7} {branch}", plan.worktree_path)
}

/// Body copy per verdict.
pub fn body_copy(plan: &CloseoutPlan) -> String {
    match verdict(plan) {
        CloseoutVerdict::Live => "This chat's session is still live \u{2014} it may be running a \
             turn, or just warm and idle between turns. Stop or interrupt the chat first, \
             then close out its worktree."
            .to_string(),
        CloseoutVerdict::NotWorktree => {
            "This chat's folder isn't a closeable agent worktree, so there's nothing to close out."
                .to_string()
        }
        CloseoutVerdict::OnDefaultBranch => format!(
            "This worktree is on the repository's default branch ({}), which can't be closed out.",
            plan.branch.as_deref().unwrap_or("default")
        ),
        CloseoutVerdict::Clean => "Removes the worktree, deletes its branch, and archives this \
             chat. Everything is committed and merged."
            .to_string(),
        CloseoutVerdict::NeedsForce => "Force closing out permanently discards the uncommitted \
             work below, deletes the branch even though it isn't merged, removes the worktree, \
             and archives this chat. This can\u{2019}t be undone."
            .to_string(),
    }
}

/// A dialog-level reply is only applied while the dialog still shows the
/// request that issued it (not closed/reopened since).
pub fn reply_is_current(current: Option<u64>, request_id: u64) -> bool {
    current == Some(request_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn plan() -> CloseoutPlan {
        CloseoutPlan {
            is_worktree: true,
            worktree_path: "/r/.worktrees/workspace/eng-1".into(),
            branch: Some("zeron/eng-1".into()),
            chat_live: false,
            dirty: false,
            dirty_files: vec![],
            unmerged_commits: 0,
            default_branch: Some("main".into()),
        }
    }

    #[test]
    fn decode_plan_pins_wire_field_names() {
        let p = decode_plan(&json!({
            "isWorktree": true,
            "worktreePath": "/r/.worktrees/workspace/eng-1",
            "branch": "zeron/eng-1",
            "chatLive": false,
            "dirty": true,
            "dirtyFiles": [" M src/a.rs", "?? b.txt"],
            "unmergedCommits": 2,
            "defaultBranch": "main",
        }))
        .unwrap();
        assert_eq!(
            p,
            CloseoutPlan {
                is_worktree: true,
                worktree_path: "/r/.worktrees/workspace/eng-1".into(),
                branch: Some("zeron/eng-1".into()),
                chat_live: false,
                dirty: true,
                dirty_files: vec![" M src/a.rs".into(), "?? b.txt".into()],
                unmerged_commits: 2,
                default_branch: Some("main".into()),
            }
        );
    }

    #[test]
    fn decode_plan_not_worktree_shape_and_nulls() {
        let p = decode_plan(&json!({
            "isWorktree": false,
            "worktreePath": "",
            "branch": null,
            "chatLive": false,
            "dirty": false,
            "dirtyFiles": [],
            "unmergedCommits": 0,
            "defaultBranch": null,
        }))
        .unwrap();
        assert!(!p.is_worktree);
        assert_eq!(p.branch, None);
        assert_eq!(p.default_branch, None);
        assert_eq!(verdict(&p), CloseoutVerdict::NotWorktree);
    }

    #[test]
    fn decode_plan_requires_gating_flags() {
        assert!(decode_plan(&json!({"chatLive": false})).is_err());
        assert!(decode_plan(&json!({"isWorktree": true})).is_err());
        // snake_case is not the wire shape.
        assert!(decode_plan(&json!({"is_worktree": true, "chat_live": false})).is_err());
    }

    #[test]
    fn decode_outcome_pins_wire_field_names() {
        let o = decode_outcome(&json!({"removed": true, "branchDeleted": true, "archived": true}))
            .unwrap();
        assert_eq!(
            o,
            CloseoutOutcome {
                removed: true,
                branch_deleted: true,
                archived: true
            }
        );
        let partial = decode_outcome(&json!({"removed": true})).unwrap();
        assert!(!partial.branch_deleted && !partial.archived);
        assert!(decode_outcome(&json!({"branchDeleted": true})).is_err());
    }

    #[test]
    fn verdict_precedence() {
        assert_eq!(verdict(&plan()), CloseoutVerdict::Clean);

        let mut live = plan();
        live.chat_live = true;
        live.dirty = true;
        assert_eq!(verdict(&live), CloseoutVerdict::Live);
        assert!(!verdict(&live).can_proceed());

        let mut default = plan();
        default.branch = Some("main".into());
        default.unmerged_commits = 3;
        assert_eq!(verdict(&default), CloseoutVerdict::OnDefaultBranch);
        assert_eq!(verdict(&default).button_label(), None);

        let mut dirty = plan();
        dirty.dirty = true;
        dirty.dirty_files = vec!["?? x".into()];
        assert_eq!(verdict(&dirty), CloseoutVerdict::NeedsForce);
        assert!(verdict(&dirty).force());
        assert_eq!(verdict(&dirty).button_label(), Some("Force close out"));

        let mut unmerged = plan();
        unmerged.unmerged_commits = 1;
        assert_eq!(verdict(&unmerged), CloseoutVerdict::NeedsForce);

        let mut unverifiable = plan();
        unverifiable.default_branch = None;
        assert_eq!(verdict(&unverifiable), CloseoutVerdict::NeedsForce);

        // Detached HEAD with no default branch has nothing to verify.
        let mut detached = plan();
        detached.branch = None;
        detached.default_branch = None;
        assert_eq!(verdict(&detached), CloseoutVerdict::Clean);

        assert!(!CloseoutVerdict::Clean.force());
        assert_eq!(CloseoutVerdict::Clean.button_label(), Some("Close out"));
    }

    #[test]
    fn dirty_lines_cap_at_five_with_more_tail() {
        let mut p = plan();
        p.dirty_files = (0..3).map(|i| format!(" M f{i}.rs")).collect();
        assert_eq!(dirty_count_label(&p), "3 uncommitted files");
        assert_eq!(dirty_file_lines(&p), vec!["M f0.rs", "M f1.rs", "M f2.rs"]);

        p.dirty_files = vec!["?? one".into()];
        assert_eq!(dirty_count_label(&p), "1 uncommitted file");

        p.dirty_files = (0..8).map(|i| format!("?? f{i}")).collect();
        let lines = dirty_file_lines(&p);
        assert_eq!(lines.len(), 6);
        assert_eq!(lines[5], "+3 more");

        p.dirty_files = (0..DIRTY_FILES_CAP).map(|i| format!("?? f{i}")).collect();
        assert_eq!(dirty_count_label(&p), "20+ uncommitted files");
        assert_eq!(dirty_file_lines(&p).last().unwrap(), "+15+ more");

        p.dirty_files.clear();
        p.dirty = true;
        assert_eq!(dirty_count_label(&p), "Uncommitted changes");
        assert!(dirty_file_lines(&p).is_empty());
    }

    #[test]
    fn unmerged_and_unverifiable_labels() {
        let mut p = plan();
        assert_eq!(unmerged_label(&p), None);
        p.unmerged_commits = 2;
        assert_eq!(
            unmerged_label(&p).as_deref(),
            Some("2 unmerged commits on zeron/eng-1 (vs main)")
        );
        p.unmerged_commits = 1;
        p.default_branch = None;
        assert_eq!(unmerged_label(&p).as_deref(), Some("1 unmerged commit on zeron/eng-1"));
        assert_eq!(unverifiable_label(&p), None);
        p.unmerged_commits = 0;
        assert_eq!(
            unverifiable_label(&p).as_deref(),
            Some("Couldn't resolve the default branch to verify zeron/eng-1 is merged")
        );
        assert_eq!(unverifiable_label(&plan()), None);
    }

    #[test]
    fn summary_and_copy() {
        let mut p = plan();
        assert_eq!(summary_line(&p), "/r/.worktrees/workspace/eng-1 \u{00B7} zeron/eng-1");
        p.branch = None;
        assert!(summary_line(&p).ends_with("detached HEAD"));
        let mut live = plan();
        live.chat_live = true;
        assert!(body_copy(&live).contains("warm and idle"));
        let mut forced = plan();
        forced.dirty = true;
        let copy = body_copy(&forced);
        assert!(copy.contains("discards the uncommitted"));
        assert!(copy.contains("deletes the branch"));
    }

    #[test]
    fn workspace_root_predicate() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        assert!(!has_workspace_root(Some(cwd)));
        std::fs::write(dir.path().join(".workspace-root"), "/r\n").unwrap();
        assert!(has_workspace_root(Some(cwd)));
        assert!(!has_workspace_root(None));
        assert!(!has_workspace_root(Some("")));
        // A directory named like the marker doesn't count.
        let other = tempfile::tempdir().unwrap();
        std::fs::create_dir(other.path().join(".workspace-root")).unwrap();
        assert!(!has_workspace_root(other.path().to_str()));
    }

    #[test]
    fn stale_reply_guard() {
        assert!(reply_is_current(Some(3), 3));
        assert!(!reply_is_current(Some(4), 3));
        assert!(!reply_is_current(None, 3));
    }
}
