//! Read-only discovery for the repository topology canvas.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use chrono::Utc;
use sha2::{Digest, Sha256};
use zeron_proto::{
    Chat, ChatIndicator, HarnessId, RepositoryTopology, RepositoryTopologyActivity,
    RepositoryTopologyAgent, RepositoryTopologyChat, RepositoryTopologyKind,
    RepositoryTopologyRepository, RepositoryTopologyStatus, RepositoryTopologyWorkspace, Session,
    Space,
};

use crate::EngineError;
use crate::repos::{Repos, is_broad_workspace_root_resolved};

const NESTED_REPO_SCAN_MAX_DIRS: usize = 20_000;
const NESTED_REPO_SCAN_MAX_DEPTH: usize = 12;
const NESTED_REPO_SCAN_MAX_REPOS: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorktreeRecord {
    path: PathBuf,
    head_sha: Option<String>,
    branch: Option<String>,
    locked: bool,
    prunable: bool,
}

#[derive(Debug, Clone)]
struct RepoCandidate {
    path: PathBuf,
    kind: RepositoryTopologyKind,
    configured_parent_path: Option<PathBuf>,
}

/// Build a point-in-time graph for one local Space. This never fetches and
/// never mutates git state; ahead/behind uses only an already configured local
/// upstream.
pub async fn build(
    repos: &Repos,
    device_id: &str,
    space: &Space,
    chats: Vec<Chat>,
    sessions: Vec<Session>,
    linked_pr_urls: HashMap<String, Vec<String>>,
) -> Result<RepositoryTopology, EngineError> {
    let requested_root = PathBuf::from(&space.path);
    if is_broad_workspace_root_resolved(&requested_root) {
        return Err(EngineError::Other(
            "repository topology requires a specific project folder".into(),
        ));
    }
    let workspace_root = std::fs::canonicalize(&requested_root).unwrap_or(requested_root);

    let mut candidates = Vec::<RepoCandidate>::new();
    let mut candidate_paths = HashSet::<PathBuf>::new();
    if repos.is_repo(&workspace_root).await {
        let identity = repos.checkout_identity(&workspace_root).await?;
        candidate_paths.insert(identity.root.clone());
        candidates.push(RepoCandidate {
            path: identity.root,
            kind: RepositoryTopologyKind::Workspace,
            configured_parent_path: None,
        });
    }

    // Product scope includes embedded repositories as well as submodules. The
    // walk is bounded, does not follow symlinks, respects common heavyweight
    // build-directory boundaries, and merely looks for .git markers.
    let scan_root = workspace_root.clone();
    let nested = tokio::task::spawn_blocking(move || scan_nested_repo_markers(&scan_root))
        .await
        .map_err(|err| EngineError::Other(format!("repository scan join failed: {err}")))?;
    for path in nested {
        if candidate_paths.insert(path.clone()) {
            candidates.push(RepoCandidate {
                path,
                kind: RepositoryTopologyKind::Repository,
                configured_parent_path: None,
            });
        }
    }

    // Git config handles quoting/escaping in .gitmodules. Iterate because an
    // initialized submodule may itself declare more submodules; unavailable
    // submodules are retained as graph nodes with unknown state.
    let mut ix = 0;
    while ix < candidates.len() && candidates.len() < NESTED_REPO_SCAN_MAX_REPOS {
        let parent = candidates[ix].path.clone();
        ix += 1;
        if !repos.is_repo(&parent).await {
            continue;
        }
        for relative in submodule_paths(repos, &parent).await {
            let path = normalize_lexical(&parent.join(relative));
            let canonical = std::fs::canonicalize(&path).unwrap_or(path);
            if candidate_paths.insert(canonical.clone()) {
                candidates.push(RepoCandidate {
                    path: canonical,
                    kind: RepositoryTopologyKind::Submodule,
                    configured_parent_path: Some(parent.clone()),
                });
            } else if let Some(candidate) = candidates.iter_mut().find(|c| c.path == canonical) {
                candidate.kind = RepositoryTopologyKind::Submodule;
                candidate.configured_parent_path = Some(parent.clone());
            }
        }
    }

    candidates.sort_by_key(|candidate| candidate.path.components().count());
    let mut topology_repos = Vec::new();
    let mut repo_path_to_id = HashMap::<PathBuf, String>::new();
    let mut seen_repo_ids = HashSet::new();

    for candidate in candidates {
        let initialized = repos.is_repo(&candidate.path).await;
        let common_dir = if initialized {
            repos
                .git(
                    &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                    Some(&candidate.path),
                )
                .await
                .ok()
                .map(PathBuf::from)
                .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
        } else {
            None
        };
        let repo_id = stable_id(
            "repo",
            device_id,
            common_dir.as_deref().unwrap_or(&candidate.path),
        );
        repo_path_to_id.insert(candidate.path.clone(), repo_id.clone());
        // A linked worktree encountered by the bounded scan shares a common
        // git dir with its owning repository and is not a second repository.
        if !seen_repo_ids.insert(repo_id.clone()) {
            continue;
        }

        let parent_repository_id = candidate
            .configured_parent_path
            .as_ref()
            .and_then(|path| repo_path_to_id.get(path))
            .cloned()
            .or_else(|| nearest_parent_id(&candidate.path, &repo_path_to_id));

        let mut worktrees = Vec::new();
        if initialized {
            let raw = repos
                .git(
                    &["worktree", "list", "--porcelain", "-z"],
                    Some(&candidate.path),
                )
                .await?;
            for (worktree_ix, record) in parse_worktree_porcelain_z(&raw).into_iter().enumerate() {
                let canonical_path =
                    std::fs::canonicalize(&record.path).unwrap_or(record.path.clone());
                let checkout = repos.checkout_identity(&canonical_path).await.ok();
                let (status, ahead, behind) = inspect_checkout(repos, &canonical_path).await;
                let branch = record.branch.clone();
                worktrees.push(zeron_proto::RepositoryTopologyWorktree {
                    id: checkout
                        .as_ref()
                        .map(|identity| identity.id.clone())
                        .unwrap_or_else(|| stable_id("worktree", device_id, &canonical_path)),
                    name: canonical_path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| canonical_path.to_string_lossy().into_owned()),
                    path: canonical_path.to_string_lossy().into_owned(),
                    relative_path: relative_path(&workspace_root, &canonical_path),
                    branch,
                    head_sha: record.head_sha,
                    status,
                    ahead,
                    behind,
                    is_main: worktree_ix == 0,
                    locked: record.locked,
                    prunable: record.prunable,
                    checkout_id: checkout.map(|identity| identity.id),
                    chats: Vec::new(),
                });
            }
        }

        let main = worktrees.iter().find(|worktree| worktree.is_main);
        topology_repos.push(RepositoryTopologyRepository {
            id: repo_id,
            name: candidate
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| candidate.path.to_string_lossy().into_owned()),
            path: candidate.path.to_string_lossy().into_owned(),
            relative_path: relative_path(&workspace_root, &candidate.path),
            kind: candidate.kind,
            branch: main.and_then(|worktree| worktree.branch.clone()),
            head_sha: main.and_then(|worktree| worktree.head_sha.clone()),
            status: main
                .map(|worktree| worktree.status)
                .unwrap_or(RepositoryTopologyStatus::Unknown),
            ahead: main.and_then(|worktree| worktree.ahead),
            behind: main.and_then(|worktree| worktree.behind),
            parent_repository_id,
            worktrees,
        });
    }

    attach_chats(repos, &mut topology_repos, chats, sessions, linked_pr_urls).await;
    let root_repo = topology_repos
        .iter()
        .find(|repo| repo.kind == RepositoryTopologyKind::Workspace);
    let workspace = RepositoryTopologyWorkspace {
        id: space.id.clone(),
        name: space.display_name().to_string(),
        path: workspace_root.to_string_lossy().into_owned(),
        branch: root_repo.and_then(|repo| repo.branch.clone()),
        status: root_repo
            .map(|repo| repo.status)
            .unwrap_or(RepositoryTopologyStatus::Unknown),
        repositories: topology_repos,
    };
    Ok(RepositoryTopology { workspace })
}

async fn attach_chats(
    repos: &Repos,
    repositories: &mut [RepositoryTopologyRepository],
    chats: Vec<Chat>,
    sessions: Vec<Session>,
    linked_pr_urls: HashMap<String, Vec<String>>,
) {
    let session_by_chat: HashMap<_, _> = sessions
        .into_iter()
        .map(|session| (session.chat_id.clone(), session))
        .collect();
    let mut checkout_positions = HashMap::new();
    for (repo_ix, repo) in repositories.iter().enumerate() {
        for (worktree_ix, worktree) in repo.worktrees.iter().enumerate() {
            if let Some(checkout_id) = &worktree.checkout_id {
                checkout_positions.insert(checkout_id.clone(), (repo_ix, worktree_ix));
            }
        }
    }

    for chat in chats {
        let mut checkout_id = chat
            .source_context
            .as_ref()
            .map(|source| source.checkout_id.clone())
            .or_else(|| chat.checkout_id.clone());
        if checkout_id
            .as_ref()
            .is_none_or(|id| !checkout_positions.contains_key(id))
            && let Some(cwd) = &chat.cwd
            && let Ok(identity) = repos.checkout_identity(Path::new(cwd)).await
        {
            checkout_id = Some(identity.id);
        }
        let Some((repo_ix, worktree_ix)) = checkout_id
            .as_ref()
            .and_then(|id| checkout_positions.get(id))
            .copied()
        else {
            continue;
        };
        let live_branch = repositories[repo_ix].worktrees[worktree_ix]
            .branch
            .as_deref();
        let source_branch = chat
            .source_context
            .as_ref()
            .map(|source| source.branch.clone())
            .or_else(|| chat.branch.clone());
        let branch_mismatch = source_branch
            .as_deref()
            .is_some_and(|source| Some(source) != live_branch);
        let indicator =
            zeron_proto::view::display_status(&chat, session_by_chat.get(&chat.id), Utc::now());
        let activity = activity(indicator);
        let kind = chat.config.as_ref().map(|config| config.harness);
        let agent_name = kind.map(harness_name).unwrap_or("Agent").to_string();
        let topology_chat = RepositoryTopologyChat {
            id: chat.id.clone(),
            title: chat.title.clone().unwrap_or_else(|| "Untitled chat".into()),
            updated_at: chat.last_message_at.or(Some(chat.created_at)),
            indicator: activity,
            source_branch,
            branch_mismatch,
            archived: chat.archived,
            checkout_id,
            parent_chat_id: chat.parent_chat_id.clone(),
            linked_ticket_id: chat.linked_ticket_id.clone(),
            linked_pr_urls: linked_pr_urls.get(&chat.id).cloned().unwrap_or_default(),
            agents: vec![RepositoryTopologyAgent {
                id: format!("agent:{}", chat.id),
                name: agent_name,
                status: activity,
                kind,
            }],
        };
        repositories[repo_ix].worktrees[worktree_ix]
            .chats
            .push(topology_chat);
    }
}

async fn inspect_checkout(
    repos: &Repos,
    path: &Path,
) -> (RepositoryTopologyStatus, Option<usize>, Option<usize>) {
    let status = match repos
        .git(
            &["status", "--porcelain=v1", "-z", "--untracked-files=normal"],
            Some(path),
        )
        .await
    {
        Ok(output) if output.is_empty() => RepositoryTopologyStatus::Clean,
        Ok(output) if has_conflict(&output) => RepositoryTopologyStatus::Conflicted,
        Ok(_) => RepositoryTopologyStatus::Modified,
        Err(_) => RepositoryTopologyStatus::Unknown,
    };
    let divergence = repos
        .git(
            &["rev-list", "--left-right", "--count", "HEAD...@{upstream}"],
            Some(path),
        )
        .await
        .ok()
        .and_then(|counts| {
            let mut counts = counts.split_whitespace();
            Some((counts.next()?.parse().ok()?, counts.next()?.parse().ok()?))
        });
    let (ahead, behind) = divergence
        .map(|(ahead, behind)| (Some(ahead), Some(behind)))
        .unwrap_or((None, None));
    (status, ahead, behind)
}

fn has_conflict(status: &str) -> bool {
    status.split('\0').any(|entry| {
        let code = entry.as_bytes().get(..2).unwrap_or_default();
        matches!(code, b"DD" | b"AU" | b"UD" | b"UA" | b"DU" | b"AA" | b"UU")
    })
}

async fn submodule_paths(repos: &Repos, repo: &Path) -> Vec<PathBuf> {
    let file = repo.join(".gitmodules");
    if !file.is_file() {
        return Vec::new();
    }
    let file = file.to_string_lossy();
    repos
        .git(
            &[
                "config",
                "-z",
                "--file",
                &file,
                "--get-regexp",
                r"^submodule\..*\.path$",
            ],
            Some(repo),
        )
        .await
        .ok()
        .into_iter()
        .flat_map(|output| {
            output
                .split('\0')
                .filter_map(|record| record.split_once('\n').map(|(_, path)| PathBuf::from(path)))
                .collect::<Vec<_>>()
        })
        .collect()
}

fn parse_worktree_porcelain_z(output: &str) -> Vec<WorktreeRecord> {
    output
        .split("\0\0")
        .filter_map(|stanza| {
            let mut path = None;
            let mut head_sha = None;
            let mut branch = None;
            let mut locked = false;
            let mut prunable = false;
            for field in stanza.split('\0').filter(|field| !field.is_empty()) {
                if let Some(value) = field.strip_prefix("worktree ") {
                    path = Some(PathBuf::from(value));
                } else if let Some(value) = field.strip_prefix("HEAD ") {
                    head_sha = Some(value.to_string());
                } else if let Some(value) = field.strip_prefix("branch refs/heads/") {
                    branch = Some(value.to_string());
                } else if field == "locked" || field.starts_with("locked ") {
                    locked = true;
                } else if field == "prunable" || field.starts_with("prunable ") {
                    prunable = true;
                }
            }
            Some(WorktreeRecord {
                path: path?,
                head_sha,
                branch,
                locked,
                prunable,
            })
        })
        .collect()
}

fn scan_nested_repo_markers(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut queue = VecDeque::from([(root.to_path_buf(), 0usize)]);
    let mut visited = 0usize;
    while let Some((directory, depth)) = queue.pop_front() {
        if visited >= NESTED_REPO_SCAN_MAX_DIRS || found.len() >= NESTED_REPO_SCAN_MAX_REPOS {
            break;
        }
        visited += 1;
        if depth > 0 && std::fs::symlink_metadata(directory.join(".git")).is_ok() {
            found.push(std::fs::canonicalize(&directory).unwrap_or(directory.clone()));
        }
        if depth >= NESTED_REPO_SCAN_MAX_DEPTH {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if matches!(
                name.as_ref(),
                ".git" | ".worktrees" | "node_modules" | "target" | ".cache" | "build" | "dist"
            ) {
                continue;
            }
            if entry
                .file_type()
                .is_ok_and(|kind| kind.is_dir() && !kind.is_symlink())
            {
                queue.push_back((entry.path(), depth + 1));
            }
        }
    }
    found
}

fn nearest_parent_id(path: &Path, ids: &HashMap<PathBuf, String>) -> Option<String> {
    path.ancestors()
        .skip(1)
        .find_map(|ancestor| ids.get(ancestor).cloned())
}

fn relative_path(root: &Path, path: &Path) -> Option<String> {
    path.strip_prefix(root)
        .ok()
        .map(|relative| relative.to_string_lossy().into_owned())
        .filter(|relative| !relative.is_empty())
}

fn normalize_lexical(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn stable_id(prefix: &str, device_id: &str, path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(device_id.as_bytes());
    hasher.update([0]);
    hasher.update(path.to_string_lossy().as_bytes());
    format!("{prefix}:{}", crate::repos::hex(&hasher.finalize()))
}

fn activity(indicator: ChatIndicator) -> RepositoryTopologyActivity {
    match indicator {
        ChatIndicator::Working => RepositoryTopologyActivity::Working,
        ChatIndicator::AwaitingInput => RepositoryTopologyActivity::AwaitingInput,
        ChatIndicator::Errored => RepositoryTopologyActivity::Error,
        ChatIndicator::Completed => RepositoryTopologyActivity::Completed,
        ChatIndicator::Idle => RepositoryTopologyActivity::Idle,
    }
}

fn harness_name(harness: HarnessId) -> &'static str {
    match harness {
        HarnessId::ClaudeCode => "Claude Code",
        HarnessId::Codex => "Codex",
        HarnessId::Cursor => "Cursor",
        HarnessId::Devin => "Devin",
        HarnessId::Grok => "Grok",
        HarnessId::Hermes => "Hermes",
        HarnessId::Pi => "Pi",
        HarnessId::Opencode => "OpenCode",
        HarnessId::Antigravity => "Antigravity",
        HarnessId::Mock => "Mock agent",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::{ChatConfig, ConversationSourceContext, SandboxLevel, SessionStatus};

    fn git(cwd: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_repo(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        git(path, &["init", "-q"]);
        git(path, &["config", "user.email", "topology@example.com"]);
        git(path, &["config", "user.name", "Topology Test"]);
        std::fs::write(path.join("README.md"), "seed\n").unwrap();
        git(path, &["add", "README.md"]);
        git(path, &["commit", "-qm", "seed"]);
    }

    #[test]
    fn parses_detached_locked_and_prunable_worktrees() {
        let input = concat!(
            "worktree /repo\0HEAD abc\0branch refs/heads/main\0\0",
            "worktree /tmp/tree with spaces\0HEAD def\0detached\0locked reason\0prunable stale\0\0",
        );
        let records = parse_worktree_porcelain_z(input);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].branch.as_deref(), Some("main"));
        assert_eq!(records[1].path, PathBuf::from("/tmp/tree with spaces"));
        assert_eq!(records[1].branch, None);
        assert!(records[1].locked);
        assert!(records[1].prunable);
    }

    #[test]
    fn status_detects_unmerged_codes() {
        assert!(has_conflict("UU src/lib.rs\0"));
        assert!(has_conflict(" M normal\0AA both-added\0"));
        assert!(!has_conflict(" M src/lib.rs\0?? new.txt\0"));
    }

    #[test]
    fn lexical_normalization_handles_uninitialized_submodule_parent_segments() {
        assert_eq!(
            normalize_lexical(Path::new("/repo/crates/../vendor/tool")),
            PathBuf::from("/repo/vendor/tool")
        );
    }

    #[tokio::test]
    async fn builds_nested_repo_external_worktree_and_checkout_joined_chat() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        init_repo(&root);
        let nested = root.join("packages/nested");
        init_repo(&nested);
        std::fs::write(
            root.join(".gitmodules"),
            "[submodule \"missing\"]\n\tpath = deps/missing\n\turl = ../missing\n",
        )
        .unwrap();
        let linked = temp.path().join("external linked tree");
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature/topology",
                linked.to_str().unwrap(),
            ],
        );

        let repos = Repos::with_worktrees_root(
            &temp.path().join("data"),
            "device-1",
            temp.path().join("managed-worktrees"),
        );
        let checkout = repos.checkout_identity(&linked).await.unwrap();
        let now = Utc::now();
        let chat = Chat {
            id: "chat-1".into(),
            device_id: "device-1".into(),
            title: Some("Build topology".into()),
            archived: false,
            // Source checkout identity is authoritative even if this older
            // cwd field happens to point at the main checkout.
            cwd: Some(root.to_string_lossy().into_owned()),
            branch: Some("feature/topology".into()),
            checkout_id: None,
            source_context: Some(ConversationSourceContext {
                checkout_id: checkout.id.clone(),
                repo_root: linked.to_string_lossy().into_owned(),
                cwd: linked.to_string_lossy().into_owned(),
                branch: "feature/topology".into(),
                head_sha: None,
                observed_at: now,
            }),
            config: Some(ChatConfig {
                harness: HarnessId::Codex,
                model: None,
                reasoning: None,
                model_options: Default::default(),
                sandbox: SandboxLevel::WorkspaceWrite,
                auto_approve: false,
            }),
            last_message_preview: None,
            last_message_at: Some(now),
            created_at: now,
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: Some("space-1".into()),
            last_seen_at: Some(now),
            room_gen: None,
            parent_chat_id: None,
            linked_pr_url: None,
            linked_pr_source: None,
            linked_ticket_id: Some("ENG-42".into()),
            linked_ticket_source: None,
        };
        let session = Session {
            last_completed_turn: None,
            chat_id: chat.id.clone(),
            device_id: chat.device_id.clone(),
            status: SessionStatus::Working,
            started_at: Some(now),
            updated_at: now,
        };
        let space = Space {
            id: "space-1".into(),
            device_id: "device-1".into(),
            path: root.to_string_lossy().into_owned(),
            name: Some("Workspace".into()),
            git_detected: true,
            git_checked_at: Some(now),
            checkout_id: None,
            created_at: now,
        };

        let topology = build(
            &repos,
            "device-1",
            &space,
            vec![chat],
            vec![session],
            HashMap::from([(
                "chat-1".into(),
                vec!["https://github.com/acme/repo/pull/42".into()],
            )]),
        )
        .await
        .unwrap();

        assert_eq!(topology.workspace.name, "Workspace");
        assert!(topology.workspace.repositories.iter().any(|repo| {
            repo.kind == RepositoryTopologyKind::Repository
                && repo.path.ends_with("packages/nested")
        }));
        assert!(topology.workspace.repositories.iter().any(|repo| {
            repo.kind == RepositoryTopologyKind::Submodule && repo.path.ends_with("deps/missing")
        }));
        let root_repo = topology
            .workspace
            .repositories
            .iter()
            .find(|repo| repo.kind == RepositoryTopologyKind::Workspace)
            .unwrap();
        assert_eq!(root_repo.worktrees.len(), 2);
        let linked = root_repo
            .worktrees
            .iter()
            .find(|worktree| worktree.checkout_id.as_deref() == Some(checkout.id.as_str()))
            .unwrap();
        assert!(!linked.is_main);
        assert_eq!(linked.chats.len(), 1);
        assert_eq!(
            linked.chats[0].indicator,
            RepositoryTopologyActivity::Working
        );
        assert_eq!(linked.chats[0].agents[0].kind, Some(HarnessId::Codex));
        assert_eq!(linked.chats[0].linked_ticket_id.as_deref(), Some("ENG-42"));
        assert_eq!(linked.chats[0].linked_pr_urls.len(), 1);
    }
}
