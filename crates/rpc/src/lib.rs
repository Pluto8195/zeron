//! zeron-rpc — the typed control plane (UiRpc / ControlRpc) over WebSocket + in-memory
//! transports, plus the device-room relay transport ({s,k,to,from} frames — [`device_room`]).
//!
//! Framing: ndjson envelopes, one JSON object per WebSocket text message (or per line on
//! byte transports), matching the shape of zeron's Effect RPC without the Effect runtime:
//!
//! - client → server: `{id, method, params}` to invoke, `{id, cancel: true}` to stop a stream;
//! - server → client: `{id, ok}` / `{id, err}` for unary calls,
//!   `{id, item}`* then `{id, done: true}` (or `{id, err}`) for streams.
//!
//! The server dispatches into an [`RpcService`]; the [`RpcClient`] offers `call` and
//! `subscribe`. Both ends run over any pair of string channels, so the in-memory transport
//! ([`memory_client`]) exercises the exact same code path as the WebSocket one.

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

mod client;
pub mod device_room;
mod server;

pub use client::{RpcClient, RpcSubscription, connect_ws};
pub use device_room::{
    DeviceFrameHeader, DeviceLink, HostRelay, HostRelayConfig, LinkCache, LinkCacheConfig,
    NudgeHandler, PeerLiveness, PeerLivenessProbe, StaticToken, TokenError, TokenSource,
    decode_device_frame, device_room_ws_url, encode_device_frame,
};
pub use server::{serve_connection, serve_ws_listener};

/// RPC method names — single source of truth for both ends.
/// Full surface: docs/research/feature-inventory.md §2.
pub mod methods {
    pub const WATCH_PREVIEWS: &str = "WatchPreviews";
    pub const LIST_HARNESSES: &str = "ListHarnesses";
    pub const INSTALL_HARNESS: &str = "InstallHarness";
    /// Flip a harness's enablement on the target device (Settings → Agents);
    /// replies with the device's fresh `ListHarnesses` catalog.
    pub const GET_TITLE_SETTINGS: &str = "GetTitleSettings";
    pub const SET_TITLE_SETTINGS: &str = "SetTitleSettings";
    pub const SET_HARNESS_ENABLED: &str = "SetHarnessEnabled";
    pub const LIST_MODELS: &str = "ListModels";
    pub const LIST_COMMANDS: &str = "ListCommands";
    pub const QUEUE_COMMAND: &str = "QueueCommand";
    pub const TAKE_PROJECT_ACTION_SETUP: &str = "TakeProjectActionSetup";
    /// Peer-to-peer delivery fallback: the SENDER's engine forwards a queued
    /// command entry (client-minted id and all) straight over the device-room
    /// link when its chat2 rows can't reach the edge but the host's peer link
    /// is alive. The host claims the id in its processed ledger before
    /// executing, so the doc row arriving later dedupes to a no-op —
    /// exactly-once by construction. Params `{chatId, entry}`.
    pub const RELAY_COMMAND: &str = "RelayCommand";
    /// User-driven delivery retry for a chat with unadopted queued sends:
    /// fresh chat2 socket, host nudge, drain pass, and a new delivery escort
    /// per pending command. Params `{chatId}`; IPC-only.
    pub const RETRY_DELIVERY: &str = "RetryDelivery";
    pub const WATCH_DOC_MESSAGES: &str = "WatchDocMessages";
    /// Messages typed while the agent was busy, held on the chat doc so every
    /// device sees the same queue. `{ chatId }` → `{ items: QueuedMessage[] }`.
    pub const WATCH_QUEUE: &str = "WatchQueue";
    /// Append to the queue. `{ chatId, text, attachments?, holdForTurnEnd? }` → `{ id }`.
    pub const QUEUE_MESSAGE: &str = "QueueMessage";
    /// Retype a queued message; empty text deletes it.
    /// `{ chatId, id, text }` → `{ changed }`.
    pub const UPDATE_QUEUED_MESSAGE: &str = "UpdateQueuedMessage";
    /// Acquire a host-authoritative edit lease for one queued row.
    pub const BEGIN_QUEUED_MESSAGE_EDIT: &str = "BeginQueuedMessageEdit";
    /// Renew an acquired queue edit lease.
    pub const RENEW_QUEUED_MESSAGE_EDIT: &str = "RenewQueuedMessageEdit";
    /// Commit, cancel, discard, or explicitly release an acquired edit.
    pub const FINISH_QUEUED_MESSAGE_EDIT: &str = "FinishQueuedMessageEdit";
    /// Reorder. `{ chatId, id, toIndex }` → `{ changed }`.
    pub const MOVE_QUEUED_MESSAGE: &str = "MoveQueuedMessage";
    pub const REMOVE_QUEUED_MESSAGE: &str = "RemoveQueuedMessage";
    /// Interrupt whatever is running and send this one. `{ chatId, id }` → `{ sent }`.
    pub const SEND_QUEUED_MESSAGE_NOW: &str = "SendQueuedMessageNow";
    /// Steer this row into the live turn without interrupting it.
    /// `{ chatId, id }` → `{ sent }`.
    pub const STEER_QUEUED_MESSAGE_NOW: &str = "SteerQueuedMessageNow";
    /// Nudge every open room client to verify liveness NOW (window focus,
    /// app foregrounded). No params; IPC-only. Each room ignores the hint
    /// unless it has been broadcast-quiet ≥30s, so this is cheap to spam.
    pub const PROBE_SYNC: &str = "ProbeSync";
    /// Live sync introspection (`zeron sync` / debug surfaces): per-room
    /// connection state, last pushed-frame/ack ages, rejoin/probe/resync
    /// counters for the workspace room and every open chat doc. No params;
    /// IPC-only.
    pub const SYNC_STATUS: &str = "SyncStatus";
    /// Pushed edge-connectivity posture (`zeron_proto::Connectivity`):
    /// current value first, then every change — the connection pill /
    /// composer-honesty / queued-badge feed. No params; IPC-only.
    pub const WATCH_CONNECTIVITY: &str = "WatchConnectivity";
    /// In-flight queued-attachment transfers (`zeron_proto::TransferProgress`
    /// list): current set first, then a fresh snapshot per landed chunk —
    /// the sending thumbnail's percent-ring feed. No params; IPC-only.
    pub const WATCH_TRANSFERS: &str = "WatchTransfers";
    pub const WATCH_CHATS: &str = "WatchChats";
    pub const WATCH_SIDEBAR_PREFERENCES: &str = "WatchSidebarPreferences";
    pub const WATCH_DEVICES: &str = "WatchDevices";
    pub const WATCH_SESSIONS: &str = "WatchSessions";
    /// Spaces registry (device+folder pairs) from the workspace doc.
    pub const WATCH_SPACES: &str = "WatchSpaces";
    /// Entity mutations against the workspace doc (feature-inventory §2 DataRpc).
    /// Params are tagged `{op: createChat|createSpace|renameSpace|deleteSpace|
    /// renameChat|setChatArchived|deleteChat|renameDevice|markChatSeen, …}`.
    pub const MUTATE: &str = "Mutate";
    /// This engine's identity → `{deviceId}` (IPC-only; never relay-forwarded —
    /// the answer is about whichever engine you are directly connected to).
    pub const LOCAL_DEVICE: &str = "LocalDevice";
    /// This engine runtime's fixed device and workspace identity.
    pub const ENGINE_INFO: &str = "EngineInfo";
    /// Readiness barrier for the engine runtime. The call completes once stores
    /// and journals are assembled, or fails with the assembly error.
    pub const ENGINE_READY: &str = "EngineReady";
    /// Ask a headless IPC owner to drain its runtime and exit successfully.
    /// Headed IPC owners do not implement this method: closing another app's
    /// engine behind its windows would leave that process unusable.
    pub const STOP_ENGINE: &str = "StopEngine";
    pub const AUTH_STATUS: &str = "AuthStatus";
    // AuthRpc mutations (feature-inventory §2 AuthRpc; IPC-only).
    pub const SIGN_IN: &str = "SignIn";
    pub const SIGN_IN_HEADLESS: &str = "SignInHeadless";
    pub const COMPLETE_SIGN_IN: &str = "CompleteSignIn";
    pub const SIGN_OUT: &str = "SignOut";
    pub const LIST_ORGS: &str = "ListOrgs";
    pub const CREATE_ORG: &str = "CreateOrg";
    pub const SELECT_ORG: &str = "SelectOrg";
    /// One-time local→synced profile import: what's importable (unary).
    pub const LOCAL_IMPORT_STATUS: &str = "LocalImportStatus";
    /// One-time local→synced profile import: run it (stream of progress items).
    pub const IMPORT_LOCAL_WORKSPACE: &str = "ImportLocalWorkspace";
    /// Claude Code sessions found on disk (`~/.claude/projects`) that this
    /// workspace hasn't already imported, newest first. No params; IPC-only
    /// (device-local filesystem, never relay-forwarded — forwarding would
    /// scan the wrong machine's `~/.claude`).
    pub const SCAN_EXTERNAL_SESSIONS: &str = "ScanExternalSessions";
    /// Import one scanned candidate into a brand-new chat. `{chatId,
    /// externalSessionId, path}` → `ImportedSession`. IPC-only, same reason.
    pub const IMPORT_EXTERNAL_SESSION: &str = "ImportExternalSession";
    /// Whether a scanned candidate looks like it might still be actively
    /// running elsewhere. `{path, sessionId, cwd}` →
    /// `{recentlyModified, liveProcessMatch}`. IPC-only: reads this
    /// machine's processes and files, never relay-forwarded.
    pub const CHECK_SESSION_LIVENESS: &str = "CheckSessionLiveness";
    /// Catch an already-imported chat up on new messages its source session
    /// accumulated since the last import/sync. `{chatId}` → `SyncResult`.
    /// IPC-only, same reason as the other external-import methods.
    pub const SYNC_EXTERNAL_SESSION: &str = "SyncExternalSession";
    /// Cached PR/ticket/worktree status for one chat's branch+cwd (ticket 002
    /// phases 2 and 4). `{chatId}` → `ChatLinkStatus { pr, ticket,
    /// isWorktree, diffStat, prSource, ticketSource }`. `isWorktree` is a
    /// pure inline check (no cache); `pr`/`ticket`/`diffStat` are pure cache
    /// reads — never block on a live `gh`/`linear`/`git` call; a miss is
    /// registered for the background sweep and reads back populated on a
    /// later call. `prSource`/`ticketSource` are `"manual" | "created_in_chat"
    /// | "mentioned" | null` — the provenance of a chat-row-DURABLE link
    /// (ticket 0xx's `SET_CHAT_LINK`/mining tap), `null` meaning the shown
    /// PR/ticket instead came from the branch/title inference this method
    /// always fell back to before durable links existed. IPC-only: reads
    /// this machine's `gh`/`linear` CLI state and local git worktrees, never
    /// relay-forwarded.
    pub const CHAT_LINK_STATUS: &str = "ChatLinkStatus";
    /// Durable PR/ticket link write (ticket 0xx) — layered over
    /// `CHAT_LINK_STATUS`'s branch/title inference: `{chatId, kind: "pr" |
    /// "ticket", value: string | null}` → the chat's updated link fields
    /// (`{linkedPrUrl, linkedPrSource, linkedTicketId, linkedTicketSource}`).
    /// Always writes source `"manual"` (the only writer allowed to CLEAR a
    /// slot — `value: null` unlinks); a mined write (`created_in_chat`/
    /// `mentioned`, from the live per-event tap or the transcript backfill
    /// repair) never rides this RPC, it calls `WorkspaceHost::set_chat_link`
    /// directly. IPC-only, same reason as `CHAT_LINK_STATUS`.
    pub const SET_CHAT_LINK: &str = "SetChatLink";
    /// Subagents a chat's source Claude Code session spawned, on disk
    /// (ticket 002 phase 3, overview wiring). `{chatId}` → `SubagentSummary[]`
    /// (empty for a chat with no resolvable transcript or no subagents).
    /// Resolves the chat's transcript path server-side — via its
    /// external-import cursor if it has one, else its recorded harness
    /// session id (covers a chat Zeron itself launched/resumed, never
    /// imported; same two-source resolution `CHAT_CONTEXT_USAGE` uses,
    /// `ContextUsageProvider::transcript_path_for_chat`) — and delegates to
    /// `crate::subagent_scan::scan_subagents`: a `chatId`-keyed convenience
    /// so the UI never needs to know the on-disk path convention. IPC-only,
    /// same reason as its sibling methods.
    pub const SCAN_CHAT_SUBAGENTS: &str = "ScanChatSubagents";
    /// Full turn history for one subagent's own on-disk transcript (session
    /// canvas web parity: `session_canvas_server.py`'s `build_transcript_turns`,
    /// the side-panel chat view's data source there). `{chatId, agentId}` →
    /// `{turns: [{role, text, timestamp, tools: [{name, inputPreview,
    /// resultPreview}]}], model}`. Re-resolves the PARENT chat's transcript
    /// path server-side (same two-source resolution `SCAN_CHAT_SUBAGENTS`
    /// uses) and derives the subagent file from it — a client-supplied path
    /// is never trusted. An unknown `chatId`/`agentId` replies with empty
    /// turns, not an error, matching `SCAN_CHAT_SUBAGENTS`'s own
    /// empty-not-error stance. IPC-only, same reason as its sibling
    /// external-import/subagent methods (device-local filesystem reads).
    pub const READ_SUBAGENT_TRANSCRIPT: &str = "ReadSubagentTranscript";
    /// Every open PR the signed-in `gh` account authored, across every repo
    /// it can see (ticket 002 phase 5, "My PRs" pane). No params →
    /// `MyPrItem[]`. Pure cache read — populated by its own background sweep
    /// (bulk list search + one throttled per-PR detail fetch per tick), never
    /// blocks on a live call. IPC-only, same reason as its sibling methods.
    pub const MY_OPEN_PRS: &str = "MyOpenPrs";
    /// One-time migration: import every remaining Claude Code session on disk
    /// (same underlying scan `ScanExternalSessions` uses), carrying over
    /// archive status from the `session_canvas` stopgap tool this ticket
    /// effort is replacing. No params; streams `BulkImportEvent` items
    /// (`Start`/`Item`-per-candidate/`Summary`), same shape as
    /// `ImportLocalWorkspace`'s progress stream. Idempotent — safe to
    /// re-run, only processes what `ScanExternalSessions` still finds.
    /// IPC-only, same reason as its sibling external-import methods.
    pub const BULK_IMPORT_SESSION_CANVAS_SESSIONS: &str = "BulkImportSessionCanvasSessions";
    /// An already-imported chat's recorded task category / launch origin
    /// (ticket 002 overview grouping/color-coding). `{chatId}` →
    /// `{category, origin}` (each `null` for a chat with no import cursor,
    /// i.e. never went through the external-import flow). Pure read of what
    /// `ImportExternalSession` already computed and persisted at import
    /// time — never re-parses the transcript. IPC-only, same reason as its
    /// sibling external-import methods.
    pub const CHAT_CLASSIFICATION: &str = "ChatClassification";
    /// Context-window occupancy for the overview's tile sizing (session-
    /// canvas parity): `{chatId}` → `{contextPct: number | null}`, `0.0..=1.0`
    /// (the UI maps this to pixels). A separate lazy-per-chat method rather
    /// than folding onto `ChatLinkStatus`/`ChatClassification` — this value
    /// changes far more often (live while a chat is actively running) than
    /// either of those, so it wants its own polling cadence rather than
    /// riding piggyback on a cache tuned for GH/Linear TTLs or a one-time
    /// import-time classification. IPC-only: the live source is this
    /// engine's own doc store, and the fallback reads this machine's
    /// `~/.claude/projects` transcripts — neither makes sense forwarded to
    /// another device.
    pub const CHAT_CONTEXT_USAGE: &str = "ChatContextUsage";
    // Repos / worktrees / folders (ControlRpc, relay-forwardable).
    pub const LIST_REPOS: &str = "ListRepos";
    pub const ADD_REPO: &str = "AddRepo";
    pub const CLONE_REPO: &str = "CloneRepo";
    pub const CREATE_REPO: &str = "CreateRepo";
    pub const LIST_BRANCHES: &str = "ListBranches";
    pub const LIST_REFS: &str = "ListRefs";
    pub const LIST_GIT_HISTORY: &str = "ListGitHistory";
    /// Fuzzy commit-subject / SHA search over the complete public history.
    pub const SEARCH_GIT_HISTORY: &str = "SearchGitHistory";
    /// Resolve hosted profile images for a page of Git commit authors.
    pub const RESOLVE_GIT_AVATARS: &str = "ResolveGitAvatars";
    /// Update remote-tracking refs without changing HEAD, the index, or files.
    pub const FETCH_ALL: &str = "FetchAll";
    pub const SWITCH_REF: &str = "SwitchRef";
    pub const LIST_FOLDERS: &str = "ListFolders";
    /// The device's browse roots: home plus mounted drives/volumes.
    pub const LIST_DRIVES: &str = "ListDrives";
    /// Fuzzy relative-path search rooted in a known chat or space checkout.
    pub const SEARCH_FILES: &str = "SearchFiles";
    // Device-local workspace filesystem operations. All are relay-forwardable;
    // WatchWorkspaceFiles is the only streaming method in this group.
    pub const LIST_WORKSPACE_DIRECTORY: &str = "ListWorkspaceDirectory";
    pub const SEARCH_WORKSPACE_FILES: &str = "SearchWorkspaceFiles";
    pub const READ_WORKSPACE_IMAGE: &str = "ReadWorkspaceImage";
    pub const READ_WORKSPACE_FILE: &str = "ReadWorkspaceFile";
    pub const WRITE_WORKSPACE_FILE: &str = "WriteWorkspaceFile";
    pub const WATCH_WORKSPACE_FILES: &str = "WatchWorkspaceFiles";
    pub const CREATE_WORKTREE: &str = "CreateWorktree";
    pub const DELETE_WORKTREE: &str = "DeleteWorktree";
    /// Smart-worktree judgment for a new chat's first message: `{message,
    /// cwd}` → `{needsWorktree, probability, source}`. A TypeSafe/Jev `Noul`
    /// call (`needsWorktree = probability >= 0.5`) with a text-heuristic
    /// fallback when TypeSafe is unavailable/errors/times out (`source`
    /// reports which one answered: `"jev" | "heuristic"`). Pure judgment, no
    /// side effects, nothing cached (one call per new chat). IPC-only: the
    /// call itself is device-agnostic, but it's paired 1:1 with
    /// `CreateChatWorktree`'s device-local filesystem work, so it stays in
    /// the same non-forwarded group rather than round-tripping the relay for
    /// no benefit.
    pub const PLAN_CHAT_WORKSPACE: &str = "PlanChatWorkspace";
    /// Materialize an isolated git worktree for a new chat, using the
    /// `agent-mode-tools/agent-mode.sh` on-disk convention (NOT `CreateWorktree`'s
    /// own `~/.zeron/worktrees` layout — see `chat_workspace_plan` module docs):
    /// `{chatId, repoPath, name}` → `{worktreePath, branch}`, or a structured
    /// error (nested-under-a-submodule, path-already-exists, not-a-git-repo,
    /// etc). Also stamps the chat row's `cwd` to the new worktree path
    /// (`WorkspaceHost::set_chat_cwd`, the same durable field `SetChatCwd`/
    /// `Mutate` writes) so the chat's next dispatch runs there automatically.
    /// IPC-only: device-local filesystem + git state.
    pub const CREATE_CHAT_WORKTREE: &str = "CreateChatWorktree";
    /// Read-only close-out inspection of a chat's agent worktree: `{chatId,
    /// cwd}` → `{isWorktree, worktreePath, branch, chatLive, dirty,
    /// dirtyFiles, unmergedCommits, defaultBranch}`. `isWorktree` requires a
    /// `.workspace-root` file AND a registered linked git worktree (never a
    /// repo's main working tree); when false the other fields are
    /// empty/null/0. `chatLive` reuses the engine's session + process
    /// liveness signals. IPC-only: device-local filesystem + git state.
    pub const PLAN_CHAT_CLOSEOUT: &str = "PlanChatCloseout";
    /// Destructive counterpart to `PlanChatCloseout`: `{chatId, cwd, force}`
    /// → `{removed, branchDeleted, archived}`. Hard refusals (live chat, not
    /// a closeable worktree, default branch, main working tree) error even
    /// with `force`; soft ones (uncommitted files, unmerged commits) error
    /// unless `force`. On success removes the worktree, deletes its own
    /// branch (`-d`, `-D` when forced) and archives the chat
    /// (`WorkspaceHost::set_chat_archived`, the `setChatArchived` mutation).
    /// Errors say why. IPC-only: device-local filesystem + git state.
    pub const CLOSE_CHAT_WORKTREE: &str = "CloseChatWorktree";
    // Project Actions are private state on the device that owns the project.
    pub const LIST_PROJECT_ACTIONS: &str = "ListProjectActions";
    pub const UPSERT_PROJECT_ACTION: &str = "UpsertProjectAction";
    pub const DELETE_PROJECT_ACTION: &str = "DeleteProjectAction";
    pub const RUN_PROJECT_ACTION: &str = "RunProjectAction";
    // Terminals (ControlRpc, relay-forwardable; SubscribeTerminal streams).
    pub const OPEN_TERMINAL: &str = "OpenTerminal";
    pub const SUBSCRIBE_TERMINAL: &str = "SubscribeTerminal";
    pub const WRITE_TERMINAL: &str = "WriteTerminal";
    pub const RESIZE_TERMINAL: &str = "ResizeTerminal";
    pub const CLOSE_TERMINAL: &str = "CloseTerminal";
    /// Checkout-diff stream for the target device's chats (DataRpc,
    /// relay-forwardable — diffs are produced where the checkout lives).
    pub const WATCH_CHECKOUT_DIFFS: &str = "WatchCheckoutDiffs";
    pub const WATCH_WORKSPACE_GIT_STATUS: &str = "WatchWorkspaceGitStatus";
    /// Current pull request for one checkout, resolved on the checkout's host device.
    pub const WATCH_CHECKOUT_CHANGE_REQUEST: &str = "WatchCheckoutChangeRequest";
    pub const GET_CHECKOUT_DIFF: &str = "GetCheckoutDiff";
    pub const GET_CHECKOUT_FILE_DIFF_TEXT: &str = "GetCheckoutFileDiffText";
    // Agent accounts (ControlRpc, relay-forwardable — CLI logins are per-device).
    pub const LIST_AGENT_ACCOUNTS: &str = "ListAgentAccounts";
    pub const ACTIVATE_AGENT_ACCOUNT: &str = "ActivateAgentAccount";
    pub const FORGET_AGENT_ACCOUNT: &str = "ForgetAgentAccount";
    pub const START_AGENT_LOGIN: &str = "StartAgentLogin";
    pub const COMPLETE_AGENT_LOGIN: &str = "CompleteAgentLogin";
    pub const POLL_AGENT_LOGIN: &str = "PollAgentLogin";
    pub const CANCEL_AGENT_LOGIN: &str = "CancelAgentLogin";
    // Uploads / attachments (ControlRpc, relay-forwardable — target the chat's host device).
    pub const UPLOAD_CHUNK: &str = "UploadChunk";
    pub const UPLOAD_COMMIT: &str = "UploadCommit";
    pub const READ_ATTACHMENT_CHUNK: &str = "ReadAttachmentChunk";
    /// Lazy full-tool-output fetch from the R2 sidecar by doc-resident ref
    /// (chat2-sync A3). Edge-direct from any device — never relay-forwarded.
    pub const FETCH_TOOL_BLOB: &str = "FetchToolBlob";
    // Updates (ControlRpc, relay-forwardable — a device reports/applies its own
    // binary's update). Stream: current UpdateStatus, then every change.
    pub const UPDATE_STATUS: &str = "UpdateStatus";
    /// Download + apply the newest release on the target device (symlink-managed
    /// installs; the service restart is scheduled after the reply flushes).
    pub const APPLY_UPDATE: &str = "ApplyUpdate";
}

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("unknown method: {0}")]
    UnknownMethod(String),
    #[error("bad params: {0}")]
    BadParams(String),
    #[error("{0}")]
    Failed(String),
    #[error("transport: {0}")]
    Transport(String),
    #[error("connection closed")]
    Closed,
}

/// A client-originated frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientFrame {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub params: serde_json::Value,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancel: bool,
}

/// A server-originated frame. Exactly one of `ok` / `err` / `item` / `done` is meaningful.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerFrame {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub err: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub done: bool,
}

/// What a service returns for one invocation.
pub enum RpcReply {
    /// Unary response — sent as `{id, ok}`.
    Value(serde_json::Value),
    /// Stream — each item sent as `{id, item}`, then `{id, done: true}` when it ends.
    Stream(BoxStream<'static, serde_json::Value>),
}

impl RpcReply {
    /// Serialize a value into a unary reply.
    pub fn value<T: Serialize>(value: &T) -> Result<Self, RpcError> {
        serde_json::to_value(value)
            .map(RpcReply::Value)
            .map_err(|e| RpcError::Failed(format!("serialize response: {e}")))
    }
}

/// Server-side dispatch: one implementation serves every transport.
#[async_trait]
pub trait RpcService: Send + Sync + 'static {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError>;
}

/// Deserialize typed params out of the envelope's `params` value.
pub fn parse_params<T: serde::de::DeserializeOwned>(
    params: serde_json::Value,
) -> Result<T, RpcError> {
    serde_json::from_value(params).map_err(|e| RpcError::BadParams(e.to_string()))
}

/// Spawn an in-memory server for `service` and return a connected client.
/// Same envelopes, same dispatch loop as the WebSocket path — the in-process UI
/// transport (ARCHITECTURE §1 "zero serialization shortcuts").
pub fn memory_client(service: Arc<dyn RpcService>) -> RpcClient {
    let (client_out, server_in) = tokio::sync::mpsc::channel::<String>(256);
    let (server_out, client_in) = tokio::sync::mpsc::channel::<String>(256);
    tokio::spawn(serve_connection(service, server_out, server_in));
    RpcClient::new(client_out, client_in)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::sync::Mutex;

    struct TestService;

    struct CancelAwareService {
        dropped: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    }

    struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            if let Some(dropped) = self.0.take() {
                let _ = dropped.send(());
            }
        }
    }

    #[async_trait]
    impl RpcService for CancelAwareService {
        async fn handle(
            &self,
            method: &str,
            _params: serde_json::Value,
        ) -> Result<RpcReply, RpcError> {
            if method != methods::WATCH_CHECKOUT_CHANGE_REQUEST {
                return Err(RpcError::UnknownMethod(method.into()));
            }
            let guard = DropSignal(self.dropped.lock().unwrap().take());
            let stream = futures::stream::unfold(guard, |guard| async move {
                let item = std::future::pending::<Option<(serde_json::Value, DropSignal)>>().await;
                drop(guard);
                item
            });
            Ok(RpcReply::Stream(stream.boxed()))
        }
    }

    #[async_trait]
    impl RpcService for TestService {
        async fn handle(
            &self,
            method: &str,
            params: serde_json::Value,
        ) -> Result<RpcReply, RpcError> {
            match method {
                "Echo" => Ok(RpcReply::Value(params)),
                "Count" => {
                    let n = params.get("n").and_then(|v| v.as_u64()).unwrap_or(0);
                    Ok(RpcReply::Stream(
                        futures::stream::iter((0..n).map(|i| serde_json::json!(i))).boxed(),
                    ))
                }
                "Never" => Ok(RpcReply::Stream(futures::stream::pending().boxed())),
                "Boom" => Err(RpcError::Failed("boom".into())),
                other => Err(RpcError::UnknownMethod(other.into())),
            }
        }
    }

    #[tokio::test]
    async fn memory_call_stream_and_error() {
        let client = memory_client(Arc::new(TestService));

        let echoed = client
            .call("Echo", serde_json::json!({"x": 1}))
            .await
            .unwrap();
        assert_eq!(echoed, serde_json::json!({"x": 1}));

        let mut items = client
            .subscribe("Count", serde_json::json!({"n": 3}))
            .await
            .unwrap();
        let mut seen = Vec::new();
        while let Some(v) = items.recv().await {
            seen.push(v);
        }
        assert_eq!(
            seen,
            vec![
                serde_json::json!(0),
                serde_json::json!(1),
                serde_json::json!(2)
            ]
        );

        let err = client
            .call("Boom", serde_json::Value::Null)
            .await
            .unwrap_err();
        assert!(matches!(err, RpcError::Failed(m) if m == "boom"));
    }

    #[tokio::test]
    async fn checked_stream_acknowledges_support_and_preserves_unknown_method() {
        let client = memory_client(Arc::new(TestService));

        let mut items = client
            .subscribe_checked("Count", serde_json::json!({"n": 1}))
            .await
            .unwrap();
        assert_eq!(items.recv().await, Some(serde_json::json!(0)));
        assert_eq!(items.recv().await, None);

        let error = match client
            .subscribe_checked("FutureStream", serde_json::Value::Null)
            .await
        {
            Ok(_) => panic!("old service must reject unknown stream"),
            Err(error) => error,
        };
        assert!(matches!(error, RpcError::UnknownMethod(method) if method == "FutureStream"));
    }

    #[tokio::test]
    async fn dropping_checked_subscription_cancels_pending_server_stream() {
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let client = memory_client(Arc::new(CancelAwareService {
            dropped: Mutex::new(Some(dropped_tx)),
        }));
        let stream = client
            .subscribe_checked(
                methods::WATCH_CHECKOUT_CHANGE_REQUEST,
                serde_json::Value::Null,
            )
            .await
            .unwrap();

        drop(stream);

        tokio::time::timeout(std::time::Duration::from_secs(1), dropped_rx)
            .await
            .expect("server stream cancelled")
            .expect("drop signal");
    }

    #[tokio::test]
    async fn websocket_round_trip() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(serve_ws_listener(listener, Arc::new(TestService)));

        let client = connect_ws(&format!("ws://127.0.0.1:{port}")).await.unwrap();
        let echoed = client
            .call("Echo", serde_json::json!("hello"))
            .await
            .unwrap();
        assert_eq!(echoed, serde_json::json!("hello"));

        let mut items = client
            .subscribe("Count", serde_json::json!({"n": 2}))
            .await
            .unwrap();
        assert_eq!(items.recv().await, Some(serde_json::json!(0)));
        assert_eq!(items.recv().await, Some(serde_json::json!(1)));
        assert_eq!(items.recv().await, None);
    }

    #[tokio::test]
    async fn handshake_with_origin_header_is_rejected() {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(serve_ws_listener(listener, Arc::new(TestService)));

        // A browser page opening ws://127.0.0.1:{port} always sends Origin;
        // the server must refuse the handshake before serving any RPC.
        let mut req = format!("ws://127.0.0.1:{port}")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("origin", "https://evil.example".parse().unwrap());
        let result = tokio_tungstenite::connect_async(req).await;
        assert!(
            result.is_err(),
            "handshake carrying an Origin header must be rejected"
        );

        // A native viewport (no Origin) still connects and can call RPC — the
        // reject must not be a blanket denial.
        let client = connect_ws(&format!("ws://127.0.0.1:{port}")).await.unwrap();
        let echoed = client.call("Echo", serde_json::json!("ok")).await.unwrap();
        assert_eq!(echoed, serde_json::json!("ok"));
    }

    #[tokio::test]
    async fn dropping_stream_receiver_cancels_server_side() {
        let client = memory_client(Arc::new(TestService));
        let items = client
            .subscribe("Never", serde_json::Value::Null)
            .await
            .unwrap();
        drop(items);
        // The next unary call still works — the dead stream didn't wedge the connection.
        let echoed = client.call("Echo", serde_json::json!(2)).await.unwrap();
        assert_eq!(echoed, serde_json::json!(2));
    }
}
