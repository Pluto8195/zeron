//! Repository topology: a read-only, spatial view of one workspace's checkout
//! tree. The RPC wire types stop at [`TopologyAdapter`]; layout and rendering
//! consume the smaller [`TopologySnapshot`] view model so the engine contract
//! can evolve without leaking transport details through the UI.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gpui::{
    Bounds, Context, Entity, MouseButton, MouseMoveEvent, MouseUpEvent, PathBuilder, PinchEvent,
    Pixels, ScrollWheelEvent, SharedString, Window, div, point, prelude::*, px,
};
use serde::Deserialize;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::shell::Shell;
use crate::state::AppState;
use crate::theme::Theme;

const COLUMN_GAP: f32 = 88.0;
const ROW_GAP: f32 = 24.0;
const DENSE_WORKTREE_THRESHOLD: usize = 4;
const WORKTREE_GRID_COLUMN_GAP: f32 = 28.0;
const WORKTREE_GRID_ROW_GAP: f32 = 12.0;
const GRAPH_PAD: f32 = 52.0;
const MIN_ZOOM: f32 = 0.35;
const MAX_ZOOM: f32 = 1.8;
const LIVE_REFRESH: std::time::Duration = std::time::Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
enum RepositoryKind {
    Workspace,
    Repository,
    Submodule,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
enum TopologyStatus {
    Clean,
    Modified,
    Conflicted,
    Working,
    AwaitingInput,
    #[serde(alias = "errored")]
    Error,
    Completed,
    Idle,
    #[default]
    #[serde(other)]
    Unknown,
}

impl TopologyStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Modified => "uncommitted",
            Self::Conflicted => "conflicts",
            Self::Working => "working",
            Self::AwaitingInput => "needs input",
            Self::Error => "error",
            Self::Completed => "done",
            Self::Idle => "idle",
            Self::Unknown => "unknown",
        }
    }

    fn color(self, theme: &Theme) -> gpui::Hsla {
        match self {
            Self::Clean | Self::Completed => theme.success,
            Self::Modified => theme.warning,
            Self::Conflicted | Self::AwaitingInput | Self::Error => theme.danger,
            Self::Working => theme.busy,
            Self::Idle | Self::Unknown => theme.text_faint,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepositoryTopologyWire {
    workspace: WorkspaceWire,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceWire {
    id: String,
    name: String,
    path: String,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    status: TopologyStatus,
    #[serde(default)]
    repositories: Vec<RepositoryWire>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepositoryWire {
    id: String,
    name: String,
    path: String,
    #[serde(default)]
    relative_path: Option<String>,
    #[serde(default)]
    kind: Option<RepositoryKind>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    head_sha: Option<String>,
    #[serde(default)]
    status: TopologyStatus,
    #[serde(default)]
    ahead: Option<u64>,
    #[serde(default)]
    behind: Option<u64>,
    #[serde(default)]
    parent_repository_id: Option<String>,
    #[serde(default)]
    repositories: Vec<RepositoryWire>,
    #[serde(default)]
    worktrees: Vec<WorktreeWire>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorktreeWire {
    id: String,
    #[serde(default)]
    checkout_id: Option<String>,
    name: String,
    path: String,
    #[serde(default)]
    relative_path: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    head_sha: Option<String>,
    #[serde(default)]
    status: TopologyStatus,
    #[serde(default)]
    ahead: Option<u64>,
    #[serde(default)]
    behind: Option<u64>,
    #[serde(default)]
    is_main: bool,
    #[serde(default)]
    locked: bool,
    #[serde(default)]
    prunable: bool,
    #[serde(default)]
    chats: Vec<TopologyChatWire>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TopologyChatWire {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    indicator: TopologyStatus,
    #[serde(default)]
    source_branch: Option<String>,
    #[serde(default)]
    branch_mismatch: bool,
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    checkout_id: Option<String>,
    #[serde(default)]
    parent_chat_id: Option<String>,
    #[serde(default)]
    linked_ticket_id: Option<String>,
    #[serde(default)]
    linked_pr_urls: Vec<String>,
    #[serde(default)]
    agents: Vec<TopologyAgentWire>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TopologyAgentWire {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default, alias = "indicator")]
    status: TopologyStatus,
}

/// Rendering-owned model. It intentionally contains no engine/proto types.
#[derive(Debug, Clone, PartialEq)]
struct TopologySnapshot {
    workspace_id: String,
    workspace_name: String,
    workspace_path: String,
    workspace_branch: Option<String>,
    workspace_status: TopologyStatus,
    repositories: Vec<TopologyRepository>,
}

#[derive(Debug, Clone, PartialEq)]
struct TopologyRepository {
    id: String,
    parent_id: Option<String>,
    name: String,
    path: String,
    full_path: String,
    kind: RepositoryKind,
    branch: Option<String>,
    head_sha: Option<String>,
    status: TopologyStatus,
    ahead: Option<u64>,
    behind: Option<u64>,
    worktrees: Vec<TopologyWorktree>,
}

#[derive(Debug, Clone, PartialEq)]
struct TopologyWorktree {
    id: String,
    name: String,
    path: String,
    full_path: String,
    branch: Option<String>,
    head_sha: Option<String>,
    status: TopologyStatus,
    ahead: Option<u64>,
    behind: Option<u64>,
    is_main: bool,
    locked: bool,
    prunable: bool,
    chats: Vec<TopologyChat>,
}

#[derive(Debug, Clone, PartialEq)]
struct TopologyChat {
    id: String,
    title: String,
    status: TopologyStatus,
    source_branch: Option<String>,
    branch_mismatch: bool,
    archived: bool,
    checkout_id: Option<String>,
    parent_chat_id: Option<String>,
    linked_ticket_id: Option<String>,
    linked_pr_urls: Vec<String>,
    agents: Vec<TopologyAgent>,
}

#[derive(Debug, Clone, PartialEq)]
struct TopologyAgent {
    id: String,
    name: String,
    kind: Option<String>,
    status: TopologyStatus,
}

#[derive(Debug, Clone, PartialEq)]
struct TopologySearchResult {
    snapshot: TopologySnapshot,
    direct_matches: usize,
    focus: Option<TopologySelection>,
    matching_chat_ids: Vec<String>,
}

fn topology_search(snapshot: &TopologySnapshot, query: &str) -> TopologySearchResult {
    let terms = query
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>();
    if terms.is_empty() {
        return TopologySearchResult {
            snapshot: snapshot.clone(),
            direct_matches: 0,
            focus: None,
            matching_chat_ids: Vec::new(),
        };
    }

    if searchable_fields_match(
        &terms,
        [
            snapshot.workspace_name.as_str(),
            snapshot.workspace_path.as_str(),
        ],
    ) {
        return TopologySearchResult {
            snapshot: snapshot.clone(),
            direct_matches: snapshot
                .repositories
                .iter()
                .map(|repository| 1 + repository.worktrees.len())
                .sum(),
            focus: None,
            matching_chat_ids: Vec::new(),
        };
    }

    let mut direct_repositories = HashSet::new();
    let mut direct_worktrees = HashMap::<String, HashSet<String>>::new();
    let mut direct_selections = Vec::new();
    let mut matching_chat_ids = Vec::new();
    let mut direct_matches = 0;
    for repository in &snapshot.repositories {
        if repository_matches(repository, &terms) {
            direct_repositories.insert(repository.id.clone());
            direct_selections.push(TopologySelection::Repository(repository.id.clone()));
            direct_matches += 1;
        }
        for worktree in &repository.worktrees {
            let matched_chats = worktree
                .chats
                .iter()
                .filter(|chat| chat_matches(chat, &terms))
                .map(|chat| chat.id.clone())
                .collect::<Vec<_>>();
            if worktree_metadata_matches(worktree, &terms) || !matched_chats.is_empty() {
                direct_worktrees
                    .entry(repository.id.clone())
                    .or_default()
                    .insert(worktree.id.clone());
                direct_selections.push(TopologySelection::Worktree {
                    repository_id: repository.id.clone(),
                    worktree_id: worktree.id.clone(),
                });
                matching_chat_ids.extend(matched_chats);
                direct_matches += 1;
            }
        }
    }

    let parents = snapshot
        .repositories
        .iter()
        .map(|repository| (repository.id.as_str(), repository.parent_id.as_deref()))
        .collect::<HashMap<_, _>>();
    let mut included_repositories = direct_repositories.clone();
    included_repositories.extend(direct_worktrees.keys().cloned());
    let mut pending = included_repositories.iter().cloned().collect::<Vec<_>>();
    while let Some(id) = pending.pop() {
        if let Some(Some(parent_id)) = parents.get(id.as_str()) {
            if included_repositories.insert((*parent_id).to_string()) {
                pending.push((*parent_id).to_string());
            }
        }
    }

    let repositories = snapshot
        .repositories
        .iter()
        .filter(|repository| included_repositories.contains(&repository.id))
        .map(|repository| {
            let mut repository = repository.clone();
            if !direct_repositories.contains(&repository.id) {
                let matching = direct_worktrees.get(&repository.id);
                repository
                    .worktrees
                    .retain(|worktree| matching.is_some_and(|ids| ids.contains(&worktree.id)));
            }
            repository
        })
        .collect();

    TopologySearchResult {
        snapshot: TopologySnapshot {
            workspace_id: snapshot.workspace_id.clone(),
            workspace_name: snapshot.workspace_name.clone(),
            workspace_path: snapshot.workspace_path.clone(),
            workspace_branch: snapshot.workspace_branch.clone(),
            workspace_status: snapshot.workspace_status,
            repositories,
        },
        direct_matches,
        focus: (direct_selections.len() == 1).then(|| direct_selections.remove(0)),
        matching_chat_ids,
    }
}

fn searchable_fields_match<'a>(
    terms: &[String],
    fields: impl IntoIterator<Item = &'a str>,
) -> bool {
    let haystack = fields
        .into_iter()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join("\n");
    terms.iter().all(|term| haystack.contains(term))
}

fn repository_matches(repository: &TopologyRepository, terms: &[String]) -> bool {
    let mut fields = vec![
        repository.id.as_str(),
        repository.name.as_str(),
        repository.path.as_str(),
        repository.full_path.as_str(),
    ];
    fields.extend(repository.branch.as_deref());
    fields.extend(repository.head_sha.as_deref());
    searchable_fields_match(terms, fields)
}

fn worktree_metadata_matches(worktree: &TopologyWorktree, terms: &[String]) -> bool {
    let mut fields = vec![
        worktree.id.as_str(),
        worktree.name.as_str(),
        worktree.path.as_str(),
        worktree.full_path.as_str(),
    ];
    fields.extend(worktree.branch.as_deref());
    fields.extend(worktree.head_sha.as_deref());
    searchable_fields_match(terms, fields)
}

fn chat_matches(chat: &TopologyChat, terms: &[String]) -> bool {
    let mut fields = vec![chat.id.as_str(), chat.title.as_str()];
    fields.extend(chat.source_branch.as_deref());
    fields.extend(chat.checkout_id.as_deref());
    fields.extend(chat.linked_ticket_id.as_deref());
    fields.extend(chat.linked_pr_urls.iter().map(String::as_str));
    searchable_fields_match(terms, fields)
}

struct TopologyAdapter;

impl TopologyAdapter {
    fn decode(value: serde_json::Value) -> Result<TopologySnapshot, String> {
        let wire: RepositoryTopologyWire =
            serde_json::from_value(value).map_err(|error| error.to_string())?;
        let mut repositories = Vec::new();
        for repository in wire.workspace.repositories {
            Self::flatten_repository(repository, None, &mut repositories);
        }
        Ok(TopologySnapshot {
            workspace_id: wire.workspace.id,
            workspace_name: wire.workspace.name,
            workspace_path: wire.workspace.path,
            workspace_branch: wire.workspace.branch,
            workspace_status: wire.workspace.status,
            repositories,
        })
    }

    fn flatten_repository(
        repository: RepositoryWire,
        nested_parent: Option<String>,
        out: &mut Vec<TopologyRepository>,
    ) {
        let id = repository.id;
        let parent_id = repository.parent_repository_id.or(nested_parent);
        let full_path = repository.path;
        let path = repository
            .relative_path
            .unwrap_or_else(|| full_path.clone());
        let nested = repository.repositories;
        let worktrees = repository
            .worktrees
            .into_iter()
            .map(|worktree| TopologyWorktree {
                id: worktree.checkout_id.unwrap_or(worktree.id),
                name: worktree.name,
                path: worktree
                    .relative_path
                    .unwrap_or_else(|| worktree.path.clone()),
                full_path: worktree.path,
                branch: worktree.branch,
                head_sha: worktree.head_sha,
                status: worktree.status,
                ahead: worktree.ahead,
                behind: worktree.behind,
                is_main: worktree.is_main,
                locked: worktree.locked,
                prunable: worktree.prunable,
                chats: worktree
                    .chats
                    .into_iter()
                    .map(|chat| TopologyChat {
                        id: chat.id.clone(),
                        title: chat
                            .title
                            .filter(|title| !title.trim().is_empty())
                            .unwrap_or_else(|| "New chat".into()),
                        status: chat.indicator,
                        source_branch: chat.source_branch,
                        branch_mismatch: chat.branch_mismatch,
                        archived: chat.archived,
                        checkout_id: chat.checkout_id,
                        parent_chat_id: chat.parent_chat_id,
                        linked_ticket_id: chat.linked_ticket_id,
                        linked_pr_urls: chat.linked_pr_urls,
                        agents: chat
                            .agents
                            .into_iter()
                            .map(|agent| TopologyAgent {
                                id: agent.id.clone(),
                                name: agent
                                    .name
                                    .filter(|name| !name.trim().is_empty())
                                    .unwrap_or(agent.id),
                                kind: agent.kind,
                                status: agent.status,
                            })
                            .collect(),
                    })
                    .collect(),
            })
            .collect();
        out.push(TopologyRepository {
            id: id.clone(),
            parent_id,
            name: repository.name,
            path,
            full_path,
            kind: repository.kind.unwrap_or(RepositoryKind::Repository),
            branch: repository.branch,
            head_sha: repository.head_sha,
            status: repository.status,
            ahead: repository.ahead,
            behind: repository.behind,
            worktrees,
        });
        for child in nested {
            Self::flatten_repository(child, Some(id.clone()), out);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GraphNodeKind {
    Workspace,
    Repository(RepositoryKind),
    Worktree,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TopologySelection {
    Repository(String),
    Worktree {
        repository_id: String,
        worktree_id: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct OccupancySummary {
    chats: usize,
    agents: usize,
    working: usize,
    awaiting_input: usize,
    errors: usize,
    archived: usize,
    branch_mismatches: usize,
}

impl OccupancySummary {
    fn from_chats(chats: &[TopologyChat]) -> Self {
        let mut summary = Self {
            chats: chats.len(),
            agents: chats.iter().map(|chat| chat.agents.len()).sum(),
            ..Self::default()
        };
        for chat in chats {
            summary.working += usize::from(chat.status == TopologyStatus::Working);
            summary.awaiting_input += usize::from(chat.status == TopologyStatus::AwaitingInput);
            summary.errors += usize::from(chat.status == TopologyStatus::Error);
            summary.archived += usize::from(chat.archived);
            summary.branch_mismatches += usize::from(chat.branch_mismatch);
        }
        summary
    }

    fn active(&self) -> usize {
        self.working + self.awaiting_input + self.errors
    }

    fn card_label(&self) -> String {
        if self.chats == 0 {
            "No matched chats".into()
        } else {
            let chats = format!(
                "{} chat{}",
                self.chats,
                if self.chats == 1 { "" } else { "s" }
            );
            if self.awaiting_input > 0 {
                format!("{} needs input · {chats}", self.awaiting_input)
            } else if self.errors > 0 {
                format!(
                    "{} error{} · {chats}",
                    self.errors,
                    if self.errors == 1 { "" } else { "s" }
                )
            } else if self.working > 0 {
                format!("{} active · {chats}", self.working)
            } else {
                format!("Idle · {chats}")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct GraphNode {
    id: String,
    kind: GraphNodeKind,
    title: String,
    path: Option<String>,
    full_path: Option<String>,
    branch: Option<String>,
    head_sha: Option<String>,
    detail: Option<String>,
    status: TopologyStatus,
    selection: Option<TopologySelection>,
    occupancy: Option<OccupancySummary>,
    is_main: bool,
    locked: bool,
    prunable: bool,
    ahead: Option<u64>,
    behind: Option<u64>,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

#[derive(Debug, Clone, PartialEq)]
struct GraphEdge {
    parent: String,
    child: String,
    relation: &'static str,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct GraphLayout {
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
    width: f32,
    height: f32,
}

#[derive(Debug, Clone)]
struct NodeSeed {
    id: String,
    parent: Option<String>,
    kind: GraphNodeKind,
    title: String,
    path: Option<String>,
    full_path: Option<String>,
    branch: Option<String>,
    head_sha: Option<String>,
    detail: Option<String>,
    status: TopologyStatus,
    selection: Option<TopologySelection>,
    occupancy: Option<OccupancySummary>,
    is_main: bool,
    locked: bool,
    prunable: bool,
    ahead: Option<u64>,
    behind: Option<u64>,
    relation: &'static str,
}

impl GraphLayout {
    fn from_snapshot(snapshot: &TopologySnapshot) -> Self {
        let root_id = format!("workspace:{}", snapshot.workspace_id);
        let mut seeds = vec![NodeSeed {
            id: root_id.clone(),
            parent: None,
            kind: GraphNodeKind::Workspace,
            title: snapshot.workspace_name.clone(),
            path: Some(snapshot.workspace_path.clone()),
            full_path: Some(snapshot.workspace_path.clone()),
            branch: snapshot.workspace_branch.clone(),
            head_sha: None,
            detail: Some(format!(
                "{} repositor{}",
                snapshot.repositories.len(),
                if snapshot.repositories.len() == 1 {
                    "y"
                } else {
                    "ies"
                }
            )),
            status: snapshot.workspace_status,
            selection: None,
            occupancy: None,
            is_main: false,
            locked: false,
            prunable: false,
            ahead: None,
            behind: None,
            relation: "workspace",
        }];
        let repository_ids: HashSet<&str> = snapshot
            .repositories
            .iter()
            .map(|repo| repo.id.as_str())
            .collect();
        for repository in &snapshot.repositories {
            let repo_id = format!("repo:{}", repository.id);
            let parent = repository
                .parent_id
                .as_deref()
                .filter(|id| repository_ids.contains(id))
                .map(|id| format!("repo:{id}"))
                .unwrap_or_else(|| root_id.clone());
            let relation = match repository.kind {
                RepositoryKind::Submodule => "submodule",
                RepositoryKind::Workspace => "root repo",
                _ => "repository",
            };
            seeds.push(NodeSeed {
                id: repo_id.clone(),
                parent: Some(parent),
                kind: GraphNodeKind::Repository(repository.kind),
                title: repository.name.clone(),
                path: Some(repository.path.clone()),
                full_path: Some(repository.full_path.clone()),
                branch: ref_label(repository.branch.as_deref(), repository.head_sha.as_deref()),
                head_sha: repository.head_sha.clone(),
                detail: Some(format!(
                    "{} worktree{}",
                    repository.worktrees.len(),
                    if repository.worktrees.len() == 1 {
                        ""
                    } else {
                        "s"
                    }
                )),
                status: repository.status,
                selection: Some(TopologySelection::Repository(repository.id.clone())),
                occupancy: None,
                is_main: false,
                locked: false,
                prunable: false,
                ahead: repository.ahead,
                behind: repository.behind,
                relation,
            });
            for worktree in &repository.worktrees {
                let worktree_id = format!("worktree:{}:{}", repository.id, worktree.id);
                let occupancy = OccupancySummary::from_chats(&worktree.chats);
                seeds.push(NodeSeed {
                    id: worktree_id,
                    parent: Some(repo_id.clone()),
                    kind: GraphNodeKind::Worktree,
                    title: worktree_ref_label(worktree),
                    path: Some(worktree.path.clone()),
                    full_path: Some(worktree.full_path.clone()),
                    branch: Some(worktree.name.clone()),
                    head_sha: worktree.head_sha.clone(),
                    detail: Some(occupancy.card_label()),
                    status: worktree.status,
                    selection: Some(TopologySelection::Worktree {
                        repository_id: repository.id.clone(),
                        worktree_id: worktree.id.clone(),
                    }),
                    occupancy: Some(occupancy),
                    is_main: worktree.is_main,
                    locked: worktree.locked,
                    prunable: worktree.prunable,
                    ahead: worktree.ahead,
                    behind: worktree.behind,
                    relation: "worktree",
                });
            }
        }
        Self::layout(seeds)
    }

    fn layout(seeds: Vec<NodeSeed>) -> Self {
        if seeds.is_empty() {
            return Self::default();
        }
        let by_id: HashMap<String, usize> = seeds
            .iter()
            .enumerate()
            .map(|(index, seed)| (seed.id.clone(), index))
            .collect();
        let mut children: HashMap<String, Vec<usize>> = HashMap::new();
        let mut edges = Vec::new();
        for (index, seed) in seeds.iter().enumerate() {
            if let Some(parent) = seed
                .parent
                .as_ref()
                .filter(|parent| by_id.contains_key(*parent))
            {
                children.entry(parent.clone()).or_default().push(index);
                edges.push(GraphEdge {
                    parent: parent.clone(),
                    child: seed.id.clone(),
                    relation: seed.relation,
                });
            }
        }
        for list in children.values_mut() {
            list.sort_by(|a, b| node_sort_key(&seeds[*a]).cmp(&node_sort_key(&seeds[*b])));
        }

        let max_width = seeds
            .iter()
            .map(|seed| node_size(seed.kind).0)
            .fold(0.0, f32::max);
        let column_step = max_width + COLUMN_GAP;
        let mut placements = HashMap::<String, (f32, f32)>::new();
        let mut cursor_y = GRAPH_PAD;
        let mut visiting = HashSet::new();
        Self::place_node(
            0,
            0,
            &seeds,
            &children,
            &mut placements,
            &mut cursor_y,
            &mut visiting,
            column_step,
        );
        // Malformed/missing-parent records stay visible as a second root row.
        for index in 1..seeds.len() {
            if !placements.contains_key(&seeds[index].id) {
                Self::place_node(
                    index,
                    1,
                    &seeds,
                    &children,
                    &mut placements,
                    &mut cursor_y,
                    &mut visiting,
                    column_step,
                );
            }
        }

        let mut nodes = Vec::with_capacity(seeds.len());
        let mut max_x = 0.0_f32;
        let mut max_y = 0.0_f32;
        for seed in seeds {
            let Some((x, center_y)) = placements.get(&seed.id).copied() else {
                continue;
            };
            let (width, height) = node_size(seed.kind);
            let y = center_y - height / 2.0;
            max_x = max_x.max(x + width);
            max_y = max_y.max(y + height);
            nodes.push(GraphNode {
                id: seed.id,
                kind: seed.kind,
                title: seed.title,
                path: seed.path,
                full_path: seed.full_path,
                branch: seed.branch,
                head_sha: seed.head_sha,
                detail: seed.detail,
                status: seed.status,
                selection: seed.selection,
                occupancy: seed.occupancy,
                is_main: seed.is_main,
                locked: seed.locked,
                prunable: seed.prunable,
                ahead: seed.ahead,
                behind: seed.behind,
                x,
                y,
                width,
                height,
            });
        }
        edges.retain(|edge| {
            placements.contains_key(&edge.parent) && placements.contains_key(&edge.child)
        });
        Self {
            nodes,
            edges,
            width: max_x + GRAPH_PAD,
            height: max_y + GRAPH_PAD,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn place_node(
        index: usize,
        depth: usize,
        seeds: &[NodeSeed],
        children: &HashMap<String, Vec<usize>>,
        placements: &mut HashMap<String, (f32, f32)>,
        cursor_y: &mut f32,
        visiting: &mut HashSet<String>,
        column_step: f32,
    ) -> f32 {
        let seed = &seeds[index];
        if let Some((_, center)) = placements.get(&seed.id) {
            return *center;
        }
        if !visiting.insert(seed.id.clone()) {
            let (_, height) = node_size(seed.kind);
            let center = *cursor_y + height / 2.0;
            *cursor_y += height + ROW_GAP;
            placements.insert(
                seed.id.clone(),
                (GRAPH_PAD + depth as f32 * column_step, center),
            );
            return center;
        }
        let child_indices = children.get(&seed.id).cloned().unwrap_or_default();
        let (worktree_children, regular_children): (Vec<_>, Vec<_>) = child_indices
            .into_iter()
            .partition(|child| seeds[*child].kind == GraphNodeKind::Worktree);
        let mut child_centers = regular_children
            .into_iter()
            .map(|child| {
                Self::place_node(
                    child,
                    depth + 1,
                    seeds,
                    children,
                    placements,
                    cursor_y,
                    visiting,
                    column_step,
                )
            })
            .collect::<Vec<_>>();
        if worktree_children.len() > DENSE_WORKTREE_THRESHOLD {
            let columns = worktree_grid_columns(worktree_children.len());
            let rows = worktree_children.len().div_ceil(columns);
            let (worktree_width, worktree_height) = node_size(GraphNodeKind::Worktree);
            let start_y = *cursor_y;
            let first_x = GRAPH_PAD + (depth + 1) as f32 * column_step;
            for (slot, child) in worktree_children.into_iter().enumerate() {
                let column = slot % columns;
                let row = slot / columns;
                let x = first_x + column as f32 * (worktree_width + WORKTREE_GRID_COLUMN_GAP);
                let center = start_y
                    + row as f32 * (worktree_height + WORKTREE_GRID_ROW_GAP)
                    + worktree_height / 2.0;
                placements.insert(seeds[child].id.clone(), (x, center));
                child_centers.push(center);
            }
            *cursor_y += rows as f32 * (worktree_height + WORKTREE_GRID_ROW_GAP);
        } else {
            child_centers.extend(worktree_children.into_iter().map(|child| {
                Self::place_node(
                    child,
                    depth + 1,
                    seeds,
                    children,
                    placements,
                    cursor_y,
                    visiting,
                    column_step,
                )
            }));
        }
        let (_, height) = node_size(seed.kind);
        let center =
            if let (Some(first), Some(last)) = (child_centers.first(), child_centers.last()) {
                (first + last) / 2.0
            } else {
                let center = *cursor_y + height / 2.0;
                *cursor_y += height + ROW_GAP;
                center
            };
        visiting.remove(&seed.id);
        placements.insert(
            seed.id.clone(),
            (GRAPH_PAD + depth as f32 * column_step, center),
        );
        center
    }

    fn node(&self, id: &str) -> Option<&GraphNode> {
        self.nodes.iter().find(|node| node.id == id)
    }
}

fn node_sort_key(seed: &NodeSeed) -> (u8, String) {
    let order = match seed.kind {
        GraphNodeKind::Repository(RepositoryKind::Submodule) => 0,
        GraphNodeKind::Repository(_) => 1,
        GraphNodeKind::Worktree => 2,
        GraphNodeKind::Workspace => 3,
    };
    (order, seed.title.to_lowercase())
}

fn worktree_grid_columns(count: usize) -> usize {
    match count {
        0..=DENSE_WORKTREE_THRESHOLD => 1,
        5..=8 => 2,
        9..=15 => 3,
        _ => 4,
    }
}

fn node_size(kind: GraphNodeKind) -> (f32, f32) {
    match kind {
        GraphNodeKind::Workspace => (220.0, 88.0),
        GraphNodeKind::Repository(_) => (222.0, 96.0),
        GraphNodeKind::Worktree => (304.0, 116.0),
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ScreenNodeRect {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    content_scale: f32,
}

/// Map a logical node into the canvas with the overview canvas's layout-time
/// scaling model. Card bounds, padding, icons, and type all use the same raw
/// zoom, avoiding both rasterized text and mismatched content/card geometry.
fn screen_node_rect(node: &GraphNode, zoom: f32, pan: (f32, f32)) -> ScreenNodeRect {
    let width = node.width * zoom;
    let height = node.height * zoom;
    let center_x = (node.x + node.width / 2.0) * zoom + pan.0;
    let center_y = (node.y + node.height / 2.0) * zoom + pan.1;
    ScreenNodeRect {
        x: center_x - width / 2.0,
        y: center_y - height / 2.0,
        width,
        height,
        content_scale: zoom,
    }
}

fn ref_label(branch: Option<&str>, sha: Option<&str>) -> Option<String> {
    branch
        .filter(|branch| !branch.trim().is_empty())
        .map(str::to_string)
        .or_else(|| sha.map(|sha| sha.chars().take(8).collect()))
}

fn worktree_ref_label(worktree: &TopologyWorktree) -> String {
    worktree
        .branch
        .as_ref()
        .filter(|branch| !branch.trim().is_empty())
        .cloned()
        .or_else(|| {
            worktree
                .head_sha
                .as_deref()
                .map(|sha| format!("detached @ {}", sha.chars().take(8).collect::<String>()))
        })
        .unwrap_or_else(|| "detached HEAD".into())
}

fn worktree_closeout_chat_id(worktree: &TopologyWorktree) -> Option<String> {
    worktree
        .chats
        .iter()
        .find(|chat| !chat.archived)
        .or_else(|| worktree.chats.first())
        .map(|chat| chat.id.clone())
}

fn worktree_can_closeout(worktree: &TopologyWorktree) -> bool {
    !worktree.is_main
}

fn render_closeout_preview(preview: &WorktreeCloseoutPreview, theme: &Theme) -> gpui::AnyElement {
    let (label, detail, color, files) = match preview {
        WorktreeCloseoutPreview::Loading { .. } => (
            "Checking uncommitted changes…".to_string(),
            None,
            theme.text_muted,
            Vec::new(),
        ),
        WorktreeCloseoutPreview::Failed(error) => (
            "Couldn’t inspect uncommitted changes".to_string(),
            Some(error.clone()),
            theme.danger,
            Vec::new(),
        ),
        WorktreeCloseoutPreview::Loaded(plan) if !plan.is_worktree => (
            "Couldn’t inspect uncommitted changes".to_string(),
            Some("This path is not a linked worktree".to_string()),
            theme.danger,
            Vec::new(),
        ),
        WorktreeCloseoutPreview::Loaded(plan) => {
            let mut blockers = Vec::new();
            let mut rows = Vec::new();
            let mut hard_blocker = false;
            if plan.branch.is_some() && plan.branch == plan.default_branch {
                blockers.push(format!(
                    "Checked out on default branch {}",
                    plan.default_branch.as_deref().unwrap_or_default()
                ));
                hard_blocker = true;
            }
            if plan.chat_live {
                blockers.push("Chat session is still live".to_string());
                hard_blocker = true;
            }
            if !plan.shared_chats.is_empty() {
                blockers.push(format!(
                    "{} other active chat{} use this worktree",
                    plan.shared_chats.len(),
                    if plan.shared_chats.len() == 1 {
                        ""
                    } else {
                        "s"
                    }
                ));
                rows.extend(
                    plan.shared_chats
                        .iter()
                        .map(|chat| format!("Chat  {}", chat.title)),
                );
                hard_blocker = true;
            }
            if let Some(label) = crate::chat_closeout::dirty_inspection_label(plan) {
                blockers.push(label);
                hard_blocker = true;
            }
            if plan.dirty {
                blockers.push(crate::chat_closeout::dirty_count_label(plan));
                rows.extend(crate::chat_closeout::dirty_file_lines(plan));
            }
            if let Some(label) = crate::chat_closeout::unmerged_label(plan) {
                blockers.push(label);
                rows.extend(plan.unmerged_commit_details.iter().map(|commit| {
                    let short = commit.sha.chars().take(8).collect::<String>();
                    format!("{short}  {}", commit.subject)
                }));
                let omitted = plan.unmerged_commits as usize
                    - plan
                        .unmerged_commit_details
                        .len()
                        .min(plan.unmerged_commits as usize);
                if omitted > 0 {
                    rows.push(format!("+{omitted} older commits not shown"));
                }
            }
            if let Some(label) = crate::chat_closeout::unverifiable_label(plan) {
                blockers.push(label);
            }
            if blockers.is_empty() {
                (
                    "No close-out blockers".to_string(),
                    None,
                    theme.success,
                    Vec::new(),
                )
            } else {
                (
                    "Close-out blockers".to_string(),
                    Some(blockers.join(" · ")),
                    if hard_blocker {
                        theme.danger
                    } else {
                        theme.warning
                    },
                    rows,
                )
            }
        }
    };

    div()
        .w_full()
        .p(px(9.0))
        .rounded(px(6.0))
        .border_1()
        .border_color(color.opacity(0.35))
        .bg(color.opacity(0.07))
        .text_size(crate::typography::ui_rems(9.5))
        .text_color(color)
        .child(SharedString::from(label))
        .when_some(detail, |element, detail| {
            element.child(
                div()
                    .mt(px(4.0))
                    .text_size(crate::typography::ui_rems(8.5))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(detail)),
            )
        })
        .children(files.into_iter().enumerate().map(|(index, file)| {
            let tooltip_path = file.clone();
            div()
                .id(("topology-closeout-dirty-file", index))
                .mt(px(3.0))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_size(crate::typography::ui_rems(8.5))
                .text_color(theme.text_muted)
                .child(SharedString::from(file))
                .tooltip(move |_, cx| {
                    cx.new(|_| TopologyTooltip(SharedString::from(tooltip_path.clone())))
                        .into()
                })
        }))
        .into_any_element()
}

fn sync_detail(ahead: Option<u64>, behind: Option<u64>) -> Option<String> {
    if ahead.is_none() && behind.is_none() {
        return Some("upstream unknown".into());
    }
    let ahead = ahead.unwrap_or(0);
    let behind = behind.unwrap_or(0);
    match (ahead, behind) {
        (0, 0) => Some("up to date".into()),
        (ahead, 0) => Some(format!("↑{ahead}")),
        (0, behind) => Some(format!("↓{behind}")),
        (ahead, behind) => Some(format!("↑{ahead} ↓{behind}")),
    }
}

#[derive(Debug, Clone)]
enum LoadState {
    Idle,
    Loading,
    Loaded(TopologySnapshot),
    Refreshing(TopologySnapshot),
    Failed(String),
}

impl LoadState {
    /// Starts a request while retaining an already rendered snapshot. Returns
    /// whether this is a background refresh rather than an initial load.
    fn begin_request(&mut self) -> bool {
        let previous = std::mem::replace(self, Self::Idle);
        match previous {
            Self::Loaded(snapshot) | Self::Refreshing(snapshot) => {
                *self = Self::Refreshing(snapshot);
                true
            }
            _ => {
                *self = Self::Loading;
                false
            }
        }
    }

    /// Applies a completed request. A failed background refresh keeps the last
    /// good snapshot on screen instead of replacing the map with an error.
    fn finish_request(&mut self, result: Result<TopologySnapshot, String>) -> bool {
        let previous = std::mem::replace(self, Self::Idle);
        match result {
            Ok(snapshot) => {
                *self = Self::Loaded(snapshot);
                true
            }
            Err(error) => {
                *self = match previous {
                    Self::Refreshing(snapshot) => Self::Loaded(snapshot),
                    _ => Self::Failed(error),
                };
                false
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct ChatDiffSummary {
    files: Vec<zeron_proto::DiffFileSummary>,
    additions: u32,
    deletions: u32,
    truncated: bool,
}

impl From<zeron_proto::CheckoutDiff> for ChatDiffSummary {
    fn from(diff: zeron_proto::CheckoutDiff) -> Self {
        Self {
            files: diff.files,
            additions: diff.additions,
            deletions: diff.deletions,
            truncated: diff.truncated,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum ChatDiffPeek {
    Loading { request_id: u64 },
    Loaded(ChatDiffSummary),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct WorktreeCloseoutPreviewKey {
    device_id: String,
    cwd: String,
    chat_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WorktreeCloseoutPreview {
    Loading { request_id: u64 },
    Loaded(crate::chat_closeout::CloseoutPlan),
    Failed(String),
}

fn closeout_preview_key(
    tab: Option<&WorkspaceTab>,
    local_device_id: Option<&str>,
    worktree: &TopologyWorktree,
) -> Option<WorktreeCloseoutPreviewKey> {
    let tab = tab?;
    if !worktree_can_closeout(worktree) || local_device_id != Some(tab.device_id.as_str()) {
        return None;
    }
    Some(WorktreeCloseoutPreviewKey {
        device_id: tab.device_id.clone(),
        cwd: worktree.full_path.clone(),
        chat_id: worktree_closeout_chat_id(worktree),
    })
}

fn begin_closeout_preview_request(
    previews: &mut HashMap<WorktreeCloseoutPreviewKey, WorktreeCloseoutPreview>,
    next_request_id: &mut u64,
    key: &WorktreeCloseoutPreviewKey,
    force: bool,
) -> Option<u64> {
    if matches!(
        previews.get(key),
        Some(WorktreeCloseoutPreview::Loading { .. })
    ) || (!force && previews.contains_key(key))
    {
        return None;
    }
    *next_request_id = (*next_request_id).wrapping_add(1);
    let request_id = *next_request_id;
    previews.insert(key.clone(), WorktreeCloseoutPreview::Loading { request_id });
    Some(request_id)
}

fn finish_closeout_preview_request(
    previews: &mut HashMap<WorktreeCloseoutPreviewKey, WorktreeCloseoutPreview>,
    key: WorktreeCloseoutPreviewKey,
    request_id: u64,
    result: Result<crate::chat_closeout::CloseoutPlan, String>,
) -> bool {
    if !matches!(
        previews.get(&key),
        Some(WorktreeCloseoutPreview::Loading {
            request_id: active_request_id,
        }) if *active_request_id == request_id
    ) {
        return false;
    }
    previews.insert(
        key,
        match result {
            Ok(plan) => WorktreeCloseoutPreview::Loaded(plan),
            Err(error) => WorktreeCloseoutPreview::Failed(error),
        },
    );
    true
}

fn begin_chat_diff_request(
    peeks: &mut HashMap<String, ChatDiffPeek>,
    next_request_id: &mut u64,
    chat_id: &str,
) -> Option<u64> {
    if matches!(peeks.get(chat_id), Some(ChatDiffPeek::Loading { .. })) {
        return None;
    }
    *next_request_id = (*next_request_id).wrapping_add(1);
    let request_id = *next_request_id;
    peeks.insert(chat_id.to_string(), ChatDiffPeek::Loading { request_id });
    Some(request_id)
}

fn finish_chat_diff_request(
    peeks: &mut HashMap<String, ChatDiffPeek>,
    chat_id: String,
    request_id: u64,
    result: Result<ChatDiffSummary, String>,
) -> bool {
    if !matches!(
        peeks.get(&chat_id),
        Some(ChatDiffPeek::Loading {
            request_id: active_request_id,
        }) if *active_request_id == request_id
    ) {
        return false;
    }
    peeks.insert(
        chat_id,
        match result {
            Ok(diff) => ChatDiffPeek::Loaded(diff),
            Err(error) => ChatDiffPeek::Failed(error),
        },
    );
    true
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkspaceTab {
    id: String,
    device_id: String,
    display_name: String,
    path: String,
}

impl WorkspaceTab {
    fn from_space(space: &zeron_proto::Space) -> Self {
        Self {
            id: space.id.clone(),
            device_id: space.device_id.clone(),
            display_name: space.display_name().to_string(),
            path: space.path.clone(),
        }
    }

    fn request_payload(&self) -> serde_json::Value {
        serde_json::json!({
            "spaceId": self.id,
            "targetDeviceId": self.device_id,
        })
    }
}

/// Route-local workspace choice. The app-wide project selection is consulted
/// exactly once when this model is seeded; subsequent chat/project navigation
/// must not re-aim an already-open repository map.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct WorkspaceTabSelection {
    selected_id: Option<String>,
    initial_app_selected_id: Option<String>,
    seeded: bool,
}

impl WorkspaceTabSelection {
    fn new(tabs: &[WorkspaceTab], app_selected_id: Option<&str>, catalog_ready: bool) -> Self {
        let mut selection = Self {
            selected_id: None,
            initial_app_selected_id: app_selected_id.map(str::to_owned),
            seeded: false,
        };
        selection.reconcile(tabs, catalog_ready);
        selection
    }

    /// Preserve a valid local choice. If its workspace was deleted, fall back
    /// to the first display-sorted tab rather than the process cwd or whatever
    /// project happens to be selected by the current chat.
    fn reconcile(&mut self, tabs: &[WorkspaceTab], catalog_ready: bool) -> bool {
        if self
            .selected_id
            .as_deref()
            .is_some_and(|id| tabs.iter().any(|tab| tab.id == id))
        {
            return false;
        }
        let previous = self.selected_id.clone();
        self.selected_id = if self.seeded {
            tabs.first().map(|tab| tab.id.clone())
        } else {
            let initial = self
                .initial_app_selected_id
                .as_deref()
                .and_then(|id| tabs.iter().find(|tab| tab.id == id));
            if initial.is_none() && !catalog_ready {
                return false;
            }
            initial.or_else(|| tabs.first()).map(|tab| tab.id.clone())
        };
        self.seeded = true;
        self.selected_id != previous
    }

    fn select(&mut self, tabs: &[WorkspaceTab], id: &str) -> bool {
        if self.selected_id.as_deref() == Some(id) || !tabs.iter().any(|tab| tab.id == id) {
            return false;
        }
        self.selected_id = Some(id.to_string());
        self.seeded = true;
        true
    }

    fn selected<'a>(&self, tabs: &'a [WorkspaceTab]) -> Option<&'a WorkspaceTab> {
        let id = self.selected_id.as_deref()?;
        tabs.iter().find(|tab| tab.id == id)
    }
}

struct TopologyTooltip(SharedString);

impl Render for TopologyTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div()
            .max_w(px(380.0))
            .px(px(9.0))
            .py(px(7.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border_strong)
            .bg(theme.surface_raised)
            .shadow_md()
            .text_size(crate::typography::ui_rems(10.5))
            .text_color(theme.text)
            .child(self.0.clone())
    }
}

fn toggle_disclosure(expanded: &mut HashSet<String>, id: &str) -> bool {
    if expanded.remove(id) {
        false
    } else {
        expanded.insert(id.to_string());
        true
    }
}

pub struct RepositoryTopology {
    state: Entity<AppState>,
    shell: gpui::WeakEntity<Shell>,
    search: Entity<ComposerInput>,
    load: LoadState,
    workspace_selection: WorkspaceTabSelection,
    requested_space_id: Option<String>,
    request_seq: u64,
    pan: (f32, f32),
    zoom: f32,
    panning: Option<((f32, f32), (f32, f32))>,
    canvas_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    auto_fit: bool,
    selected: Option<TopologySelection>,
    expanded_chats: HashSet<String>,
    chat_diff_peeks: HashMap<String, ChatDiffPeek>,
    chat_diff_request_seq: u64,
    closeout_previews: HashMap<WorktreeCloseoutPreviewKey, WorktreeCloseoutPreview>,
    closeout_preview_request_seq: u64,
    _state_subscription: gpui::Subscription,
    _search_events: gpui::Subscription,
    _refresh_task: gpui::Task<()>,
}

impl RepositoryTopology {
    pub fn new(
        state: Entity<AppState>,
        shell: gpui::WeakEntity<Shell>,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| {
            ComposerInput::with_context(
                "Search repositories, worktrees, branches, or chats…",
                crate::composer::PALETTE_SEARCH_CONTEXT,
                cx,
            )
            .with_single_line()
            .with_accessibility_role(gpui::Role::SearchInput)
            .with_text_metrics(11.0, 16.0)
        });
        let workspace_selection = {
            let state = state.read(cx);
            let tabs = Self::workspace_tabs(&state);
            WorkspaceTabSelection::new(
                &tabs,
                state.selected_space_row().map(|space| space.id.as_str()),
                state.spaces_synced,
            )
        };
        let subscription = cx.observe(&state, |_, _, cx| cx.notify());
        let search_events = cx.subscribe(&search, |this: &mut Self, input, event, cx| {
            if matches!(event, ComposerInputEvent::Edited) {
                this.auto_fit = true;
                let query = input.read(cx).text().to_string();
                this.focus_search_match(&query, cx);
                cx.notify();
            }
        });
        let refresh_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(LIVE_REFRESH).await;
                if this.update(cx, |this, cx| this.refresh_tick(cx)).is_err() {
                    break;
                }
            }
        });
        Self {
            state,
            shell,
            search,
            load: LoadState::Idle,
            workspace_selection,
            requested_space_id: None,
            request_seq: 0,
            pan: (20.0, 20.0),
            zoom: 0.9,
            panning: None,
            canvas_bounds: Rc::new(Cell::new(None)),
            auto_fit: true,
            selected: None,
            expanded_chats: HashSet::new(),
            chat_diff_peeks: HashMap::new(),
            chat_diff_request_seq: 0,
            closeout_previews: HashMap::new(),
            closeout_preview_request_seq: 0,
            _state_subscription: subscription,
            _search_events: search_events,
            _refresh_task: refresh_task,
        }
    }

    fn refresh_tick(&mut self, cx: &mut Context<Self>) {
        let visible = self
            .shell
            .upgrade()
            .is_some_and(|shell| shell.read(cx).is_repository_topology_route());
        if visible && !matches!(&self.load, LoadState::Loading | LoadState::Refreshing(_)) {
            let tabs = self.reconcile_workspace_tabs(cx);
            self.requested_space_id = None;
            self.ensure_loaded(&tabs, cx);
        }
    }

    fn focus_search_match(&mut self, query: &str, cx: &mut Context<Self>) {
        let snapshot = match &self.load {
            LoadState::Loaded(snapshot) | LoadState::Refreshing(snapshot) => snapshot,
            _ => return,
        };
        let result = topology_search(snapshot, query);
        if let Some(focus) = result.focus {
            self.expanded_chats.extend(result.matching_chat_ids);
            self.select_topology_node(focus, cx);
        }
    }

    fn select_topology_node(&mut self, selection: TopologySelection, cx: &mut Context<Self>) {
        self.selected = Some(selection);
        self.ensure_selected_closeout_preview(true, cx);
        cx.notify();
    }

    fn ensure_selected_closeout_preview(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(TopologySelection::Worktree {
            repository_id,
            worktree_id,
        }) = self.selected.as_ref()
        else {
            return;
        };
        let snapshot = match &self.load {
            LoadState::Loaded(snapshot) | LoadState::Refreshing(snapshot) => snapshot,
            _ => return,
        };
        let Some(worktree) = snapshot
            .repositories
            .iter()
            .find(|repository| &repository.id == repository_id)
            .and_then(|repository| {
                repository
                    .worktrees
                    .iter()
                    .find(|worktree| &worktree.id == worktree_id)
            })
            .cloned()
        else {
            return;
        };
        let (key, engine) = {
            let state = self.state.read(cx);
            let tabs = Self::workspace_tabs(&state);
            (
                closeout_preview_key(
                    self.workspace_selection.selected(&tabs),
                    state.local_device_id.as_deref(),
                    &worktree,
                ),
                state.engine().cloned(),
            )
        };
        let Some(key) = key else {
            return;
        };
        let Some(request_id) = begin_closeout_preview_request(
            &mut self.closeout_previews,
            &mut self.closeout_preview_request_seq,
            &key,
            force,
        ) else {
            return;
        };
        let Some(engine) = engine else {
            self.closeout_previews.insert(
                key,
                WorktreeCloseoutPreview::Failed("Engine unavailable".into()),
            );
            cx.notify();
            return;
        };
        cx.notify();
        cx.spawn(async move |this, cx| {
            let payload = serde_json::json!({
                "chatId": key.chat_id.clone(),
                "cwd": key.cwd.clone(),
            });
            let result = crate::attachments::call_with_timeout(
                &engine,
                cx.background_executor(),
                crate::chat_closeout::PLAN_CHAT_CLOSEOUT,
                payload,
                crate::chat_closeout::PLAN_TIMEOUT,
            )
            .await
            .and_then(|value| crate::chat_closeout::decode_plan(&value));
            this.update(cx, |this, cx| {
                if finish_closeout_preview_request(
                    &mut this.closeout_previews,
                    key,
                    request_id,
                    result,
                ) {
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn workspace_tabs(state: &AppState) -> Vec<WorkspaceTab> {
        state
            .spaces_sorted()
            .into_iter()
            .map(WorkspaceTab::from_space)
            .collect()
    }

    fn reconcile_workspace_tabs(&mut self, cx: &mut Context<Self>) -> Vec<WorkspaceTab> {
        let (tabs, catalog_ready) = {
            let state = self.state.read(cx);
            (Self::workspace_tabs(&state), state.spaces_synced)
        };
        if self.workspace_selection.reconcile(&tabs, catalog_ready) {
            self.invalidate_workspace_load();
        }
        tabs
    }

    fn invalidate_workspace_load(&mut self) {
        self.request_seq += 1;
        self.requested_space_id = None;
        self.load = LoadState::Idle;
        self.selected = None;
        self.expanded_chats.clear();
        self.chat_diff_peeks.clear();
        self.closeout_previews.clear();
        self.pan = (20.0, 20.0);
        self.zoom = 0.9;
        self.panning = None;
        self.auto_fit = true;
    }

    fn select_workspace(&mut self, space_id: &str, cx: &mut Context<Self>) {
        let tabs = {
            let state = self.state.read(cx);
            Self::workspace_tabs(&state)
        };
        if !self.workspace_selection.select(&tabs, space_id) {
            return;
        }
        self.invalidate_workspace_load();
        self.ensure_loaded(&tabs, cx);
        cx.notify();
    }

    fn ensure_loaded(&mut self, tabs: &[WorkspaceTab], cx: &mut Context<Self>) {
        let Some(tab) = self.workspace_selection.selected(tabs) else {
            self.requested_space_id = None;
            self.load = LoadState::Idle;
            return;
        };
        let space_id = tab.id.clone();
        let request_payload = tab.request_payload();
        if self.requested_space_id.as_deref() == Some(space_id.as_str()) {
            return;
        }
        if self.requested_space_id.is_some() {
            self.selected = None;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.requested_space_id = Some(space_id.clone());
        self.request_seq += 1;
        let request_id = self.request_seq;
        let is_refresh = self.load.begin_request();
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(zeron_rpc::methods::GET_REPOSITORY_TOPOLOGY, request_payload)
                .await
                .map_err(|error| error.to_string())
                .and_then(TopologyAdapter::decode);
            this.update(cx, |this, cx| {
                if this.request_seq != request_id {
                    return;
                }
                let loaded = this.load.finish_request(result);
                if loaded && !is_refresh {
                    this.auto_fit = true;
                }
                if loaded {
                    // The topology snapshot refreshes while Repo Map is open;
                    // refresh the selected worktree's detailed blocker plan at
                    // the same time so file/commit rows cannot contradict the
                    // newly refreshed card status.
                    this.ensure_selected_closeout_preview(is_refresh, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.requested_space_id = None;
        let tabs = self.reconcile_workspace_tabs(cx);
        self.ensure_loaded(&tabs, cx);
        cx.notify();
    }

    fn load_chat_diff(
        &mut self,
        chat_id: String,
        cwd: String,
        target_device_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if matches!(
            self.chat_diff_peeks.get(&chat_id),
            Some(ChatDiffPeek::Loading { .. })
        ) {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.chat_diff_peeks
                .insert(chat_id, ChatDiffPeek::Failed("Engine unavailable".into()));
            cx.notify();
            return;
        };
        let Some(request_id) = begin_chat_diff_request(
            &mut self.chat_diff_peeks,
            &mut self.chat_diff_request_seq,
            &chat_id,
        ) else {
            return;
        };
        cx.notify();
        cx.spawn(async move |this, cx| {
            let mut payload = serde_json::json!({
                "cwd": cwd,
                "mode": "turn",
                "chatId": chat_id,
            });
            if let Some(target_device_id) = target_device_id {
                payload["targetDeviceId"] = serde_json::Value::String(target_device_id);
            }
            let result = engine
                .client()
                .call(zeron_rpc::methods::GET_CHECKOUT_DIFF, payload)
                .await
                .map_err(|error| error.to_string())
                .and_then(|value| {
                    serde_json::from_value::<zeron_proto::CheckoutDiff>(value)
                        .map_err(|error| error.to_string())
                })
                .map(ChatDiffSummary::from);
            this.update(cx, |this, cx| {
                if finish_chat_diff_request(&mut this.chat_diff_peeks, chat_id, request_id, result)
                {
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn fit_layout(&mut self, layout: &GraphLayout) {
        let Some(bounds) = self.canvas_bounds.get() else {
            return;
        };
        let viewport_w = f32::from(bounds.size.width).max(1.0);
        let viewport_h = f32::from(bounds.size.height).max(1.0);
        let scale_x = (viewport_w - 56.0) / layout.width.max(1.0);
        let scale_y = (viewport_h - 56.0) / layout.height.max(1.0);
        self.zoom = scale_x.min(scale_y).clamp(MIN_ZOOM, 1.0);
        self.pan = (
            (viewport_w - layout.width * self.zoom) / 2.0,
            (viewport_h - layout.height * self.zoom) / 2.0,
        );
    }

    fn zoom_toward(&mut self, proposed: f32, cursor_window: (f32, f32)) {
        let zoom = proposed.clamp(MIN_ZOOM, MAX_ZOOM);
        if (zoom - self.zoom).abs() < f32::EPSILON {
            return;
        }
        let origin = self
            .canvas_bounds
            .get()
            .map(|bounds| (f32::from(bounds.origin.x), f32::from(bounds.origin.y)))
            .unwrap_or((0.0, 0.0));
        let cursor = (cursor_window.0 - origin.0, cursor_window.1 - origin.1);
        let logical = (
            (cursor.0 - self.pan.0) / self.zoom,
            (cursor.1 - self.pan.1) / self.zoom,
        );
        self.zoom = zoom;
        self.pan = (cursor.0 - logical.0 * zoom, cursor.1 - logical.1 * zoom);
    }

    fn render_center_state(
        &self,
        theme: &Theme,
        title: &str,
        detail: &str,
        action: Option<&str>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let action = action.map(|label| {
            div()
                .id("topology-state-action")
                .mt(px(12.0))
                .px(px(12.0))
                .h(px(30.0))
                .flex()
                .items_center()
                .rounded(px(6.0))
                .bg(theme.solid)
                .text_color(theme.on_solid)
                .cursor_pointer()
                .child(SharedString::from(label.to_string()))
                .on_click(cx.listener(|this, _, _, cx| this.reload(cx)))
        });
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .max_w(px(420.0))
                    .flex()
                    .flex_col()
                    .items_center()
                    .text_center()
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(14.0))
                            .text_color(theme.text)
                            .child(SharedString::from(title.to_string())),
                    )
                    .child(
                        div()
                            .mt(px(5.0))
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(detail.to_string())),
                    )
                    .children(action),
            )
            .into_any_element()
    }

    fn render_node(
        &self,
        node: &GraphNode,
        theme: &Theme,
        index: usize,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let rect = screen_node_rect(node, self.zoom, self.pan);
        let selection = node.selection.clone();
        let selected = selection
            .as_ref()
            .is_some_and(|selection| self.selected.as_ref() == Some(selection));
        let kind = match node.kind {
            GraphNodeKind::Workspace => "workspace",
            GraphNodeKind::Repository(RepositoryKind::Workspace) => "root repository",
            GraphNodeKind::Repository(RepositoryKind::Submodule) => "submodule",
            GraphNodeKind::Repository(_) => "repository",
            GraphNodeKind::Worktree => "worktree",
        };
        let icon = match node.kind {
            GraphNodeKind::Workspace => crate::icons::FOLDER_WITH_FILES,
            GraphNodeKind::Repository(_) => crate::icons::GIT_BRANCH,
            GraphNodeKind::Worktree => crate::icons::FILE_TREE,
        };
        let scale = rect.content_scale;
        let tooltip = if node.kind == GraphNodeKind::Worktree {
            let occupancy = node.occupancy.clone().unwrap_or_default();
            Some(SharedString::from(format!(
                "{}\n{}{}\nGit: {} · {}\nOccupancy: {} chats, {} agents, {} active, {} branch mismatches{}{}",
                node.full_path.as_deref().unwrap_or("Path unavailable"),
                node.title,
                node.head_sha
                    .as_ref()
                    .map(|sha| format!(" · {sha}"))
                    .unwrap_or_default(),
                node.status.label(),
                sync_detail(node.ahead, node.behind).unwrap_or_else(|| "upstream unknown".into()),
                occupancy.chats,
                occupancy.agents,
                occupancy.active(),
                occupancy.branch_mismatches,
                if node.locked { " · locked" } else { "" },
                if node.prunable { " · prunable" } else { "" },
            )))
        } else {
            node.full_path.clone().map(SharedString::from)
        };
        let base = div()
            .id(("topology-node", index))
            .absolute()
            .left(px(rect.x))
            .top(px(rect.y))
            .w(px(rect.width))
            .h(px(rect.height))
            .p(px(if node.kind == GraphNodeKind::Worktree {
                9.0
            } else {
                10.0
            } * scale))
            .rounded(px(10.0 * scale))
            .border_1()
            .border_color(if selected {
                theme.busy
            } else if node.kind == GraphNodeKind::Workspace {
                theme.border_strong
            } else {
                theme.border
            })
            .bg(match node.kind {
                GraphNodeKind::Workspace => theme.surface_raised,
                _ => theme.surface_card,
            })
            .shadow_sm()
            .when(selection.is_some(), |element| {
                element.cursor_pointer().hover(|element| {
                    element
                        .border_color(theme.border_strong)
                        .bg(theme.element_hover)
                })
            })
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .when_some(selection, |element, selection| {
                element.on_click(cx.listener(move |this, _, _, cx| {
                    this.select_topology_node(selection.clone(), cx);
                }))
            })
            .when_some(tooltip, |element, tooltip| {
                element.tooltip(move |_, cx| cx.new(|_| TopologyTooltip(tooltip.clone())).into())
            });

        if node.kind == GraphNodeKind::Worktree {
            let occupancy = node.occupancy.clone().unwrap_or_default();
            let occupancy_color = if occupancy.awaiting_input > 0 || occupancy.errors > 0 {
                theme.danger
            } else if occupancy.working > 0 {
                theme.busy
            } else {
                theme.text_faint
            };
            let mut badges = div().flex().items_center().gap(px(5.0 * scale));
            if node.is_main {
                badges = badges.child(self.node_chip("MAIN", theme.busy, theme, scale));
            }
            if node.locked {
                badges = badges.child(self.node_chip("LOCKED", theme.warning, theme, scale));
            }
            if node.prunable {
                badges = badges.child(self.node_chip("PRUNABLE", theme.warning, theme, scale));
            }
            base.flex()
                .flex_col()
                .gap(px(4.0 * scale))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(px(6.0 * scale))
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_size(crate::typography::ui_rems(13.5 * scale))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(theme.text)
                                .child(SharedString::from(node.title.clone())),
                        )
                        .child(badges),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.0 * scale))
                        .text_color(theme.text_muted)
                        .child(crate::icons::icon(crate::icons::FILE_TREE).size(px(10.0 * scale)))
                        .child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_size(crate::typography::ui_rems(10.0 * scale))
                                .child(SharedString::from(node.branch.clone().unwrap_or_default())),
                        ),
                )
                .when_some(node.path.clone(), |element, path| {
                    element.child(
                        div()
                            .w_full()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(crate::typography::ui_rems(9.0 * scale))
                            .text_color(theme.text_faint)
                            .child(SharedString::from(path)),
                    )
                })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(5.0 * scale))
                        .child(self.status_chip(node.status, theme, scale))
                        .child(
                            self.muted_chip(
                                sync_detail(node.ahead, node.behind)
                                    .unwrap_or_else(|| "upstream unknown".into()),
                                theme,
                                scale,
                            ),
                        ),
                )
                .child(
                    div()
                        .mt_auto()
                        .h(px(24.0 * scale))
                        .px(px(7.0 * scale))
                        .flex()
                        .items_center()
                        .gap(px(5.0 * scale))
                        .rounded(px(6.0 * scale))
                        .bg(theme.element_hover)
                        .child(
                            div()
                                .size(px(6.0 * scale))
                                .rounded_full()
                                .bg(occupancy_color),
                        )
                        .child(
                            crate::icons::icon(crate::icons::CHAT_ROUND_LINE)
                                .size(px(10.0 * scale)),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_size(crate::typography::ui_rems(9.5 * scale))
                                .text_color(theme.text_muted)
                                .child(SharedString::from(occupancy.card_label())),
                        ),
                )
                .into_any_element()
        } else {
            let summary = node.detail.clone().unwrap_or_default();
            base.flex()
                .flex_col()
                .gap(px(4.0 * scale))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(5.0 * scale))
                                .text_size(crate::typography::ui_rems(9.5 * scale))
                                .text_color(theme.text_muted)
                                .child(crate::icons::icon(icon).size(px(12.0 * scale)))
                                .child(SharedString::from(kind)),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .text_size(crate::typography::ui_rems(9.5 * scale))
                                .text_color(theme.text_muted)
                                .child(SharedString::from(summary)),
                        ),
                )
                .child(
                    div()
                        .w_full()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_size(crate::typography::ui_rems(12.0 * scale))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(SharedString::from(node.title.clone())),
                )
                .when_some(node.path.clone(), |element, path| {
                    element.child(
                        div()
                            .w_full()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(crate::typography::ui_rems(9.5 * scale))
                            .text_color(theme.text_faint)
                            .child(SharedString::from(path)),
                    )
                })
                .into_any_element()
        }
    }

    fn node_chip(
        &self,
        label: &'static str,
        color: gpui::Hsla,
        theme: &Theme,
        scale: f32,
    ) -> gpui::AnyElement {
        div()
            .px(px(5.0 * scale))
            .py(px(2.0 * scale))
            .rounded(px(4.0 * scale))
            .border_1()
            .border_color(color)
            .text_size(crate::typography::ui_rems(8.0 * scale))
            .text_color(theme.text)
            .child(label)
            .into_any_element()
    }

    fn status_chip(&self, status: TopologyStatus, theme: &Theme, scale: f32) -> gpui::AnyElement {
        div()
            .px(px(7.0 * scale))
            .py(px(3.0 * scale))
            .flex()
            .items_center()
            .gap(px(4.0 * scale))
            .rounded(px(5.0 * scale))
            .bg(theme.element_hover)
            .text_size(crate::typography::ui_rems(9.0 * scale))
            .text_color(theme.text_muted)
            .child(
                div()
                    .size(px(6.0 * scale))
                    .rounded_full()
                    .bg(status.color(theme)),
            )
            .child(SharedString::from(status.label()))
            .into_any_element()
    }

    fn muted_chip(&self, label: String, theme: &Theme, scale: f32) -> gpui::AnyElement {
        div()
            .px(px(7.0 * scale))
            .py(px(3.0 * scale))
            .rounded(px(5.0 * scale))
            .bg(theme.element_hover)
            .text_size(crate::typography::ui_rems(9.0 * scale))
            .text_color(theme.text_muted)
            .child(SharedString::from(label))
            .into_any_element()
    }

    fn detail_row(
        &self,
        label: &'static str,
        value: impl Into<SharedString>,
        theme: &Theme,
    ) -> gpui::AnyElement {
        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(
                div()
                    .text_size(crate::typography::ui_rems(9.0))
                    .text_color(theme.text_faint)
                    .child(label),
            )
            .child(
                div()
                    .text_size(crate::typography::ui_rems(10.5))
                    .text_color(theme.text)
                    .child(value.into()),
            )
            .into_any_element()
    }

    fn render_chat_diff_peek(&self, peek: &ChatDiffPeek, theme: &Theme) -> gpui::AnyElement {
        match peek {
            ChatDiffPeek::Loading { .. } => div()
                .mt(px(8.0))
                .p(px(8.0))
                .rounded(px(6.0))
                .bg(theme.element_hover)
                .text_size(crate::typography::ui_rems(9.0))
                .text_color(theme.text_muted)
                .child("Loading this turn’s changes…")
                .into_any_element(),
            ChatDiffPeek::Failed(error) => div()
                .mt(px(8.0))
                .p(px(8.0))
                .rounded(px(6.0))
                .bg(theme.danger.opacity(0.08))
                .text_size(crate::typography::ui_rems(9.0))
                .text_color(theme.danger)
                .child(SharedString::from(format!("Changes unavailable: {error}")))
                .into_any_element(),
            ChatDiffPeek::Loaded(diff) => {
                let visible_files = diff.files.iter().take(5).map(|file| {
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .text_size(crate::typography::ui_rems(8.5))
                        .child(
                            div()
                                .w(px(12.0))
                                .flex_none()
                                .text_color(theme.text_faint)
                                .child(SharedString::from(file.status.clone())),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_color(theme.text_muted)
                                .child(SharedString::from(file.path.clone())),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_color(theme.success)
                                .child(SharedString::from(format!("+{}", file.additions))),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_color(theme.danger)
                                .child(SharedString::from(format!("−{}", file.deletions))),
                        )
                });
                div()
                    .mt(px(8.0))
                    .p(px(8.0))
                    .flex()
                    .flex_col()
                    .gap(px(5.0))
                    .rounded(px(6.0))
                    .bg(theme.element_hover)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .text_size(crate::typography::ui_rems(9.0))
                            .child(
                                div()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(SharedString::from(format!(
                                        "{} changed file{}",
                                        diff.files.len(),
                                        if diff.files.len() == 1 { "" } else { "s" }
                                    ))),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap(px(5.0))
                                    .child(
                                        div().text_color(theme.success).child(SharedString::from(
                                            format!("+{}", diff.additions),
                                        )),
                                    )
                                    .child(
                                        div().text_color(theme.danger).child(SharedString::from(
                                            format!("−{}", diff.deletions),
                                        )),
                                    ),
                            ),
                    )
                    .when(diff.files.is_empty(), |element| {
                        element.child(
                            div()
                                .text_size(crate::typography::ui_rems(8.5))
                                .text_color(theme.text_faint)
                                .child("No file changes in the latest turn"),
                        )
                    })
                    .children(visible_files)
                    .when(diff.files.len() > 5, |element| {
                        element.child(
                            div()
                                .text_size(crate::typography::ui_rems(8.5))
                                .text_color(theme.text_faint)
                                .child(SharedString::from(format!(
                                    "+{} more",
                                    diff.files.len() - 5
                                ))),
                        )
                    })
                    .when(diff.truncated, |element| {
                        element.child(
                            div()
                                .text_size(crate::typography::ui_rems(8.0))
                                .text_color(theme.warning)
                                .child("Partial snapshot"),
                        )
                    })
                    .into_any_element()
            }
        }
    }

    fn inspector_header(
        &self,
        eyebrow: &'static str,
        title: String,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        div()
            .flex()
            .items_start()
            .justify_between()
            .gap(px(10.0))
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(9.0))
                            .text_color(theme.text_faint)
                            .child(eyebrow),
                    )
                    .child(
                        div()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(crate::typography::ui_rems(14.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child(SharedString::from(title)),
                    ),
            )
            .child(
                div()
                    .id("topology-inspector-close")
                    .size(px(24.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .text_color(theme.text_muted)
                    .hover(|element| element.bg(theme.element_hover).text_color(theme.text))
                    .child("×")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.selected = None;
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    fn render_inspector(
        &self,
        snapshot: &TopologySnapshot,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let selected = self.selected.as_ref()?;
        let content = match selected {
            TopologySelection::Repository(repository_id) => {
                let repository = snapshot
                    .repositories
                    .iter()
                    .find(|repository| &repository.id == repository_id)?;
                let repo_kind = match repository.kind {
                    RepositoryKind::Workspace => "root repository",
                    RepositoryKind::Submodule => "submodule",
                    _ => "repository",
                };
                let worktrees = repository
                    .worktrees
                    .iter()
                    .map(|worktree| {
                        let occupancy = OccupancySummary::from_chats(&worktree.chats);
                        div()
                            .px(px(9.0))
                            .py(px(7.0))
                            .rounded(px(6.0))
                            .bg(theme.element_hover)
                            .child(
                                div()
                                    .text_size(crate::typography::ui_rems(10.5))
                                    .text_color(theme.text)
                                    .child(SharedString::from(worktree_ref_label(worktree))),
                            )
                            .child(
                                div()
                                    .mt(px(2.0))
                                    .text_size(crate::typography::ui_rems(9.5))
                                    .text_color(theme.text_muted)
                                    .child(SharedString::from(format!(
                                        "{} · {}",
                                        worktree.status.label(),
                                        occupancy.card_label()
                                    ))),
                            )
                    })
                    .collect::<Vec<_>>();
                div()
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .child(self.inspector_header(repo_kind, repository.name.clone(), theme, cx))
                    .child(self.detail_row("Repository ID", repository.id.clone(), theme))
                    .child(self.detail_row("Full path", repository.full_path.clone(), theme))
                    .child(
                        self.detail_row(
                            "Branch / HEAD",
                            ref_label(repository.branch.as_deref(), repository.head_sha.as_deref())
                                .unwrap_or_else(|| "unknown".into()),
                            theme,
                        ),
                    )
                    .child(self.detail_row(
                        "Worktrees",
                        repository.worktrees.len().to_string(),
                        theme,
                    ))
                    .when(repository.worktrees.is_empty(), |element| {
                        element.child(
                            div()
                                .p(px(10.0))
                                .rounded(px(6.0))
                                .bg(theme.element_hover)
                                .text_size(crate::typography::ui_rems(10.0))
                                .text_color(theme.text_muted)
                                .child("No discovered worktrees"),
                        )
                    })
                    .children(worktrees)
                    .into_any_element()
            }
            TopologySelection::Worktree {
                repository_id,
                worktree_id,
            } => {
                let repository = snapshot
                    .repositories
                    .iter()
                    .find(|repository| &repository.id == repository_id)?;
                let worktree = repository
                    .worktrees
                    .iter()
                    .find(|worktree| &worktree.id == worktree_id)?;
                let occupancy = OccupancySummary::from_chats(&worktree.chats);
                let flags = [
                    worktree.is_main.then_some("main checkout"),
                    worktree.locked.then_some("locked"),
                    worktree.prunable.then_some("prunable"),
                    worktree.branch.is_none().then_some("detached"),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
                let has_chats = !worktree.chats.is_empty();
                let closeout_cwd = worktree.full_path.clone();
                let closeout_title =
                    format!("{} · {}", repository.name, worktree_ref_label(worktree));
                let closeout_chat_id = worktree_closeout_chat_id(worktree);
                let (
                    last_message_previews,
                    target_space_id,
                    target_device_id,
                    closeout_is_local,
                    closeout_preview,
                ) = {
                    let state = self.state.read(cx);
                    let previews = worktree
                        .chats
                        .iter()
                        .filter_map(|topology_chat| {
                            state
                                .chats
                                .iter()
                                .find(|chat| chat.id == topology_chat.id)
                                .and_then(|chat| chat.last_message_preview.as_ref())
                                .filter(|preview| !preview.trim().is_empty())
                                .map(|preview| (topology_chat.id.clone(), preview.clone()))
                        })
                        .collect::<HashMap<_, _>>();
                    let tabs = Self::workspace_tabs(&state);
                    let selected_tab = self.workspace_selection.selected(&tabs);
                    let device_id = selected_tab.map(|tab| tab.device_id.clone());
                    let closeout_key = closeout_preview_key(
                        selected_tab,
                        state.local_device_id.as_deref(),
                        worktree,
                    );
                    let closeout_preview = closeout_key
                        .as_ref()
                        .and_then(|key| self.closeout_previews.get(key))
                        .cloned();
                    (
                        previews,
                        selected_tab.map(|tab| tab.id.clone()),
                        device_id,
                        closeout_key.is_some(),
                        closeout_preview,
                    )
                };
                let chats = worktree
                    .chats
                    .iter()
                    .enumerate()
                    .map(|(index, chat)| {
                        let expanded = self.expanded_chats.contains(&chat.id);
                        let last_message_preview = last_message_previews.get(&chat.id).cloned();
                        let diff_peek = self.chat_diff_peeks.get(&chat.id);
                        let toggle_chat_id = chat.id.clone();
                        let open_chat_id = chat.id.clone();
                        let open_changes_chat_id = chat.id.clone();
                        let peek_changes_chat_id = chat.id.clone();
                        let peek_changes_cwd = worktree.full_path.clone();
                        let peek_changes_device_id = target_device_id.clone();
                        let mut meta = vec![
                            chat.status.label().to_string(),
                            format!(
                                "{} agent{}",
                                chat.agents.len(),
                                if chat.agents.len() == 1 { "" } else { "s" }
                            ),
                        ];
                        if chat.branch_mismatch {
                            meta.push("branch mismatch".into());
                        }
                        if chat.archived {
                            meta.push("archived".into());
                        }
                        if let Some(ticket) = &chat.linked_ticket_id {
                            meta.push(format!("ticket {ticket}"));
                        }
                        if !chat.linked_pr_urls.is_empty() {
                            meta.push(format!(
                                "{} PR{}",
                                chat.linked_pr_urls.len(),
                                if chat.linked_pr_urls.len() == 1 {
                                    ""
                                } else {
                                    "s"
                                }
                            ));
                        }
                        let pr_rows = chat
                            .linked_pr_urls
                            .iter()
                            .enumerate()
                            .map(|(pr_index, url)| {
                                let open_url = url.clone();
                                div()
                                    .id(("topology-inspector-chat-pr", index * 100 + pr_index))
                                    .mt(px(4.0))
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .cursor_pointer()
                                    .text_size(crate::typography::ui_rems(8.5))
                                    .text_color(theme.accent)
                                    .hover(|element| element.text_color(theme.busy))
                                    .child(SharedString::from(url.clone()))
                                    .on_click(move |_, _, cx| cx.open_url(&open_url))
                            })
                            .collect::<Vec<_>>();
                        let agent_rows = chat
                            .agents
                            .iter()
                            .map(|agent| {
                                div()
                                    .mt(px(4.0))
                                    .pl(px(8.0))
                                    .flex()
                                    .items_center()
                                    .gap(px(5.0))
                                    .text_size(crate::typography::ui_rems(9.0))
                                    .text_color(theme.text_muted)
                                    .child(
                                        div()
                                            .size(px(5.0))
                                            .rounded_full()
                                            .bg(agent.status.color(theme)),
                                    )
                                    .child(SharedString::from(format!(
                                        "{} · {} · {}",
                                        agent.name,
                                        agent.kind.as_deref().unwrap_or("agent"),
                                        agent.id
                                    )))
                            })
                            .collect::<Vec<_>>();
                        let diff_peek_element =
                            diff_peek.map(|peek| self.render_chat_diff_peek(peek, theme));
                        let peek_changes_label = match diff_peek {
                            Some(ChatDiffPeek::Loading { .. }) => "Loading…",
                            Some(ChatDiffPeek::Loaded(_)) => "Refresh changes",
                            Some(ChatDiffPeek::Failed(_)) => "Retry changes",
                            None => "Peek changes",
                        };
                        div()
                            .id(("topology-inspector-chat", index))
                            .rounded(px(7.0))
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.surface_card)
                            .child(
                                div()
                                    .id(("topology-inspector-chat-toggle", index))
                                    .p(px(9.0))
                                    .flex()
                                    .items_center()
                                    .gap(px(7.0))
                                    .cursor_pointer()
                                    .hover(|element| element.bg(theme.element_hover))
                                    .child(
                                        crate::icons::icon(if expanded {
                                            crate::icons::ALT_ARROW_DOWN
                                        } else {
                                            crate::icons::ALT_ARROW_RIGHT
                                        })
                                        .size(px(11.0))
                                        .text_color(theme.text_muted),
                                    )
                                    .child(
                                        div()
                                            .min_w_0()
                                            .flex_1()
                                            .flex()
                                            .flex_col()
                                            .gap(px(3.0))
                                            .child(
                                                div()
                                                    .overflow_hidden()
                                                    .whitespace_nowrap()
                                                    .text_ellipsis()
                                                    .text_size(crate::typography::ui_rems(10.5))
                                                    .font_weight(gpui::FontWeight::MEDIUM)
                                                    .text_color(theme.text)
                                                    .child(SharedString::from(chat.title.clone())),
                                            )
                                            .child(
                                                div()
                                                    .overflow_hidden()
                                                    .whitespace_nowrap()
                                                    .text_ellipsis()
                                                    .text_size(crate::typography::ui_rems(9.0))
                                                    .text_color(theme.text_muted)
                                                    .child(SharedString::from(meta.join(" · "))),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .size(px(6.0))
                                            .rounded_full()
                                            .bg(chat.status.color(theme)),
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        toggle_disclosure(
                                            &mut this.expanded_chats,
                                            &toggle_chat_id,
                                        );
                                        cx.notify();
                                    })),
                            )
                            .when(expanded, |element| {
                                element.child(
                                    div()
                                        .px(px(9.0))
                                        .pb(px(9.0))
                                        .border_t_1()
                                        .border_color(theme.border)
                                        .child(
                                            div()
                                                .mt(px(8.0))
                                                .text_size(crate::typography::ui_rems(8.5))
                                                .text_color(theme.text_faint)
                                                .child(SharedString::from(format!(
                                                    "ID {}{}{}",
                                                    chat.id,
                                                    chat.checkout_id
                                                        .as_ref()
                                                        .map(|id| format!(" · checkout {id}"))
                                                        .unwrap_or_default(),
                                                    chat.parent_chat_id
                                                        .as_ref()
                                                        .map(|id| format!(" · parent {id}"))
                                                        .unwrap_or_default()
                                                ))),
                                        )
                                        .when_some(
                                            chat.source_branch.clone(),
                                            |element, source_branch| {
                                                element.child(
                                                    div()
                                                        .mt(px(4.0))
                                                        .text_size(crate::typography::ui_rems(8.5))
                                                        .text_color(theme.text_faint)
                                                        .child(SharedString::from(format!(
                                                            "Started on {source_branch}"
                                                        ))),
                                                )
                                            },
                                        )
                                        .when_some(last_message_preview, |element, preview| {
                                            element.child(
                                                div()
                                                    .mt(px(8.0))
                                                    .p(px(8.0))
                                                    .rounded(px(6.0))
                                                    .bg(theme.element_hover)
                                                    .text_size(crate::typography::ui_rems(9.0))
                                                    .text_color(theme.text_muted)
                                                    .child(
                                                        div()
                                                            .mb(px(3.0))
                                                            .text_size(crate::typography::ui_rems(
                                                                8.0,
                                                            ))
                                                            .text_color(theme.text_faint)
                                                            .child("LATEST MESSAGE"),
                                                    )
                                                    .child(SharedString::from(preview)),
                                            )
                                        })
                                        .children(pr_rows)
                                        .children(agent_rows)
                                        .children(diff_peek_element)
                                        .child(
                                            div()
                                                .mt(px(9.0))
                                                .pt(px(8.0))
                                                .border_t_1()
                                                .border_color(theme.border)
                                                .flex()
                                                .items_center()
                                                .justify_between()
                                                .gap(px(8.0))
                                                .child(
                                                    div()
                                                        .id((
                                                            "topology-inspector-chat-peek-changes",
                                                            index,
                                                        ))
                                                        .flex_none()
                                                        .px(px(8.0))
                                                        .py(px(4.0))
                                                        .rounded(px(5.0))
                                                        .cursor_pointer()
                                                        .bg(theme.element_hover)
                                                        .text_size(crate::typography::ui_rems(9.0))
                                                        .text_color(theme.text_muted)
                                                        .hover(|element| {
                                                            element
                                                                .bg(theme.element_active)
                                                                .text_color(theme.text)
                                                        })
                                                        .on_click(cx.listener(
                                                            move |this, _, _, cx| {
                                                                this.load_chat_diff(
                                                                    peek_changes_chat_id.clone(),
                                                                    peek_changes_cwd.clone(),
                                                                    peek_changes_device_id.clone(),
                                                                    cx,
                                                                );
                                                            },
                                                        ))
                                                        .child(peek_changes_label),
                                                )
                                                .child(
                                                    div()
                                                        .id(("topology-inspector-chat-open", index))
                                                        .flex_none()
                                                        .px(px(8.0))
                                                        .py(px(4.0))
                                                        .rounded(px(5.0))
                                                        .cursor_pointer()
                                                        .bg(theme.accent.opacity(0.12))
                                                        .text_size(crate::typography::ui_rems(9.0))
                                                        .text_color(theme.accent)
                                                        .hover(|element| {
                                                            element.bg(theme.accent.opacity(0.2))
                                                        })
                                                        .on_mouse_down(
                                                            MouseButton::Left,
                                                            |_, _, cx| cx.stop_propagation(),
                                                        )
                                                        .on_click(cx.listener(
                                                            move |this, _, _, cx| {
                                                                if let Some(shell) =
                                                                    this.shell.upgrade()
                                                                {
                                                                    shell.update(
                                                                        cx,
                                                                        |shell, cx| {
                                                                            shell.open_chat(
                                                                                open_chat_id
                                                                                    .clone(),
                                                                                cx,
                                                                            )
                                                                        },
                                                                    );
                                                                }
                                                            },
                                                        ))
                                                        .child("Open chat →"),
                                                )
                                                .child(
                                                    div()
                                                        .id((
                                                            "topology-inspector-chat-open-changes",
                                                            index,
                                                        ))
                                                        .flex_none()
                                                        .px(px(8.0))
                                                        .py(px(4.0))
                                                        .rounded(px(5.0))
                                                        .cursor_pointer()
                                                        .bg(theme.accent.opacity(0.12))
                                                        .text_size(crate::typography::ui_rems(9.0))
                                                        .text_color(theme.accent)
                                                        .hover(|element| {
                                                            element.bg(theme.accent.opacity(0.2))
                                                        })
                                                        .on_click(cx.listener(
                                                            move |this, _, _, cx| {
                                                                if let Some(shell) =
                                                                    this.shell.upgrade()
                                                                {
                                                                    shell.update(
                                                                        cx,
                                                                        |shell, cx| {
                                                                            shell.open_chat_changes(
                                                                                open_changes_chat_id
                                                                                    .clone(),
                                                                                cx,
                                                                            )
                                                                        },
                                                                    );
                                                                }
                                                            },
                                                        ))
                                                        .child("Open changes →"),
                                                ),
                                        ),
                                )
                            })
                    })
                    .collect::<Vec<_>>();
                let new_chat_path = worktree.full_path.clone();
                let new_chat_branch = worktree.branch.clone();
                div()
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .child(self.inspector_header(
                        "WORKTREE",
                        worktree_ref_label(worktree),
                        theme,
                        cx,
                    ))
                    .child(self.detail_row("Repository", repository.name.clone(), theme))
                    .child(self.detail_row("Worktree / checkout ID", worktree.id.clone(), theme))
                    .child(self.detail_row("Full path", worktree.full_path.clone(), theme))
                    .child(
                        self.detail_row(
                            "HEAD",
                            worktree
                                .head_sha
                                .clone()
                                .unwrap_or_else(|| "unknown".into()),
                            theme,
                        ),
                    )
                    .child(self.detail_row(
                        "Git state",
                        format!(
                                "{} · {}",
                                worktree.status.label(),
                                sync_detail(worktree.ahead, worktree.behind)
                                    .unwrap_or_else(|| "upstream unknown".into())
                            ),
                        theme,
                    ))
                    .when(!flags.is_empty(), |element| {
                        element.child(self.detail_row("Flags", flags, theme))
                    })
                    .child(self.detail_row(
                        "Occupancy",
                        format!(
                            "{} chats · {} agents · {} active · {} mismatched",
                            occupancy.chats,
                            occupancy.agents,
                            occupancy.active(),
                            occupancy.branch_mismatches
                        ),
                        theme,
                    ))
                    .when_some(target_space_id, |element, space_id| {
                        element.child(
                            div()
                                .id("topology-inspector-new-chat-in-worktree")
                                .w_full()
                                .px(px(10.0))
                                .py(px(7.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .gap(px(6.0))
                                .rounded(px(6.0))
                                .bg(theme.accent.opacity(0.12))
                                .cursor_pointer()
                                .text_size(crate::typography::ui_rems(10.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.accent)
                                .hover(|element| element.bg(theme.accent.opacity(0.2)))
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(shell) = this.shell.upgrade() {
                                        shell.update(cx, |shell, cx| {
                                            shell.open_new_session_in_worktree(
                                                space_id.clone(),
                                                new_chat_path.clone(),
                                                new_chat_branch.clone(),
                                                cx,
                                            )
                                        });
                                    }
                                }))
                                .child(
                                    crate::icons::icon(crate::icons::PEN_NEW_SQUARE).size(px(11.0)),
                                )
                                .child("New chat in this worktree"),
                        )
                    })
                    .when_some(closeout_preview, |element, preview| {
                        element.child(render_closeout_preview(&preview, theme))
                    })
                    .when(closeout_is_local, |element| {
                        element.child(
                            div()
                                .id("topology-inspector-closeout-worktree")
                                .w_full()
                                .px(px(10.0))
                                .py(px(7.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(6.0))
                                .border_1()
                                .border_color(theme.danger.opacity(0.4))
                                .bg(theme.danger.opacity(0.08))
                                .cursor_pointer()
                                .text_size(crate::typography::ui_rems(10.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.danger)
                                .hover(|element| element.bg(theme.danger.opacity(0.16)))
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(shell) = this.shell.upgrade() {
                                        shell.update(cx, |shell, cx| {
                                            shell.open_worktree_closeout(
                                                closeout_cwd.clone(),
                                                closeout_title.clone(),
                                                closeout_chat_id.clone(),
                                                cx,
                                            )
                                        });
                                    }
                                }))
                                .child("Close out worktree\u{2026}"),
                        )
                    })
                    .child(
                        div()
                            .pt(px(3.0))
                            .text_size(crate::typography::ui_rems(9.0))
                            .text_color(theme.text_faint)
                            .child("MATCHED CHATS"),
                    )
                    .when(!has_chats, |element| {
                        element.child(
                            div()
                                .p(px(10.0))
                                .rounded(px(6.0))
                                .bg(theme.element_hover)
                                .text_size(crate::typography::ui_rems(10.0))
                                .text_color(theme.text_muted)
                                .child("No chat is matched to this worktree"),
                        )
                    })
                    .children(chats)
                    .into_any_element()
            }
        };
        Some(
            div()
                .id("topology-inspector")
                .absolute()
                .right(px(14.0))
                .top(px(14.0))
                .bottom(px(54.0))
                .w(px(348.0))
                .p(px(14.0))
                .overflow_y_scroll()
                .rounded(px(10.0))
                .border_1()
                .border_color(theme.border_strong)
                .bg(theme.surface_raised)
                .shadow_md()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(content)
                .into_any_element(),
        )
    }

    fn render_graph(
        &mut self,
        snapshot: &TopologySnapshot,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let layout = GraphLayout::from_snapshot(snapshot);
        if self.auto_fit && self.canvas_bounds.get().is_some() {
            self.fit_layout(&layout);
            self.auto_fit = false;
        }
        let zoom = self.zoom;
        let pan = self.pan;
        let stroke = theme.border_strong;
        let edge_layout = layout.clone();
        let edges = gpui::canvas(
            |_, _, _| (),
            move |bounds, _, window, _| {
                for edge in &edge_layout.edges {
                    let (Some(parent), Some(child)) = (
                        edge_layout.node(&edge.parent),
                        edge_layout.node(&edge.child),
                    ) else {
                        continue;
                    };
                    let parent_rect = screen_node_rect(parent, zoom, pan);
                    let child_rect = screen_node_rect(child, zoom, pan);
                    let start = (
                        parent_rect.x + parent_rect.width,
                        parent_rect.y + parent_rect.height / 2.0,
                    );
                    let end = (child_rect.x, child_rect.y + child_rect.height / 2.0);
                    let elbow = (start.0 + end.0) / 2.0;
                    let mut builder = PathBuilder::stroke(px((1.2 * zoom).clamp(0.8, 1.6)));
                    builder.move_to(point(
                        bounds.origin.x + px(start.0),
                        bounds.origin.y + px(start.1),
                    ));
                    builder.line_to(point(
                        bounds.origin.x + px(elbow),
                        bounds.origin.y + px(start.1),
                    ));
                    builder.line_to(point(
                        bounds.origin.x + px(elbow),
                        bounds.origin.y + px(end.1),
                    ));
                    builder.line_to(point(
                        bounds.origin.x + px(end.0),
                        bounds.origin.y + px(end.1),
                    ));
                    if let Ok(path) = builder.build() {
                        window.paint_path(path, stroke);
                    }
                }
            },
        )
        .absolute()
        .inset_0();

        let relation_labels = layout.edges.iter().filter_map(|edge| {
            let parent = layout.node(&edge.parent)?;
            let child = layout.node(&edge.child)?;
            let parent_rect = screen_node_rect(parent, zoom, pan);
            let child_rect = screen_node_rect(child, zoom, pan);
            let x = (parent_rect.x + parent_rect.width + child_rect.x) / 2.0;
            let y = child_rect.y + child_rect.height / 2.0;
            Some(
                div()
                    .absolute()
                    .left(px(x - 22.0))
                    .top(px(y - 9.0))
                    .px(px(4.0))
                    .py(px(1.0))
                    .rounded(px(3.0))
                    .bg(theme.surface)
                    .text_size(crate::typography::ui_rems(8.5 * zoom))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(edge.relation)),
            )
        });
        let nodes = layout
            .nodes
            .iter()
            .enumerate()
            .map(|(index, node)| self.render_node(node, theme, index, cx))
            .collect::<Vec<_>>();
        let inspector = self.render_inspector(snapshot, theme, cx);
        let bounds_cell = self.canvas_bounds.clone();
        let fit_layout = layout.clone();
        let zoom_pct = (self.zoom * 100.0).round() as u32;
        let controls = div()
            .absolute()
            .right(px(14.0))
            .bottom(px(14.0))
            .h(px(30.0))
            .px(px(4.0))
            .flex()
            .items_center()
            .gap(px(2.0))
            .rounded(px(7.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface_raised)
            .shadow_sm()
            .child(self.zoom_button("topology-zoom-out", "−", theme, cx, 1.0 / 1.2))
            .child(
                div()
                    .id("topology-zoom-reset")
                    .w(px(44.0))
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .text_size(crate::typography::ui_rems(10.5))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(format!("{zoom_pct}%")))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.zoom = 1.0;
                        this.pan = (20.0, 20.0);
                        cx.notify();
                    })),
            )
            .child(self.zoom_button("topology-zoom-in", "+", theme, cx, 1.2))
            .child(
                div()
                    .id("topology-fit")
                    .size(px(22.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .text_color(theme.text_muted)
                    .hover(|element| element.bg(theme.element_hover).text_color(theme.text))
                    .child(crate::icons::icon(crate::icons::EXPAND_ARROWS).size(px(11.0)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.fit_layout(&fit_layout);
                        cx.notify();
                    })),
            );
        div()
            .id("repository-topology-canvas")
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(theme.surface)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _, _| {
                    this.panning = Some((
                        (f32::from(event.position.x), f32::from(event.position.y)),
                        this.pan,
                    ));
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                if let Some((start, pan)) = this.panning {
                    let cursor = (f32::from(event.position.x), f32::from(event.position.y));
                    this.pan = (pan.0 + cursor.0 - start.0, pan.1 + cursor.1 - start.1);
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, _| this.panning = None),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, _| this.panning = None),
            )
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                let delta = event.delta.pixel_delta(px(1.0));
                if event.modifiers.control {
                    let factor = (-f32::from(delta.y) * 0.01).exp();
                    this.zoom_toward(
                        this.zoom * factor,
                        (f32::from(event.position.x), f32::from(event.position.y)),
                    );
                } else {
                    this.pan.0 += f32::from(delta.x);
                    this.pan.1 += f32::from(delta.y);
                }
                cx.notify();
            }))
            .on_pinch(cx.listener(|this, event: &PinchEvent, _, cx| {
                this.zoom_toward(
                    this.zoom * (1.0 + event.delta),
                    (f32::from(event.position.x), f32::from(event.position.y)),
                );
                cx.notify();
            }))
            .child(edges)
            .children(relation_labels)
            .children(nodes)
            .child(
                gpui::canvas(
                    move |bounds, _, _| bounds_cell.set(Some(bounds)),
                    |_, _, _, _| {},
                )
                .absolute()
                .inset_0(),
            )
            .child(controls)
            .children(inspector)
            .into_any_element()
    }

    fn zoom_button(
        &self,
        id: &'static str,
        label: &'static str,
        theme: &Theme,
        cx: &mut Context<Self>,
        factor: f32,
    ) -> gpui::AnyElement {
        div()
            .id(id)
            .size(px(22.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(4.0))
            .cursor_pointer()
            .text_size(crate::typography::ui_rems(13.0))
            .text_color(theme.text_muted)
            .hover(|element| element.bg(theme.element_hover).text_color(theme.text))
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| {
                let center = this
                    .canvas_bounds
                    .get()
                    .map(|bounds| {
                        (
                            f32::from(bounds.origin.x + bounds.size.width / 2.0),
                            f32::from(bounds.origin.y + bounds.size.height / 2.0),
                        )
                    })
                    .unwrap_or((0.0, 0.0));
                this.zoom_toward(this.zoom * factor, center);
                cx.notify();
            }))
            .into_any_element()
    }

    fn render_workspace_tabs(
        &self,
        tabs: &[WorkspaceTab],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let selected_id = self.workspace_selection.selected_id.as_deref();
        div()
            .id("repository-topology-workspace-tabs")
            .h(px(34.0))
            .w_full()
            .px(px(12.0))
            .flex()
            .items_end()
            .gap(px(3.0))
            .overflow_x_scroll()
            .children(tabs.iter().map(|tab| {
                let active = selected_id == Some(tab.id.as_str());
                let space_id = tab.id.clone();
                let path = tab.path.clone();
                let tab = div()
                    .id(SharedString::from(format!(
                        "repository-topology-tab-{space_id}"
                    )))
                    .h(px(30.0))
                    .px(px(10.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .rounded_t(px(6.0))
                    .border_b_2()
                    .cursor_pointer()
                    .whitespace_nowrap()
                    .text_size(crate::typography::ui_rems(11.0))
                    .child(crate::icons::icon(crate::icons::FOLDER).size(px(12.0)))
                    .child(SharedString::from(tab.display_name.clone()))
                    .tooltip(move |_, cx| {
                        cx.new(|_| TopologyTooltip(SharedString::from(path.clone())))
                            .into()
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_workspace(&space_id, cx);
                    }));
                if active {
                    tab.bg(theme.element_active)
                        .border_color(theme.accent)
                        .text_color(theme.text)
                } else {
                    tab.border_color(gpui::transparent_black())
                        .text_color(theme.text_muted)
                        .hover(|element| element.bg(theme.element_hover).text_color(theme.text))
                }
            }))
            .when(tabs.is_empty(), |row| {
                row.items_center().child(
                    div()
                        .px(px(4.0))
                        .text_size(crate::typography::ui_rems(11.0))
                        .text_color(theme.text_faint)
                        .child("No workspaces available"),
                )
            })
            .into_any_element()
    }
}

impl Render for RepositoryTopology {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let workspace_tabs = self.reconcile_workspace_tabs(cx);
        self.ensure_loaded(&workspace_tabs, cx);
        let theme = Theme::of(cx).clone();
        let selected_workspace = self.workspace_selection.selected(&workspace_tabs).cloned();
        let search_query = self.search.read(cx).text().trim().to_string();
        let search_active = !search_query.is_empty();
        let search_control = div()
            .id("repository-topology-search")
            .w(px(340.0))
            .h(px(32.0))
            .flex_none()
            .px(px(9.0))
            .flex()
            .items_center()
            .gap(px(7.0))
            .rounded(px(7.0))
            .border_1()
            .border_color(if search_active {
                theme.border_strong
            } else {
                theme.border
            })
            .bg(theme.surface_raised)
            .text_color(theme.text_muted)
            .child(crate::icons::icon(crate::icons::MAGNIFER).size(px(12.0)))
            .child(div().min_w_0().flex_1().child(self.search.clone()))
            .when(search_active, |row| {
                row.child(
                    div()
                        .id("repository-topology-search-clear")
                        .size(px(20.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .role(gpui::Role::Button)
                        .aria_label("Clear repository map search")
                        .hover(|element| element.bg(theme.element_hover).text_color(theme.text))
                        .child(crate::icons::icon(crate::icons::CLOSE_CIRCLE).size(px(12.0)))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.search.update(cx, |input, cx| input.set_text("", cx));
                            this.auto_fit = true;
                            cx.notify();
                        })),
                )
            });
        let header = div()
            .flex_none()
            .flex()
            .flex_col()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .h(px(58.0))
                    .px(px(16.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .min_w_0()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.0))
                                    .text_size(crate::typography::ui_rems(14.0))
                                    .text_color(theme.text)
                                    .child("Repository map")
                                    .when_some(selected_workspace.as_ref(), |row, workspace| {
                                        row.child(
                                            div()
                                                .px(px(6.0))
                                                .py(px(2.0))
                                                .rounded(px(4.0))
                                                .bg(theme.element_hover)
                                                .text_size(crate::typography::ui_rems(10.0))
                                                .text_color(theme.text_muted)
                                                .child(SharedString::from(
                                                    workspace.display_name.clone(),
                                                )),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .max_w(px(720.0))
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_size(crate::typography::ui_rems(11.0))
                                    .text_color(theme.text_muted)
                                    .when_some(selected_workspace.as_ref(), |row, workspace| {
                                        row.child(SharedString::from(workspace.path.clone()))
                                    })
                                    .when(selected_workspace.is_none(), |row| {
                                        row.child("Choose a workspace to map its repositories")
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(search_control)
                            .child(
                                div()
                                    .id("topology-refresh")
                                    .size(px(30.0))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(6.0))
                                    .cursor_pointer()
                                    .text_color(theme.text_muted)
                                    .hover(|element| {
                                        element.bg(theme.element_hover).text_color(theme.text)
                                    })
                                    .child(crate::icons::icon(crate::icons::REFRESH).size(px(14.0)))
                                    .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                            ),
                    ),
            )
            .child(self.render_workspace_tabs(&workspace_tabs, &theme, cx));
        let body = match self.load.clone() {
            LoadState::Idle if selected_workspace.is_none() => self.render_center_state(
                &theme,
                "No workspace available",
                "Add a workspace to map its repositories and worktrees.",
                None,
                cx,
            ),
            LoadState::Idle | LoadState::Loading => {
                let view = cx.entity_id();
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(12.0))
                    .child(crate::loaders::mini_mono_spinner(
                        "repository-topology-loading",
                        3.0,
                        theme.text_muted,
                        view,
                        cx,
                    ))
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.text_muted)
                            .child("Mapping repositories and worktrees…"),
                    )
                    .into_any_element()
            }
            LoadState::Failed(error) => self.render_center_state(
                &theme,
                "Couldn’t load the repository map",
                &error,
                Some("Try again"),
                cx,
            ),
            LoadState::Loaded(snapshot) | LoadState::Refreshing(snapshot) => {
                if snapshot.repositories.is_empty() {
                    self.render_center_state(
                        &theme,
                        "No Git repositories found",
                        "This workspace does not contain a detected repository or worktree yet.",
                        Some("Refresh"),
                        cx,
                    )
                } else if search_active {
                    let result = topology_search(&snapshot, &search_query);
                    if result.direct_matches == 0 {
                        self.render_center_state(
                            &theme,
                            "No repository map matches",
                            &format!(
                                "No repository, worktree, branch, path, or attached chat matches ‘{search_query}’."
                            ),
                            None,
                            cx,
                        )
                    } else {
                        self.render_graph(&result.snapshot, &theme, cx)
                    }
                } else {
                    self.render_graph(&snapshot, &theme, cx)
                }
            }
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg)
            .child(header)
            .child(div().flex_1().min_h_0().child(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace_tabs() -> Vec<WorkspaceTab> {
        vec![
            WorkspaceTab {
                id: "space-alpha".into(),
                device_id: "device-a".into(),
                display_name: "Alpha".into(),
                path: "/work/alpha".into(),
            },
            WorkspaceTab {
                id: "space-beta".into(),
                device_id: "device-b".into(),
                display_name: "Beta".into(),
                path: "/work/beta".into(),
            },
        ]
    }

    fn fixture() -> TopologySnapshot {
        TopologyAdapter::decode(serde_json::json!({
            "workspace": {
                "id": "space-1",
                "name": "zeron",
                "path": "/src/zeron",
                "repositories": [{
                    "id": "repo-root",
                    "name": "zeron",
                    "path": "/src/zeron",
                    "kind": "workspace",
                    "branch": "main",
                    "status": "modified",
                    "worktrees": [{
                        "id": "wt-main",
                        "checkoutId": "checkout-main",
                        "name": "zeron",
                        "path": "/src/zeron",
                        "branch": "main",
                        "headSha": "abcdef1234567890",
                        "status": "modified",
                        "isMain": true,
                        "locked": true,
                        "chats": [{
                            "id": "chat-1",
                            "title": "Build repository map",
                            "indicator": "working",
                            "checkoutId": "checkout-main",
                            "parentChatId": "chat-parent",
                            "linkedTicketId": "ENG-42",
                            "linkedPrUrls": ["https://github.com/acme/zeron/pull/42"],
                            "agents": [{
                                "id": "agent-1",
                                "name": "Codex",
                                "kind": "codex",
                                "status": "working"
                            }]
                        }]
                    }]
                }, {
                    "id": "repo-sub",
                    "parentRepositoryId": "repo-root",
                    "name": "theme",
                    "path": "/src/zeron/crates/theme",
                    "relativePath": "crates/theme",
                    "kind": "submodule",
                    "branch": "feature/colors",
                    "status": "clean",
                    "worktrees": []
                }]
            }
        }))
        .unwrap()
    }

    #[test]
    fn adapter_flattens_nested_repositories_and_prefers_compact_paths() {
        let snapshot = TopologyAdapter::decode(serde_json::json!({
            "workspace": {
                "id": "w", "name": "workspace", "path": "/workspace",
                "repositories": [{
                    "id": "parent", "name": "parent", "path": "/workspace",
                    "kind": "workspace",
                    "repositories": [{
                        "id": "child", "name": "child", "path": "/workspace/vendor/child",
                        "relativePath": "vendor/child", "kind": "submodule"
                    }]
                }]
            }
        }))
        .unwrap();
        assert_eq!(snapshot.repositories.len(), 2);
        assert_eq!(
            snapshot.repositories[1].parent_id.as_deref(),
            Some("parent")
        );
        assert_eq!(snapshot.repositories[1].path, "vendor/child");
        assert_eq!(
            snapshot.repositories[1].full_path,
            "/workspace/vendor/child"
        );
    }

    #[test]
    fn graph_is_repository_and_worktree_first() {
        let layout = GraphLayout::from_snapshot(&fixture());
        assert_eq!(layout.nodes.len(), 4);
        assert!(layout.edges.iter().any(|edge| edge.relation == "root repo"));
        assert!(layout.edges.iter().any(|edge| edge.relation == "submodule"));
        assert!(layout.edges.iter().any(|edge| edge.relation == "worktree"));
        assert!(!layout.edges.iter().any(|edge| edge.relation == "chat"));
        assert!(!layout.edges.iter().any(|edge| edge.relation == "agent"));
        for edge in &layout.edges {
            let parent = layout.node(&edge.parent).unwrap();
            let child = layout.node(&edge.child).unwrap();
            assert!(
                child.x > parent.x,
                "{} should be right of {}",
                child.id,
                parent.id
            );
        }
    }

    #[test]
    fn many_worktrees_use_a_dense_grid_without_losing_card_data() {
        let mut snapshot = fixture();
        let template = snapshot.repositories[0].worktrees[0].clone();
        snapshot.repositories[0].worktrees = (0..24)
            .map(|index| TopologyWorktree {
                id: format!("checkout-{index}"),
                name: format!("worktree-{index}"),
                path: format!(".worktrees/worktree-{index}"),
                full_path: format!("/src/zeron/.worktrees/worktree-{index}"),
                branch: Some(format!("feature/worktree-{index}")),
                ..template.clone()
            })
            .collect();

        let layout = GraphLayout::from_snapshot(&snapshot);
        let worktrees = layout
            .nodes
            .iter()
            .filter(|node| node.kind == GraphNodeKind::Worktree)
            .collect::<Vec<_>>();
        let columns = worktrees
            .iter()
            .map(|node| node.x.to_bits())
            .collect::<HashSet<_>>();
        let rows = worktrees
            .iter()
            .map(|node| node.y.to_bits())
            .collect::<HashSet<_>>();

        assert_eq!(worktrees.len(), 24);
        assert_eq!(columns.len(), 4);
        assert_eq!(rows.len(), 6);
        assert!(
            layout.height < 1_100.0,
            "dense layout was {}px tall",
            layout.height
        );

        let last = worktrees
            .iter()
            .find(|node| node.id.ends_with("checkout-23"))
            .unwrap();
        assert_eq!(last.title, "feature/worktree-23");
        assert_eq!(last.path.as_deref(), Some(".worktrees/worktree-23"));
        assert_eq!(last.status, template.status);
        assert_eq!(last.occupancy.as_ref().unwrap().chats, template.chats.len());
        assert_eq!(last.height, 116.0);
    }

    #[test]
    fn search_matches_attached_chat_title_and_id() {
        let result = topology_search(&fixture(), "CHAT-1 repository map");

        assert_eq!(result.direct_matches, 1);
        assert_eq!(result.matching_chat_ids, vec!["chat-1"]);
        assert_eq!(
            result.focus,
            Some(TopologySelection::Worktree {
                repository_id: "repo-root".into(),
                worktree_id: "checkout-main".into(),
            })
        );
        assert_eq!(result.snapshot.repositories.len(), 1);
        assert_eq!(result.snapshot.repositories[0].id, "repo-root");
        assert_eq!(result.snapshot.repositories[0].worktrees.len(), 1);
    }

    #[test]
    fn search_keeps_repository_ancestors_but_removes_unmatched_worktrees() {
        let result = topology_search(&fixture(), "feature/colors");

        assert_eq!(result.direct_matches, 1);
        assert_eq!(result.snapshot.repositories.len(), 2);
        assert_eq!(result.snapshot.repositories[0].id, "repo-root");
        assert!(result.snapshot.repositories[0].worktrees.is_empty());
        assert_eq!(result.snapshot.repositories[1].id, "repo-sub");
    }

    #[test]
    fn direct_repository_match_keeps_its_worktrees() {
        let result = topology_search(&fixture(), "repo-root");

        assert_eq!(result.direct_matches, 1);
        assert_eq!(result.snapshot.repositories.len(), 1);
        assert_eq!(result.snapshot.repositories[0].worktrees.len(), 1);
    }

    #[test]
    fn search_returns_an_empty_map_when_nothing_matches() {
        let result = topology_search(&fixture(), "missing-saturn-repository");

        assert_eq!(result.direct_matches, 0);
        assert!(result.snapshot.repositories.is_empty());
    }

    #[test]
    fn screen_node_rect_scales_bounds_and_center_by_the_same_zoom() {
        let layout = GraphLayout::from_snapshot(&fixture());
        let node = &layout.nodes[0];
        let zoom = MIN_ZOOM;
        let pan = (17.0, 29.0);
        let rect = screen_node_rect(node, zoom, pan);

        assert_eq!(rect.width, node.width * zoom);
        assert_eq!(rect.height, node.height * zoom);
        assert_eq!(rect.content_scale, zoom);
        assert_eq!(
            rect.x + rect.width / 2.0,
            (node.x + node.width / 2.0) * zoom + pan.0
        );
        assert_eq!(
            rect.y + rect.height / 2.0,
            (node.y + node.height / 2.0) * zoom + pan.1
        );
    }

    #[test]
    fn worktree_card_leads_with_branch_and_keeps_git_and_occupancy_separate() {
        let layout = GraphLayout::from_snapshot(&fixture());
        let worktree = layout
            .nodes
            .iter()
            .find(|node| node.kind == GraphNodeKind::Worktree)
            .unwrap();
        assert_eq!(worktree.title, "main");
        assert_eq!(worktree.branch.as_deref(), Some("zeron"));
        assert_eq!(worktree.status, TopologyStatus::Modified);
        assert_eq!(worktree.detail.as_deref(), Some("1 active · 1 chat"));
        assert_eq!(worktree.occupancy.as_ref().unwrap().working, 1);
        assert!(worktree.is_main);
        assert!(worktree.locked);
    }

    #[test]
    fn adapter_preserves_inspector_metadata_and_full_worktree_path() {
        let snapshot = fixture();
        let worktree = &snapshot.repositories[0].worktrees[0];
        let chat = &worktree.chats[0];
        assert_eq!(worktree.full_path, "/src/zeron");
        assert_eq!(worktree.head_sha.as_deref(), Some("abcdef1234567890"));
        assert_eq!(chat.checkout_id.as_deref(), Some("checkout-main"));
        assert_eq!(chat.parent_chat_id.as_deref(), Some("chat-parent"));
        assert_eq!(chat.linked_ticket_id.as_deref(), Some("ENG-42"));
        assert_eq!(chat.linked_pr_urls.len(), 1);
    }

    #[test]
    fn unknown_upstream_is_not_presented_as_up_to_date() {
        assert_eq!(sync_detail(None, None).as_deref(), Some("upstream unknown"));
        assert_eq!(sync_detail(Some(0), Some(0)).as_deref(), Some("up to date"));
        assert_eq!(sync_detail(Some(2), Some(1)).as_deref(), Some("↑2 ↓1"));
    }

    #[test]
    fn detached_worktree_uses_short_sha_as_primary_identity() {
        let mut snapshot = fixture();
        let worktree = &mut snapshot.repositories[0].worktrees[0];
        worktree.branch = None;
        assert_eq!(worktree_ref_label(worktree), "detached @ abcdef12");
    }

    #[test]
    fn closeout_is_only_offered_for_non_main_worktrees() {
        let mut snapshot = fixture();
        let worktree = &mut snapshot.repositories[0].worktrees[0];
        assert!(!worktree_can_closeout(worktree));

        worktree.is_main = false;
        assert!(worktree_can_closeout(worktree));
        assert_eq!(
            worktree_closeout_chat_id(worktree).as_deref(),
            Some("chat-1")
        );

        worktree.chats.clear();
        assert!(worktree_can_closeout(worktree));
        assert_eq!(worktree_closeout_chat_id(worktree), None);
    }

    #[test]
    fn closeout_preview_key_requires_the_local_workspace_owner() {
        let tabs = workspace_tabs();
        let mut snapshot = fixture();
        let worktree = &mut snapshot.repositories[0].worktrees[0];
        worktree.is_main = false;

        let key = closeout_preview_key(Some(&tabs[0]), Some("device-a"), worktree).unwrap();
        assert_eq!(key.device_id, "device-a");
        assert_eq!(key.cwd, "/src/zeron");
        assert_eq!(key.chat_id.as_deref(), Some("chat-1"));
        assert!(closeout_preview_key(Some(&tabs[0]), Some("device-b"), worktree).is_none());

        worktree.is_main = true;
        assert!(closeout_preview_key(Some(&tabs[0]), Some("device-a"), worktree).is_none());
    }

    #[test]
    fn closeout_preview_ignores_stale_completions_and_keys_all_inputs() {
        let first_key = WorktreeCloseoutPreviewKey {
            device_id: "device-a".into(),
            cwd: "/repo/one".into(),
            chat_id: Some("chat-1".into()),
        };
        let other_key = WorktreeCloseoutPreviewKey {
            device_id: "device-a".into(),
            cwd: "/repo/one".into(),
            chat_id: Some("chat-2".into()),
        };
        let mut previews = HashMap::new();
        let mut sequence = 0;
        let request_id =
            begin_closeout_preview_request(&mut previews, &mut sequence, &first_key, false)
                .unwrap();

        assert!(
            begin_closeout_preview_request(&mut previews, &mut sequence, &first_key, true)
                .is_none()
        );
        assert!(!finish_closeout_preview_request(
            &mut previews,
            first_key.clone(),
            request_id + 1,
            Ok(crate::chat_closeout::CloseoutPlan::default()),
        ));
        assert!(matches!(
            previews.get(&first_key),
            Some(WorktreeCloseoutPreview::Loading { request_id: active })
                if *active == request_id
        ));

        assert!(finish_closeout_preview_request(
            &mut previews,
            first_key.clone(),
            request_id,
            Ok(crate::chat_closeout::CloseoutPlan::default()),
        ));
        assert!(matches!(
            previews.get(&first_key),
            Some(WorktreeCloseoutPreview::Loaded(_))
        ));

        let other_request =
            begin_closeout_preview_request(&mut previews, &mut sequence, &other_key, false)
                .unwrap();
        assert_ne!(other_request, request_id);
        assert!(previews.contains_key(&first_key));
        assert!(previews.contains_key(&other_key));
    }

    #[test]
    fn closeout_preview_refreshes_only_for_explicit_selection_or_a_new_key() {
        let key = WorktreeCloseoutPreviewKey {
            device_id: "device-a".into(),
            cwd: "/repo/one".into(),
            chat_id: Some("chat-1".into()),
        };
        let mut previews = HashMap::from([(
            key.clone(),
            WorktreeCloseoutPreview::Loaded(crate::chat_closeout::CloseoutPlan::default()),
        )]);
        let mut sequence = 7;

        assert!(
            begin_closeout_preview_request(&mut previews, &mut sequence, &key, false).is_none()
        );
        assert_eq!(sequence, 7);

        let forced =
            begin_closeout_preview_request(&mut previews, &mut sequence, &key, true).unwrap();
        assert_eq!(forced, 8);
        assert!(matches!(
            previews.get(&key),
            Some(WorktreeCloseoutPreview::Loading { request_id: 8 })
        ));

        let changed_key = WorktreeCloseoutPreviewKey {
            chat_id: Some("chat-2".into()),
            ..key
        };
        let background =
            begin_closeout_preview_request(&mut previews, &mut sequence, &changed_key, false)
                .unwrap();
        assert_eq!(background, 9);
    }

    #[test]
    fn chat_disclosure_toggles_without_affecting_other_chats() {
        let mut expanded = HashSet::from(["chat-other".to_string()]);

        assert!(toggle_disclosure(&mut expanded, "chat-1"));
        assert!(expanded.contains("chat-1"));
        assert!(expanded.contains("chat-other"));

        assert!(!toggle_disclosure(&mut expanded, "chat-1"));
        assert!(!expanded.contains("chat-1"));
        assert!(expanded.contains("chat-other"));
    }

    #[test]
    fn background_refresh_keeps_the_last_snapshot_visible() {
        let snapshot = fixture();
        let mut load = LoadState::Loaded(snapshot.clone());

        assert!(load.begin_request());
        assert!(matches!(load, LoadState::Refreshing(ref current) if current == &snapshot));

        assert!(!load.finish_request(Err("temporary failure".into())));
        assert!(matches!(load, LoadState::Loaded(ref current) if current == &snapshot));
    }

    #[test]
    fn initial_load_still_uses_loading_and_failure_states() {
        let mut load = LoadState::Idle;

        assert!(!load.begin_request());
        assert!(matches!(load, LoadState::Loading));

        assert!(!load.finish_request(Err("unavailable".into())));
        assert!(matches!(load, LoadState::Failed(ref error) if error == "unavailable"));
    }

    fn diff_summary() -> ChatDiffSummary {
        ChatDiffSummary {
            files: vec![zeron_proto::DiffFileSummary {
                path: "src/main.rs".into(),
                old_path: None,
                status: "M".into(),
                additions: 4,
                deletions: 2,
                binary: false,
            }],
            additions: 4,
            deletions: 2,
            truncated: false,
        }
    }

    #[test]
    fn diff_peek_ignores_stale_completions_and_can_refresh_loaded_data() {
        let mut peeks = HashMap::new();
        let mut sequence = 0;

        let first = begin_chat_diff_request(&mut peeks, &mut sequence, "chat-1").unwrap();
        assert!(begin_chat_diff_request(&mut peeks, &mut sequence, "chat-1").is_none());
        assert!(!finish_chat_diff_request(
            &mut peeks,
            "chat-1".into(),
            first + 1,
            Ok(diff_summary()),
        ));
        assert_eq!(
            peeks.get("chat-1"),
            Some(&ChatDiffPeek::Loading { request_id: first })
        );

        assert!(finish_chat_diff_request(
            &mut peeks,
            "chat-1".into(),
            first,
            Ok(diff_summary()),
        ));
        assert!(matches!(peeks.get("chat-1"), Some(ChatDiffPeek::Loaded(_))));

        let refresh = begin_chat_diff_request(&mut peeks, &mut sequence, "chat-1").unwrap();
        assert_ne!(refresh, first);
        assert_eq!(
            peeks.get("chat-1"),
            Some(&ChatDiffPeek::Loading {
                request_id: refresh,
            })
        );
    }

    #[test]
    fn diff_peek_summary_discards_the_patch_payload() {
        let summary = ChatDiffSummary::from(zeron_proto::CheckoutDiff {
            checkout_id: "checkout-1".into(),
            device_id: "device-1".into(),
            cwd: "/repo".into(),
            patch: "large patch body".repeat(1024),
            files: diff_summary().files,
            submodules: Vec::new(),
            additions: 4,
            deletions: 2,
            truncated: true,
            checksum: "checksum".into(),
            updated_at: chrono::Utc::now(),
        });

        assert_eq!(summary.files.len(), 1);
        assert_eq!(summary.additions, 4);
        assert_eq!(summary.deletions, 2);
        assert!(summary.truncated);
    }

    #[test]
    fn topology_workspace_selection_is_seeded_once_and_ignores_global_changes() {
        let tabs = workspace_tabs();
        let mut selection = WorkspaceTabSelection::new(&tabs, Some("space-beta"), true);
        assert_eq!(selection.selected_id.as_deref(), Some("space-beta"));

        // Opening a chat in Alpha changes AppState's selection, but the map's
        // route-local choice remains Beta.
        assert!(!selection.reconcile(&tabs, true));
        assert_eq!(selection.selected_id.as_deref(), Some("space-beta"));

        // If Beta is deleted, recovery is deterministic display order and
        // still does not inherit the new global selection.
        assert!(selection.reconcile(&tabs[..1], true));
        assert_eq!(selection.selected_id.as_deref(), Some("space-alpha"));
    }

    #[test]
    fn switching_workspace_tab_targets_its_space_and_owning_device() {
        let tabs = workspace_tabs();
        let mut selection = WorkspaceTabSelection::new(&tabs, Some("space-alpha"), true);
        assert!(selection.select(&tabs, "space-beta"));

        let selected = selection.selected(&tabs).unwrap();
        assert_eq!(
            selected.request_payload(),
            serde_json::json!({
                "spaceId": "space-beta",
                "targetDeviceId": "device-b",
            })
        );
    }

    #[test]
    fn selected_tab_supplies_header_name_and_path() {
        let tabs = workspace_tabs();
        let selection = WorkspaceTabSelection::new(&tabs, Some("space-beta"), true);
        let selected = selection.selected(&tabs).unwrap();

        assert_eq!(selection.selected_id.as_deref(), Some("space-beta"));
        assert_eq!(selected.display_name, "Beta");
        assert_eq!(selected.path, "/work/beta");
    }
}
