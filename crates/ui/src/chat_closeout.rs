//! Chat close-out: tear down a chat's agent worktree (and its branch) from
//! the UI, then let the engine archive the chat.
//!
//! Two engine calls, both IPC-only:
//! - [`PLAN_CHAT_CLOSEOUT`] (read-only) feeds the confirmation dialog.
//! - [`CLOSE_CHAT_WORKTREE`] (destructive) runs on confirm; clean worktrees
//!   close in one step, while dirty/unmerged/unverifiable worktrees require a
//!   second, explicit permanent-discard confirmation before `force` is sent.
//!
//! The action is offered when a chat cwd cheaply looks like an agent worktree:
//! either it has Zeron's `.workspace-root` marker or its `.git` metadata is a
//! file, as Git uses for linked worktrees. The engine remains authoritative
//! about whether the candidate is actually safe and eligible to remove.
//! Everything here is pure so the decode/format/state logic is unit-testable
//! without a gpui context; the dialog itself lives in `shell/closeout_ui.rs`.

use serde_json::Value;
use zeron_proto::GitHistoryCommit;

/// `{chatId, cwd, force}` → `{removed, branchDeleted, archived}`; errors carry
/// human-readable refusal reasons.
pub use zeron_rpc::methods::CLOSE_CHAT_WORKTREE;
/// `{chatId, cwd}` → `{isWorktree, worktreePath, branch, chatLive, dirty,
/// dirtyFiles, unmergedCommits, defaultBranch}`.
pub use zeron_rpc::methods::PLAN_CHAT_CLOSEOUT;

/// The plan runs a handful of `git` commands plus a process scan.
pub const PLAN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// `git worktree remove` on a large tree can take a while.
pub const CLOSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The engine caps `dirtyFiles` at this many entries.
pub const DIRTY_FILES_CAP: usize = 20;
/// How many dirty filenames the dialog lists before "+N more".
pub const DIRTY_FILES_SHOWN: usize = 5;

/// Cheap visibility predicate for the "Close out worktree…" action.
///
/// Zeron-created worktrees may carry a `.workspace-root` marker. Imported or
/// otherwise ordinary linked worktrees instead have a `.git` *file* at their
/// root (main checkouts have a `.git` directory). This intentionally only
/// decides whether to expose the action; the engine plan validates that the
/// cwd is the chat's registered, removable linked worktree.
pub fn is_closeout_candidate(cwd: Option<&str>) -> bool {
    let Some(cwd) = cwd.filter(|c| !c.trim().is_empty()) else {
        return false;
    };
    let cwd = std::path::Path::new(cwd);
    if zeron_engine::repos::is_automatic_access_blocked(cwd) {
        return false;
    }
    cwd.join(".workspace-root").is_file() || cwd.join(".git").is_file()
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CloseoutPlan {
    pub is_worktree: bool,
    pub worktree_path: String,
    pub branch: Option<String>,
    pub chat_live: bool,
    pub dirty: bool,
    pub dirty_files: Vec<String>,
    /// Why dirty-state inspection could not be completed. This is a hard
    /// blocker because force cannot safely distinguish or preserve work.
    pub dirty_inspection_error: Option<String>,
    pub unmerged_commits: u32,
    /// Newest-first commit rows supplied by the engine. Older engines omit
    /// this field; the count still keeps their warning behavior intact.
    pub unmerged_commit_details: Vec<GitHistoryCommit>,
    pub default_branch: Option<String>,
    /// Why merge inspection could not be completed. Unlike dirty inspection,
    /// this is forceable after the destructive two-step acknowledgement.
    pub merge_inspection_error: Option<String>,
    /// Other active chats attached to this checkout. Planning still succeeds
    /// so the dialog can name them; the destructive close remains blocked.
    pub shared_chats: Vec<CloseoutChatReference>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseoutChatReference {
    pub id: String,
    pub title: String,
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
    let unmerged_commit_details = value
        .get("unmergedCommitDetails")
        .cloned()
        .map(serde_json::from_value::<Vec<GitHistoryCommit>>)
        .transpose()
        .map_err(|error| format!("invalid unmerged commit details: {error}"))?
        .unwrap_or_default();
    let shared_chats = value
        .get("sharedChats")
        .and_then(Value::as_array)
        .map(|chats| {
            chats
                .iter()
                .filter_map(|chat| {
                    let id = chat.get("id")?.as_str()?.to_string();
                    let title = chat
                        .get("title")
                        .and_then(Value::as_str)
                        .filter(|title| !title.trim().is_empty())
                        .unwrap_or("Untitled chat")
                        .to_string();
                    Some(CloseoutChatReference { id, title })
                })
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
        dirty_inspection_error: opt_str("dirtyInspectionError"),
        unmerged_commits: value
            .get("unmergedCommits")
            .and_then(Value::as_u64)
            .map(|n| n.min(u32::MAX as u64) as u32)
            .unwrap_or(0),
        unmerged_commit_details,
        default_branch: opt_str("defaultBranch"),
        merge_inspection_error: opt_str("mergeInspectionError"),
        shared_chats,
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
    /// Git could not establish whether uncommitted work exists. The engine
    /// refuses even with force, so the user must fix/retry inspection.
    DirtyInspectionFailed,
    /// One or more other active chats still own this checkout. The plan names
    /// them so the user can inspect/open them before trying again.
    InUse,
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

/// Pure proceed-state decision for the confirmation dialog. Keeping this
/// separate from the GPUI click handler makes the destructive two-step guard
/// straightforward to pin with tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseoutProceed {
    /// The plan cannot currently be closed out.
    Blocked,
    /// First force click: reveal the permanent-discard acknowledgement.
    ArmForce,
    /// Perform the close-out RPC with the supplied `force` flag.
    Submit { force: bool },
}

pub fn proceed_action(verdict: CloseoutVerdict, force_armed: bool) -> CloseoutProceed {
    match verdict {
        CloseoutVerdict::Clean => CloseoutProceed::Submit { force: false },
        CloseoutVerdict::NeedsForce if force_armed => CloseoutProceed::Submit { force: true },
        CloseoutVerdict::NeedsForce => CloseoutProceed::ArmForce,
        _ => CloseoutProceed::Blocked,
    }
}

/// The engine refuses a non-forced close when it can't resolve the default
/// branch. That makes merge safety unverifiable for both named branches and
/// detached HEADs, which may still contain unique commits.
fn merge_unverifiable(plan: &CloseoutPlan) -> bool {
    plan.default_branch.is_none() || plan.merge_inspection_error.is_some()
}

/// Permanent blockers first (not a worktree, default branch — stopping the
/// chat wouldn't help, so don't tell the user to), then the temporary one
/// (live), then the soft blockers `force` overrides. The engine checks
/// liveness first, but every one of these is a refusal there too, so the
/// order only decides which explanation the dialog leads with.
pub fn verdict(plan: &CloseoutPlan) -> CloseoutVerdict {
    if !plan.is_worktree {
        CloseoutVerdict::NotWorktree
    } else if plan.branch.is_some() && plan.branch == plan.default_branch {
        CloseoutVerdict::OnDefaultBranch
    } else if plan.chat_live {
        CloseoutVerdict::Live
    } else if plan.dirty_inspection_error.is_some() {
        CloseoutVerdict::DirtyInspectionFailed
    } else if !plan.shared_chats.is_empty() {
        CloseoutVerdict::InUse
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

/// The first [`DIRTY_FILES_SHOWN`] entries (worktree-relative paths — the
/// engine strips the porcelain status columns) plus a
/// `"+N more"` tail when there are more (`"+N more"` becomes `"+N+ more"`
/// when the engine's cap truncated the list).
pub fn dirty_file_lines(plan: &CloseoutPlan) -> Vec<String> {
    let files = &plan.dirty_files;
    let mut lines: Vec<String> = files.iter().take(DIRTY_FILES_SHOWN).cloned().collect();
    if files.len() > DIRTY_FILES_SHOWN {
        let more = files.len() - DIRTY_FILES_SHOWN;
        let capped = if files.len() >= DIRTY_FILES_CAP {
            "+"
        } else {
            ""
        };
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
    (merge_unverifiable(plan) && plan.unmerged_commits == 0).then(|| match plan.branch.as_deref() {
        _ if plan.merge_inspection_error.is_some() => format!(
            "Couldn\u{2019}t verify whether commits are merged: {}",
            plan.merge_inspection_error.as_deref().unwrap_or_default()
        ),
        Some(branch) => {
            format!("Couldn't resolve the default branch to verify {branch} is merged")
        }
        None => "Couldn't resolve the default branch to verify detached HEAD commits are merged"
            .to_string(),
    })
}

/// Warning shown when git could not establish whether the worktree is dirty.
pub fn dirty_inspection_label(plan: &CloseoutPlan) -> Option<String> {
    plan.dirty_inspection_error
        .as_deref()
        .map(|error| format!("Couldn\u{2019}t verify uncommitted changes: {error}"))
}

/// The dialog's `label: value` summary of what gets torn down. Empty for a
/// non-worktree (nothing to summarize).
pub fn summary_rows(plan: &CloseoutPlan) -> Vec<(&'static str, String)> {
    if !plan.is_worktree || plan.worktree_path.is_empty() {
        return Vec::new();
    }
    let mut rows = vec![
        ("Worktree", plan.worktree_path.clone()),
        (
            "Branch",
            plan.branch
                .clone()
                .unwrap_or_else(|| "detached HEAD".to_string()),
        ),
    ];
    if let Some(default) = plan.default_branch.as_deref() {
        rows.push(("Base", default.to_string()));
    }
    rows
}

/// Body copy per verdict. The forced variant names only what's actually at
/// stake (uncommitted work and/or an unmerged branch).
pub fn body_copy(plan: &CloseoutPlan) -> String {
    match verdict(plan) {
        CloseoutVerdict::Live => "This chat\u{2019}s session is still live \u{2014} it may be \
             running a turn, or just warm and idle between turns (or a claude process is \
             running in its folder). Stop the chat first, then close out its worktree."
            .to_string(),
        CloseoutVerdict::NotWorktree => "This chat\u{2019}s folder isn\u{2019}t a closeable agent \
             worktree, so there\u{2019}s nothing to close out."
            .to_string(),
        CloseoutVerdict::OnDefaultBranch => format!(
            "This worktree is on the repository\u{2019}s default branch ({}), which can\u{2019}t be \
             closed out.",
            plan.branch.as_deref().unwrap_or("default")
        ),
        CloseoutVerdict::DirtyInspectionFailed => "Couldn\u{2019}t safely inspect this worktree for uncommitted changes. Close out is blocked until inspection succeeds."
            .to_string(),
        CloseoutVerdict::InUse => {
            let count = plan.shared_chats.len();
            format!(
                "This worktree is still attached to {count} other active chat{}. Open or archive those chats before closing it out.",
                if count == 1 { "" } else { "s" }
            )
        }
        CloseoutVerdict::Clean => match plan.branch.as_deref() {
            Some(branch) => format!(
                "Removes the worktree, deletes its branch {branch}, and archives this chat. \
                 Everything is committed and merged."
            ),
            None => "Removes the worktree (detached HEAD \u{2014} no branch to delete) and \
                 archives this chat. There are no uncommitted changes."
                .to_string(),
        },
        CloseoutVerdict::NeedsForce => {
            let mut losses: Vec<String> = Vec::new();
            if plan.dirty {
                losses.push("permanently discards the uncommitted changes below".into());
            } else if plan.dirty_inspection_error.is_some() {
                losses.push(
                    "may permanently discard uncommitted changes that couldn\u{2019}t be inspected"
                        .into(),
                );
            }
            if let Some(branch) = plan.branch.as_deref() {
                losses.push(if plan.unmerged_commits > 0 {
                    format!("deletes {branch} even though it isn\u{2019}t merged")
                } else if merge_unverifiable(plan) {
                    format!("deletes {branch} even though it can\u{2019}t be verified as merged")
                } else {
                    format!("deletes {branch}")
                });
            } else if plan.unmerged_commits > 0 {
                losses.push(format!(
                    "discards {} on detached HEAD",
                    plural(plan.unmerged_commits as usize, "unique commit")
                ));
            } else if merge_unverifiable(plan) {
                losses.push(
                    "may discard commits on detached HEAD because merge status can\u{2019}t be verified"
                        .into(),
                );
            }
            losses.push("removes the worktree".into());
            format!(
                "Force closing out {}, and archives this chat. This can\u{2019}t be undone.",
                losses.join(", ")
            )
        }
    }
}

/// The sidebar notice after a successful close-out. The engine deletes the
/// branch best-effort (the worktree is already gone by then), so say when it
/// was kept; likewise when archiving the chat didn't take.
pub fn success_notice(branch: Option<&str>, outcome: &CloseoutOutcome) -> String {
    let mut note = match branch {
        Some(b) if outcome.branch_deleted => format!("Closed out worktree and deleted {b}"),
        Some(b) => format!("Closed out worktree; branch {b} was kept"),
        None => "Closed out worktree".to_string(),
    };
    if !outcome.archived {
        note.push_str(" (chat not archived)");
    }
    note
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
            dirty_inspection_error: None,
            unmerged_commits: 0,
            unmerged_commit_details: vec![],
            default_branch: Some("main".into()),
            merge_inspection_error: None,
            shared_chats: vec![],
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
            "dirtyFiles": ["src/a.rs", "b.txt"],
            "dirtyInspectionError": "git status timed out",
            "unmergedCommits": 2,
            "unmergedCommitDetails": [{
                "sha": "0123456789abcdef",
                "parentShas": ["fedcba9876543210"],
                "subject": "inspect close-out work",
                "authorName": "Test",
                "authorEmail": "test@example.com",
                "authoredAt": "2026-10-06T12:00:00-04:00",
                "refs": [],
            }],
            "defaultBranch": "main",
            "mergeInspectionError": "rev-list timed out",
            "sharedChats": [{"id": "chat-2", "title": "Other work"}],
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
                dirty_files: vec!["src/a.rs".into(), "b.txt".into()],
                dirty_inspection_error: Some("git status timed out".into()),
                unmerged_commits: 2,
                unmerged_commit_details: vec![GitHistoryCommit {
                    sha: "0123456789abcdef".into(),
                    parent_shas: vec!["fedcba9876543210".into()],
                    subject: "inspect close-out work".into(),
                    author_name: "Test".into(),
                    author_email: "test@example.com".into(),
                    authored_at: "2026-10-06T12:00:00-04:00".into(),
                    refs: vec![],
                }],
                default_branch: Some("main".into()),
                merge_inspection_error: Some("rev-list timed out".into()),
                shared_chats: vec![CloseoutChatReference {
                    id: "chat-2".into(),
                    title: "Other work".into(),
                }],
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
        assert_eq!(p.dirty_inspection_error, None);
        assert_eq!(p.merge_inspection_error, None);
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
        assert_eq!(verdict(&live).button_label(), None);

        let mut in_use = plan();
        in_use.shared_chats.push(CloseoutChatReference {
            id: "other".into(),
            title: "Other work".into(),
        });
        assert_eq!(verdict(&in_use), CloseoutVerdict::InUse);
        assert!(!verdict(&in_use).can_proceed());
        assert!(body_copy(&in_use).contains("other active chat"));

        let mut default = plan();
        default.branch = Some("main".into());
        default.unmerged_commits = 3;
        assert_eq!(verdict(&default), CloseoutVerdict::OnDefaultBranch);
        assert_eq!(verdict(&default).button_label(), None);

        // Permanent blockers lead over liveness: stopping the chat wouldn't
        // make these closeable, so the dialog mustn't suggest it.
        default.chat_live = true;
        assert_eq!(verdict(&default), CloseoutVerdict::OnDefaultBranch);
        let not_wt = CloseoutPlan {
            chat_live: true,
            ..CloseoutPlan::default()
        };
        assert_eq!(verdict(&not_wt), CloseoutVerdict::NotWorktree);
        assert!(!verdict(&not_wt).can_proceed());

        let mut dirty = plan();
        dirty.dirty = true;
        dirty.dirty_files = vec!["x".into()];
        assert_eq!(verdict(&dirty), CloseoutVerdict::NeedsForce);
        assert!(verdict(&dirty).force());
        assert_eq!(verdict(&dirty).button_label(), Some("Force close out"));

        let mut unmerged = plan();
        unmerged.unmerged_commits = 1;
        assert_eq!(verdict(&unmerged), CloseoutVerdict::NeedsForce);

        let mut unverifiable = plan();
        unverifiable.default_branch = None;
        assert_eq!(verdict(&unverifiable), CloseoutVerdict::NeedsForce);

        // Detached HEAD may still contain unique commits. Without a default
        // branch the engine cannot prove removing it is safe.
        let mut detached = plan();
        detached.branch = None;
        detached.default_branch = None;
        assert_eq!(verdict(&detached), CloseoutVerdict::NeedsForce);

        assert!(!CloseoutVerdict::Clean.force());
        assert_eq!(CloseoutVerdict::Clean.button_label(), Some("Close out"));
    }

    #[test]
    fn destructive_proceed_requires_two_steps_but_clean_does_not() {
        assert_eq!(
            proceed_action(CloseoutVerdict::Clean, false),
            CloseoutProceed::Submit { force: false }
        );
        assert_eq!(
            proceed_action(CloseoutVerdict::NeedsForce, false),
            CloseoutProceed::ArmForce
        );
        assert_eq!(
            proceed_action(CloseoutVerdict::NeedsForce, true),
            CloseoutProceed::Submit { force: true }
        );
        assert_eq!(
            proceed_action(CloseoutVerdict::Live, true),
            CloseoutProceed::Blocked
        );
        assert_eq!(
            proceed_action(CloseoutVerdict::DirtyInspectionFailed, true),
            CloseoutProceed::Blocked
        );
    }

    #[test]
    fn dirty_lines_cap_at_five_with_more_tail() {
        let mut p = plan();
        p.dirty_files = (0..3).map(|i| format!("src/f{i}.rs")).collect();
        assert_eq!(dirty_count_label(&p), "3 uncommitted files");
        assert_eq!(
            dirty_file_lines(&p),
            vec!["src/f0.rs", "src/f1.rs", "src/f2.rs"]
        );

        p.dirty_files = vec!["one".into()];
        assert_eq!(dirty_count_label(&p), "1 uncommitted file");

        p.dirty_files = (0..8).map(|i| format!("f{i}")).collect();
        let lines = dirty_file_lines(&p);
        assert_eq!(lines.len(), 6);
        assert_eq!(lines[5], "+3 more");

        p.dirty_files = (0..DIRTY_FILES_CAP).map(|i| format!("f{i}")).collect();
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
        assert_eq!(
            unmerged_label(&p).as_deref(),
            Some("1 unmerged commit on zeron/eng-1")
        );
        assert_eq!(unverifiable_label(&p), None);
        p.unmerged_commits = 0;
        assert_eq!(
            unverifiable_label(&p).as_deref(),
            Some("Couldn't resolve the default branch to verify zeron/eng-1 is merged")
        );
        p.branch = None;
        assert_eq!(
            unverifiable_label(&p).as_deref(),
            Some("Couldn't resolve the default branch to verify detached HEAD commits are merged")
        );
        assert_eq!(unverifiable_label(&plan()), None);

        let mut inspection_failed = plan();
        inspection_failed.dirty_inspection_error = Some("git status timed out".into());
        assert_eq!(
            dirty_inspection_label(&inspection_failed).as_deref(),
            Some("Couldn\u{2019}t verify uncommitted changes: git status timed out")
        );
        assert_eq!(
            verdict(&inspection_failed),
            CloseoutVerdict::DirtyInspectionFailed
        );

        let mut merge_failed = plan();
        merge_failed.merge_inspection_error = Some("rev-list timed out".into());
        assert_eq!(verdict(&merge_failed), CloseoutVerdict::NeedsForce);
        assert_eq!(
            unverifiable_label(&merge_failed).as_deref(),
            Some("Couldn\u{2019}t verify whether commits are merged: rev-list timed out")
        );
    }

    #[test]
    fn summary_and_copy() {
        let mut p = plan();
        assert_eq!(
            summary_rows(&p),
            vec![
                ("Worktree", "/r/.worktrees/workspace/eng-1".to_string()),
                ("Branch", "zeron/eng-1".to_string()),
                ("Base", "main".to_string()),
            ]
        );
        p.branch = None;
        p.default_branch = None;
        assert_eq!(summary_rows(&p)[1], ("Branch", "detached HEAD".to_string()));
        assert_eq!(summary_rows(&p).len(), 2);
        assert!(summary_rows(&CloseoutPlan::default()).is_empty());
        let mut live = plan();
        live.chat_live = true;
        let copy = body_copy(&live);
        assert!(copy.contains("warm and idle"));
        assert!(copy.contains("Stop the chat first"));

        assert!(body_copy(&plan()).contains("deletes its branch zeron/eng-1"));
        let mut detached = plan();
        detached.branch = None;
        detached.default_branch = Some("main".into());
        assert!(body_copy(&detached).contains("no branch to delete"));

        let mut detached_unverifiable = detached.clone();
        detached_unverifiable.default_branch = None;
        let copy = body_copy(&detached_unverifiable);
        assert!(copy.contains("may discard commits on detached HEAD"));
        assert!(copy.contains("merge status can\u{2019}t be verified"));

        // Dirty only: work is lost, branch is deleted (merged, so no caveat).
        let mut dirty = plan();
        dirty.dirty = true;
        dirty.dirty_files = vec!["src/a.rs".into()];
        let copy = body_copy(&dirty);
        assert!(copy.contains("permanently discards the uncommitted changes"));
        assert!(copy.contains("deletes zeron/eng-1,"));
        assert!(!copy.contains("isn\u{2019}t merged"));
        assert!(copy.ends_with("and archives this chat. This can\u{2019}t be undone."));

        // Unmerged only: no "uncommitted" claim, branch-loss caveat.
        let mut unmerged = plan();
        unmerged.unmerged_commits = 2;
        let copy = body_copy(&unmerged);
        assert!(!copy.contains("uncommitted"));
        assert!(copy.contains("deletes zeron/eng-1 even though it isn\u{2019}t merged"));

        let mut both = dirty.clone();
        both.unmerged_commits = 1;
        let copy = body_copy(&both);
        assert!(copy.contains("uncommitted") && copy.contains("isn\u{2019}t merged"));

        let mut unverifiable = plan();
        unverifiable.default_branch = None;
        assert!(body_copy(&unverifiable).contains("can\u{2019}t be verified as merged"));

        // Dirty on a detached HEAD: nothing about a branch.
        let mut dirty_detached = dirty.clone();
        dirty_detached.branch = None;
        assert!(!body_copy(&dirty_detached).contains("deletes"));

        let mut unmerged_detached = plan();
        unmerged_detached.branch = None;
        unmerged_detached.unmerged_commits = 2;
        let copy = body_copy(&unmerged_detached);
        assert!(copy.contains("discards 2 unique commits on detached HEAD"));

        let mut inspection_failed = plan();
        inspection_failed.dirty_inspection_error = Some("git status timed out".into());
        let copy = body_copy(&inspection_failed);
        assert!(copy.contains("Close out is blocked until inspection succeeds"));

        let mut merge_failed = plan();
        merge_failed.merge_inspection_error = Some("rev-list timed out".into());
        let copy = body_copy(&merge_failed);
        assert!(copy.contains("can\u{2019}t be verified as merged"));
    }

    #[test]
    fn closeout_candidate_accepts_marker_or_linked_worktree_git_file() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        assert!(!is_closeout_candidate(Some(cwd)));
        std::fs::write(dir.path().join(".workspace-root"), "/r\n").unwrap();
        assert!(is_closeout_candidate(Some(cwd)));

        let linked = tempfile::tempdir().unwrap();
        std::fs::write(
            linked.path().join(".git"),
            "gitdir: /repo/.git/worktrees/wt\n",
        )
        .unwrap();
        assert!(is_closeout_candidate(linked.path().to_str()));
    }

    #[test]
    fn closeout_candidate_rejects_missing_empty_and_directory_sentinels() {
        assert!(!is_closeout_candidate(None));
        assert!(!is_closeout_candidate(Some("")));
        assert!(!is_closeout_candidate(Some("   ")));

        let unrelated = tempfile::tempdir().unwrap();
        assert!(!is_closeout_candidate(unrelated.path().to_str()));

        // Main checkouts use a `.git` directory, which is not closeable.
        std::fs::create_dir(unrelated.path().join(".git")).unwrap();
        std::fs::create_dir(unrelated.path().join(".workspace-root")).unwrap();
        assert!(!is_closeout_candidate(unrelated.path().to_str()));
    }

    #[test]
    fn closeout_candidate_rejects_privacy_managed_history_before_metadata_probes() {
        let home = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .expect("test requires HOME");
        for child in ["Music", "Pictures", "Documents"] {
            let cwd = home.join(child).join("historical-chat");
            assert!(!is_closeout_candidate(cwd.to_str()));
        }

        let ordinary = tempfile::tempdir().unwrap();
        std::fs::write(ordinary.path().join(".workspace-root"), "/r\n").unwrap();
        assert!(is_closeout_candidate(ordinary.path().to_str()));
    }

    #[test]
    fn success_notice_reports_kept_branch_and_archive_miss() {
        let full = CloseoutOutcome {
            removed: true,
            branch_deleted: true,
            archived: true,
        };
        assert_eq!(
            success_notice(Some("zeron/eng-1"), &full),
            "Closed out worktree and deleted zeron/eng-1"
        );
        let kept = CloseoutOutcome {
            branch_deleted: false,
            ..full
        };
        assert_eq!(
            success_notice(Some("zeron/eng-1"), &kept),
            "Closed out worktree; branch zeron/eng-1 was kept"
        );
        let unarchived = CloseoutOutcome {
            branch_deleted: false,
            archived: false,
            ..full
        };
        assert_eq!(
            success_notice(None, &unarchived),
            "Closed out worktree (chat not archived)"
        );
    }

    #[test]
    fn stale_reply_guard() {
        assert!(reply_is_current(Some(3), 3));
        assert!(!reply_is_current(Some(4), 3));
        assert!(!reply_is_current(None, 3));
    }
}
