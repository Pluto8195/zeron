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

use crate::shell::Shell;
use crate::state::AppState;
use crate::theme::Theme;

const COLUMN_GAP: f32 = 88.0;
const ROW_GAP: f32 = 24.0;
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
            Self::Modified => "modified",
            Self::Conflicted => "conflicted",
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

        let mut placements = HashMap::<String, (usize, f32)>::new();
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
                );
            }
        }

        let max_width = seeds
            .iter()
            .map(|seed| node_size(seed.kind).0)
            .fold(0.0, f32::max);
        let column_step = max_width + COLUMN_GAP;
        let mut nodes = Vec::with_capacity(seeds.len());
        let mut max_x = 0.0_f32;
        let mut max_y = 0.0_f32;
        for seed in seeds {
            let Some((depth, center_y)) = placements.get(&seed.id).copied() else {
                continue;
            };
            let (width, height) = node_size(seed.kind);
            let x = GRAPH_PAD + depth as f32 * column_step;
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
        placements: &mut HashMap<String, (usize, f32)>,
        cursor_y: &mut f32,
        visiting: &mut HashSet<String>,
    ) -> f32 {
        let seed = &seeds[index];
        if let Some((_, center)) = placements.get(&seed.id) {
            return *center;
        }
        if !visiting.insert(seed.id.clone()) {
            let (_, height) = node_size(seed.kind);
            let center = *cursor_y + height / 2.0;
            *cursor_y += height + ROW_GAP;
            return center;
        }
        let child_centers = children
            .get(&seed.id)
            .into_iter()
            .flatten()
            .map(|child| {
                Self::place_node(
                    *child,
                    depth + 1,
                    seeds,
                    children,
                    placements,
                    cursor_y,
                    visiting,
                )
            })
            .collect::<Vec<_>>();
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
        placements.insert(seed.id.clone(), (depth, center));
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

fn node_size(kind: GraphNodeKind) -> (f32, f32) {
    match kind {
        GraphNodeKind::Workspace => (220.0, 88.0),
        GraphNodeKind::Repository(_) => (222.0, 96.0),
        GraphNodeKind::Worktree => (304.0, 164.0),
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
    Failed(String),
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

pub struct RepositoryTopology {
    state: Entity<AppState>,
    shell: gpui::WeakEntity<Shell>,
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
    _state_subscription: gpui::Subscription,
    _refresh_task: gpui::Task<()>,
}

impl RepositoryTopology {
    pub fn new(
        state: Entity<AppState>,
        shell: gpui::WeakEntity<Shell>,
        cx: &mut Context<Self>,
    ) -> Self {
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
            _state_subscription: subscription,
            _refresh_task: refresh_task,
        }
    }

    fn refresh_tick(&mut self, cx: &mut Context<Self>) {
        let visible = self
            .shell
            .upgrade()
            .is_some_and(|shell| shell.read(cx).is_repository_topology_route());
        if visible && !matches!(&self.load, LoadState::Loading) {
            let tabs = self.reconcile_workspace_tabs(cx);
            self.requested_space_id = None;
            self.ensure_loaded(&tabs, cx);
        }
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
        self.load = LoadState::Loading;
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
                this.load = match result {
                    Ok(snapshot) => LoadState::Loaded(snapshot),
                    Err(error) => LoadState::Failed(error),
                };
                this.auto_fit = true;
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
        let x = node.x * self.zoom + self.pan.0;
        let y = node.y * self.zoom + self.pan.1;
        let width = node.width * self.zoom;
        let height = node.height * self.zoom;
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
        let scale = self.zoom.clamp(0.65, 1.0);
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
            .left(px(x))
            .top(px(y))
            .w(px(width))
            .h(px(height))
            .p(px(if node.kind == GraphNodeKind::Worktree {
                13.0
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
                    this.selected = Some(selection.clone());
                    cx.notify();
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
                .gap(px(7.0 * scale))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(px(8.0 * scale))
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_size(crate::typography::ui_rems(14.5 * scale))
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
                        .gap(px(5.0 * scale))
                        .text_color(theme.text_muted)
                        .child(crate::icons::icon(crate::icons::FILE_TREE).size(px(11.0 * scale)))
                        .child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_size(crate::typography::ui_rems(10.5 * scale))
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
                            .text_size(crate::typography::ui_rems(9.5 * scale))
                            .text_color(theme.text_faint)
                            .child(SharedString::from(path)),
                    )
                })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0 * scale))
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
                        .h(px(28.0 * scale))
                        .px(px(8.0 * scale))
                        .flex()
                        .items_center()
                        .gap(px(6.0 * scale))
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
                                .size(px(11.0 * scale)),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_size(crate::typography::ui_rems(10.0 * scale))
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
                let chats = worktree
                    .chats
                    .iter()
                    .enumerate()
                    .map(|(index, chat)| {
                        let chat_id = chat.id.clone();
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
                        div()
                            .id(("topology-inspector-chat", index))
                            .p(px(9.0))
                            .rounded(px(7.0))
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.surface_card)
                            .child(
                                div()
                                    .id(("topology-inspector-chat-title", index))
                                    .cursor_pointer()
                                    .hover(|element| element.text_color(theme.busy))
                                    .text_size(crate::typography::ui_rems(10.5))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(SharedString::from(chat.title.clone()))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if let Some(shell) = this.shell.upgrade() {
                                            shell.update(cx, |shell, cx| {
                                                shell.open_chat(chat_id.clone(), cx)
                                            });
                                        }
                                    })),
                            )
                            .child(
                                div()
                                    .mt(px(3.0))
                                    .text_size(crate::typography::ui_rems(9.0))
                                    .text_color(theme.text_muted)
                                    .child(SharedString::from(meta.join(" · "))),
                            )
                            .child(
                                div()
                                    .mt(px(3.0))
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
                            .when_some(chat.source_branch.clone(), |element, source_branch| {
                                element.child(
                                    div()
                                        .mt(px(3.0))
                                        .text_size(crate::typography::ui_rems(8.5))
                                        .text_color(theme.text_faint)
                                        .child(SharedString::from(format!(
                                            "Started on {source_branch}"
                                        ))),
                                )
                            })
                            .children(chat.linked_pr_urls.iter().map(|url| {
                                div()
                                    .mt(px(3.0))
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .text_size(crate::typography::ui_rems(8.5))
                                    .text_color(theme.text_faint)
                                    .child(SharedString::from(url.clone()))
                            }))
                            .children(agent_rows)
                    })
                    .collect::<Vec<_>>();
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
                    let start = (
                        (parent.x + parent.width) * zoom + pan.0,
                        (parent.y + parent.height / 2.0) * zoom + pan.1,
                    );
                    let end = (
                        child.x * zoom + pan.0,
                        (child.y + child.height / 2.0) * zoom + pan.1,
                    );
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
            let x = ((parent.x + parent.width + child.x) / 2.0) * zoom + pan.0;
            let y = (child.y + child.height / 2.0) * zoom + pan.1;
            Some(
                div()
                    .absolute()
                    .left(px(x - 22.0))
                    .top(px(y - 9.0))
                    .px(px(4.0))
                    .py(px(1.0))
                    .rounded(px(3.0))
                    .bg(theme.surface)
                    .text_size(crate::typography::ui_rems(8.5 * zoom.clamp(0.7, 1.0)))
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
                            .id("topology-refresh")
                            .size(px(30.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(6.0))
                            .cursor_pointer()
                            .text_color(theme.text_muted)
                            .hover(|element| element.bg(theme.element_hover).text_color(theme.text))
                            .child(crate::icons::icon(crate::icons::REFRESH).size(px(14.0)))
                            .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
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
            LoadState::Loaded(snapshot) if snapshot.repositories.is_empty() => self
                .render_center_state(
                    &theme,
                    "No Git repositories found",
                    "This workspace does not contain a detected repository or worktree yet.",
                    Some("Refresh"),
                    cx,
                ),
            LoadState::Loaded(snapshot) => self.render_graph(&snapshot, &theme, cx),
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
