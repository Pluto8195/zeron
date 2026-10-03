//! Read-only repository/worktree/chat topology for the workspace canvas.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::HarnessId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RepositoryTopologyKind {
    Workspace,
    Repository,
    Submodule,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RepositoryTopologyStatus {
    Clean,
    Modified,
    Conflicted,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RepositoryTopologyActivity {
    Working,
    AwaitingInput,
    Completed,
    Idle,
    Error,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryTopology {
    pub workspace: RepositoryTopologyWorkspace,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryTopologyWorkspace {
    pub id: String,
    pub name: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub status: RepositoryTopologyStatus,
    pub repositories: Vec<RepositoryTopologyRepository>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryTopologyRepository {
    pub id: String,
    pub name: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relative_path: Option<String>,
    pub kind: RepositoryTopologyKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    pub status: RepositoryTopologyStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ahead: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behind: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_repository_id: Option<String>,
    pub worktrees: Vec<RepositoryTopologyWorktree>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryTopologyWorktree {
    pub id: String,
    pub name: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relative_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    pub status: RepositoryTopologyStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ahead: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behind: Option<usize>,
    pub is_main: bool,
    #[serde(default)]
    pub locked: bool,
    #[serde(default)]
    pub prunable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout_id: Option<String>,
    pub chats: Vec<RepositoryTopologyChat>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryTopologyChat {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
    pub indicator: RepositoryTopologyActivity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_branch: Option<String>,
    #[serde(default)]
    pub branch_mismatch: bool,
    #[serde(default)]
    pub archived: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_chat_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linked_ticket_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub linked_pr_urls: Vec<String>,
    pub agents: Vec<RepositoryTopologyAgent>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryTopologyAgent {
    pub id: String,
    pub name: String,
    pub status: RepositoryTopologyActivity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<HarnessId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topology_wire_names_match_canvas_adapter() {
        let value = serde_json::to_value(RepositoryTopologyStatus::Conflicted).unwrap();
        assert_eq!(value, serde_json::json!("conflicted"));
        let value = serde_json::to_value(RepositoryTopologyActivity::AwaitingInput).unwrap();
        assert_eq!(value, serde_json::json!("awaitingInput"));
        let value = serde_json::to_value(RepositoryTopologyKind::Submodule).unwrap();
        assert_eq!(value, serde_json::json!("submodule"));
    }
}
