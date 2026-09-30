//! Multi-chat overview (ticket 002 phase 1 + spatial-canvas UX preview):
//! every current chat, at once, in one view — the thing
//! `session_canvas.html`/`session_canvas_server.py` (the `agent-mode-tools`
//! stopgap tool) already does, without leaving for the single-chat route.
//! "View full chat →" opens a side panel that embeds the Shell's OWN
//! `Transcript`/`Composer` entities (live transcript + working composer) by
//! selecting the chat without changing the route — see
//! [`Overview::chat_panel_open`]. The same right-side slot has a second,
//! read-only mode: clicking one of a chat's compact subagent tiles "peeks"
//! into that subagent's transcript (`READ_SUBAGENT_TRANSCRIPT`; no composer
//! — a subagent can't be continued) — see [`Overview::subagent_peek`].
//!
//! Alongside either mode, an optional left "My PRs" sidebar lists the
//! user's open PRs (toolbar toggle; see [`Overview::pr_sidebar_open`]),
//! filtered by the same search box.
//!
//! Two rendering modes over the SAME `OverviewRow` data:
//! - List: phase 1's original plain row list.
//! - Canvas: a spatial tile board (drag-to-reposition, pan, zoom), previewing
//!   the target UX from `session_canvas.html`'s `renderTile`/`canvasScroll`
//!   ahead of phases 2-5 (PR/ticket linkage, subagent nesting, archive,
//!   staleness) landing their data. Tile layout/drag/pan/zoom code never
//!   reads anything beyond `OverviewRow` — a later phase adds a field there
//!   and a small append to `tile_body`, not a rewrite of this file's spatial
//!   mechanics.
//!
//! Tile size follows context-window occupancy, like `session_canvas.html`'s
//! `tileSize`: each chat's `CHAT_CONTEXT_USAGE` fraction (fetched lazily per
//! visible row by `ensure_context_usage`) maps to a main-tile width of
//! 160..=260px (null → 160) and a MINIMUM height of 0.62 × width (see
//! [`tile_size_for_pct`]) — matching the reference, which treats 0.62×width
//! as a floor and lets the real card grow to fit its content. gpui tiles
//! need a size known up front (no "measure the real rendered card" pass like
//! the reference's own packer), so `OverviewRow::tile_size` takes the `max`
//! of that aspect floor and a deterministic content-height ESTIMATE
//! ([`tile_content_height_estimate`]) accounting for what varies per row: a
//! PR/ticket badge row, and the stack of compact subagent tiles — one per
//! subagent, uncapped, so a 36-subagent card really is ~1.3k px tall.
//! `tile_size` is the ONE place every consumer reads
//! a tile's size from — the flat grid, the grouped packer, both overlap-
//! repair passes, viewport culling and fit-to-matches all use it, so a new
//! content driver of height only ever needs adding inside that one function.
//! Nested subagent tiles follow their parent tile's width; they never size by
//! their own context. (Flat mode's fixed default-slot stride only budgets a
//! bounded number of them — see [`tile_max_size`].) An expanded tile's detail block is a separate,
//! deliberately NOT height-aware case — see `render_canvas_flat`/
//! `render_canvas_grouped`'s own comments on why (a real, scoped follow-up,
//! not touched here).
//!
//! Flat-canvas drag positions persist per device (`overview_positions`,
//! JSON under the engine data dir — the analog of the reference's
//! localStorage), as do the acknowledged-"done" set and the repo filter's
//! hidden set and the My PRs sidebar's open state (`UI_FLAGS_FILE`). View
//! mode, pan/zoom and the other toggles are in-memory only. Ticket 002
//! puts cross-device layout sync out of scope for v1.
//!
//! Freshness: the TTL-gated `ensure_*` fetches only get a chance to re-run
//! when something calls them, so a mounted, visible overview runs a light
//! [`IDLE_REFRESH`] tick (`refresh_tick`) that re-checks every visible row's
//! TTLs and repaints relative times without forcing a row rebuild or
//! grouped relayout unless a fetch actually changed something.

use std::cell::Cell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, Instant};

use chrono::Utc;
use gpui::{
    Bounds, Context, Entity, MouseButton, MouseMoveEvent, MouseUpEvent, PinchEvent, Pixels,
    ScrollHandle, ScrollWheelEvent, SharedString, Subscription, Window, div, prelude::*, px,
};

use zeron_engine::pr_ticket_cache::{
    ChatLinkStatus, ChecksStatus, MyPrItem, PrAction, PrStatus, TicketStatus,
};
use zeron_proto::{Chat, ChatIndicator, ChatLinkSource};
use zeron_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::overview_grouping::{self, GroupDimension, GroupKeyed, Partition};
use crate::overview_layout::{self, PackItem};
use crate::overview_positions;
use crate::popover;
use crate::shell::Shell;
use crate::state::{AppState, format_time_ago};
use crate::theme::Theme;
use crate::transcript::Transcript;

/// A chat with no activity for longer than this, and not currently
/// working/awaiting input, reads as stale. Adapted from
/// `session_canvas_server.py`'s `CURSOR_STALE_SECONDS` (3600s) — that value
/// times a continuously-polled cursor-liveness hook, a much noisier signal
/// than Zeron's per-message timestamps, so a day is the closer match in
/// spirit ("this chat has gone quiet") rather than a literal port of the
/// number.
const STALE_AFTER: chrono::Duration = chrono::Duration::hours(24);

/// How long a fetched `ChatLinkStatus` stays good before this view refetches
/// it — independent of the server's own cache TTL, just how often the UI
/// bothers to poll for a fresher answer.
const LINK_STATUS_REFRESH: Duration = Duration::from_secs(45);

/// Classification (category/origin/tool chips) and subagent scans are
/// settled-ish once a transcript exists, but an imported chat that keeps
/// running elsewhere (or gets resumed here) grows new tool calls/subagents —
/// a generous TTL instead of fetch-once-forever.
const CLASSIFICATION_REFRESH: Duration = Duration::from_secs(120);
const SUBAGENTS_REFRESH: Duration = Duration::from_secs(120);
/// Subagent refetch TTL while any of a chat's subagents is still `running`
/// — a running subagent flips to `done` on its own, and the tile's status
/// dot should follow within an idle tick or so, not two minutes later. Just
/// under [`IDLE_REFRESH`], same reasoning as [`CONTEXT_USAGE_REFRESH`].
const SUBAGENTS_RUNNING_REFRESH: Duration = Duration::from_secs(12);

/// One subagent, as `SCAN_CHAT_SUBAGENTS` reports it — the UI's own decode
/// of the wire item (a subset of `zeron_engine::subagent_scan::SubagentSummary`
/// plus the `status` field), so this view depends on the RPC contract rather
/// than the engine struct's exact shape. Unknown extra keys
/// (`transcriptPath`) are ignored.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubagentItem {
    agent_id: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    agent_type: Option<String>,
    #[serde(default)]
    status: SubagentStatus,
}

impl SubagentItem {
    /// The web's `sub.description || sub.agent_type || sub.id`.
    fn display_name(&self) -> String {
        self.description
            .clone()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| self.agent_type.clone().filter(|s| !s.trim().is_empty()))
            .unwrap_or_else(|| self.agent_id.clone())
    }
}

/// `SCAN_CHAT_SUBAGENTS`'s `status`. Missing (an engine that predates the
/// field) or unrecognized reads as `Done` — the settled, non-alarming
/// default; a subagent is never shown "running" without the engine saying so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum SubagentStatus {
    Running,
    #[default]
    #[serde(other)]
    Done,
}

impl SubagentStatus {
    /// Reuses the reference status palette: running = `--accent` (the
    /// working color), done = `--status-good`.
    fn color(self) -> gpui::Hsla {
        match self {
            SubagentStatus::Running => reference_status_color(ChatIndicator::Working),
            SubagentStatus::Done => reference_status_color(ChatIndicator::Completed),
        }
    }

    fn label(self) -> &'static str {
        match self {
            SubagentStatus::Running => "running",
            SubagentStatus::Done => "done",
        }
    }
}

/// `READ_SUBAGENT_TRANSCRIPT` reply: `{turns, model}`.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
struct SubagentTranscript {
    #[serde(default)]
    turns: Vec<TranscriptTurn>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
struct TranscriptTurn {
    /// `"user" | "assistant"`; anything else renders under its raw name.
    role: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    tools: Vec<TranscriptTool>,
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct TranscriptTool {
    name: String,
    #[serde(default)]
    input_preview: String,
    #[serde(default)]
    result_preview: Option<String>,
}

/// The web's `turn-role` label: "You" / "Assistant".
fn turn_role_label(role: &str) -> String {
    match role {
        "user" => "You".to_string(),
        "assistant" => "Assistant".to_string(),
        other => other.to_string(),
    }
}

/// Split message text on ``` fences into `(is_code, chunk)` segments — the
/// web `renderMarkdownLite`'s code-block treatment, minus everything else
/// (plain text keeps its newlines as-is). A fence's language tag line is
/// dropped; an unterminated fence runs to the end. Empty prose chunks
/// (e.g. between back-to-back fences) are skipped.
fn split_fenced_code(text: &str) -> Vec<(bool, String)> {
    let mut out = Vec::new();
    let mut rest = text;
    loop {
        let Some(open) = rest.find("```") else {
            let prose = rest.trim_matches('\n');
            if !prose.trim().is_empty() {
                out.push((false, prose.to_string()));
            }
            return out;
        };
        let prose = rest[..open].trim_matches('\n');
        if !prose.trim().is_empty() {
            out.push((false, prose.to_string()));
        }
        let after = &rest[open + 3..];
        // Drop the language tag (everything up to the first newline).
        let body_start = after.find('\n').map_or(after.len(), |nl| nl + 1);
        let body = &after[body_start..];
        let (code, next) = match body.find("```") {
            Some(close) => (&body[..close], &body[close + 3..]),
            None => (body, ""),
        };
        out.push((true, code.trim_end_matches('\n').to_string()));
        rest = next;
    }
}

/// Web `formatTurnTime`: a short local "Mon D, HH:MM", empty when absent or
/// unparseable.
fn format_turn_time(timestamp: Option<&str>) -> String {
    timestamp
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&chrono::Local).format("%b %-d, %H:%M").to_string())
        .unwrap_or_default()
}

/// Where a subagent-transcript peek's load stands.
#[derive(Debug, Clone, PartialEq)]
enum PeekLoad {
    Loading,
    Loaded(SubagentTranscript),
    Failed,
}

/// The read-only subagent-transcript mode of the right-side panel (web
/// `openTranscriptPanel(sessionId, subagentId, ...)`): distinct from the
/// live chat panel (`Overview::chat_panel_open`) — a subagent can't be
/// continued, so there's no composer, only its turns.
#[derive(Debug, Clone)]
struct SubagentPeek {
    chat_id: String,
    agent_id: String,
    name: String,
    agent_type: Option<String>,
    /// The `Overview::peek_request_seq` value this peek's fetch was issued
    /// under — web `transcriptRequestId`. See [`peek_reply_is_current`].
    request_id: u64,
    load: PeekLoad,
    /// `(turn index, tool index)` pairs whose result preview is disclosed
    /// (web `<details class="tool-detail">`).
    open_tools: HashSet<(usize, usize)>,
}

/// Stale-response guard (web `if (requestId !== transcriptRequestId)
/// return`): a landed `READ_SUBAGENT_TRANSCRIPT` reply only applies if the
/// panel is still showing the peek that issued it — not closed since, and
/// not superseded by a quicker click on another subagent (or the same one
/// again, which issues a fresh id).
fn peek_reply_is_current(peek: Option<&SubagentPeek>, request_id: u64) -> bool {
    peek.is_some_and(|p| p.request_id == request_id)
}

/// Context-window % moves constantly for a live chat, so it gets the
/// shortest TTL of any per-row fetch. Set just under [`IDLE_REFRESH`] so
/// every idle tick refetches it; a TTL of exactly 15s would lose the race
/// against the tick's own timer jitter and only refresh every other tick.
const CONTEXT_USAGE_REFRESH: Duration = Duration::from_secs(12);

/// Idle-refresh cadence while the overview is the visible route — see
/// `refresh_tick`. Only re-checks TTLs (each fetch keeps its own, longer
/// TTL) and repaints; it never forces a relayout by itself.
const IDLE_REFRESH: Duration = Duration::from_secs(15);

/// Every Nth idle tick also re-derives rows (staleness is time-based, so a
/// chat can cross `STALE_AFTER` while nothing else changes). ~60s — cheap
/// relative to the per-frame rebuild `rows_dirty` exists to avoid, and the
/// grouped layout cache stays keyed on ids so it still hits unless the
/// visible id set actually changed.
const STALENESS_REDERIVE_EVERY_TICKS: u32 = 4;

/// Web `fitViewToCards` padding, board-space px.
const FIT_PADDING: f32 = 80.0;
/// Search auto-fit only fires once the query has narrowed to this many
/// matches or fewer (web: `matches.length <= 8`).
const FIT_MAX_MATCHES: usize = 8;

/// `CHAT_CLASSIFICATION`'s cached response — category/origin (used as
/// `OverviewRow` fields/grouping keys) plus tool-usage breakdown (used only
/// by the expanded tile detail's chip rows, not copied onto `OverviewRow`
/// since nothing else needs it per-row).
#[derive(Clone, Default)]
struct ClassificationInfo {
    category: String,
    origin: String,
    tool_counts: HashMap<String, usize>,
    skills_loaded: Vec<String>,
}

/// One row's worth of what this pass can show.
#[derive(Clone)]
struct OverviewRow {
    status: ChatIndicator,
    chat: Chat,
    stale: bool,
    /// Linked ticket identifier, if `link_status` has it cached by the time
    /// `rows()` builds this row — copied in at construction (rather than
    /// read from `Overview.link_status` inside `group_key`) because
    /// `GroupKeyed::group_key` only has `&self` on the row itself, matching
    /// how `overview_grouping` is deliberately UI-crate-agnostic.
    ticket: Option<String>,
    /// Real `classify_heuristic`/`classify_entrypoint` output, fetched via
    /// `CHAT_CLASSIFICATION` and copied in the same way `ticket` is — `None`
    /// while the fetch is still pending, or for a chat that predates this
    /// feature (imported before classification existed, so it has no
    /// persisted category/origin) or was never externally imported at all
    /// (no cursor to read a classification from). All three are legitimate,
    /// not errors; `group_key` below falls back to `NONE_KEY` for them.
    category: Option<String>,
    origin: Option<String>,
    /// [`repo_key`] of `chat.cwd` — the Repo grouping key and the repo
    /// filter's key, computed once so both agree by construction.
    repo: String,
    /// Linked PR number/title, copied in like `ticket`: the number is the
    /// Ticket dimension's `PR #<n>` fallback (`ticketGroupKey`), the title is
    /// a search field (`sessionMatchesSearch` includes `pr.title`).
    pr_number: Option<u64>,
    pr_title: Option<String>,
    /// `CHAT_CONTEXT_USAGE` fraction (`0.0..=1.0`), copied in like `ticket`
    /// because it sets the tile's size and therefore feeds layout. `None`
    /// until the fetch lands, or when the engine has no usage for this chat.
    context_pct: Option<f32>,
    /// How many compact subagent tiles `subagent_tiles_for` renders for this
    /// chat (all of them) — copied in from `Overview.subagents` like
    /// `ticket`, because it feeds `tile_size` (subagent tiles push a tile
    /// taller than its context-% aspect ratio) and `tile_size` is a method on this type, not
    /// on `Overview`, which has no `&self` access to that map. `0` while the
    /// fetch is pending or the chat genuinely has none — both render no
    /// rows, so they're indistinguishable here on purpose.
    subagent_count: usize,
    /// Whether `badges_for` would render a PR/ticket badge row for this
    /// chat — copied in like `subagent_count`, for the same reason (it
    /// feeds `tile_size`). See [`link_has_badge_row`].
    has_badge_row: bool,
}

impl OverviewRow {
    /// This row's main-tile board-space size, `(width, height)` — the ONE
    /// place every packing/culling/overlap-repair/fit-to-view consumer reads
    /// a tile's size from (see the module doc comment), so a new content
    /// driver of tile height only ever needs adding here. Height is the
    /// `max` of the context-%-driven aspect ratio ([`tile_size_for_pct`]) and
    /// a deterministic estimate of what this row's actual content needs
    /// ([`tile_content_height_estimate`]) — the packing-time stand-in for
    /// the reference's real DOM measurement, since gpui has no "measure this
    /// offscreen" primitive to ask for one instead.
    fn tile_size(&self) -> (f32, f32) {
        let (w, aspect_h) = tile_size_for_pct(self.context_pct);
        let content_h = tile_content_height_estimate(self.subagent_count, self.has_badge_row);
        (w, aspect_h.max(content_h))
    }
}

impl GroupKeyed for OverviewRow {
    /// Repo/Ticket are proxies (cwd basename, linked-ticket identifier).
    /// Category/Origin are real per-chat classifications computed during
    /// import (`classify_heuristic`/`classify_entrypoint` +
    /// peon-hook-derived `agent_mode` detection, `external_import.rs`),
    /// fetched lazily via `CHAT_CLASSIFICATION` and cached on `Overview`
    /// (see `ensure_classification`) — `self.category`/`self.origin` are
    /// `Some` once that fetch lands. `NONE_KEY` is the honest fallback for
    /// "not fetched yet" / "chat predates this feature" / "never externally
    /// imported", not a permanent stub — `overview_grouping`'s fixed-order
    /// buckets absorb it into the dimension's last bucket either way.
    fn group_key(&self, dim: GroupDimension) -> String {
        match dim {
            GroupDimension::Category => self
                .category
                .clone()
                .unwrap_or_else(|| overview_grouping::NONE_KEY.to_string()),
            GroupDimension::Origin => self
                .origin
                .clone()
                .unwrap_or_else(|| overview_grouping::NONE_KEY.to_string()),
            GroupDimension::Repo => self.repo.clone(),
            GroupDimension::Ticket => ticket_group_key(self.ticket.as_deref(), self.pr_number),
        }
    }
}

impl GroupKeyed for &OverviewRow {
    fn group_key(&self, dim: GroupDimension) -> String {
        (*self).group_key(dim)
    }
}

/// Port of `repoGroupKey` (`session_canvas.html:968-972`): the cwd's last
/// path segment after stripping trailing slashes, `NONE_KEY` for no cwd (or
/// a cwd that is nothing but slashes).
///
/// Deliberate divergence from the reference: agent worktrees carry a
/// `.workspace-root` file at their root whose first line is the absolute path
/// of the superproject checkout. When `<cwd>/.workspace-root` exists and that
/// trimmed first line is non-empty, the key is the last segment of THAT path
/// instead, so worktree chats group under their real repo rather than each
/// worktree name becoming its own one-chat "repo". A missing, unreadable, or
/// empty file falls back to the reference behavior above.
///
/// Does a small fs read per call; only invoked from the cached `rows()`
/// recompute (not per frame), so it is not memoized.
fn repo_key(cwd: Option<&str>) -> String {
    let Some(cwd) = cwd else {
        return overview_grouping::NONE_KEY.to_string();
    };
    if let Some(root) = workspace_root_of(cwd) {
        if let Some(key) = last_segment(&root) {
            return key;
        }
    }
    last_segment(cwd).unwrap_or_else(|| overview_grouping::NONE_KEY.to_string())
}

fn last_segment(path: &str) -> Option<String> {
    match path.trim_end_matches('/').rsplit('/').next() {
        Some(last) if !last.is_empty() => Some(last.to_string()),
        _ => None,
    }
}

/// First line (trimmed) of `<cwd>/.workspace-root`, if present and non-empty.
fn workspace_root_of(cwd: &str) -> Option<String> {
    if cwd.is_empty() {
        return None;
    }
    let contents = std::fs::read_to_string(std::path::Path::new(cwd).join(".workspace-root")).ok()?;
    let line = contents.lines().next()?.trim();
    (!line.is_empty()).then(|| line.to_string())
}

/// Port of `ticketGroupKey` (`session_canvas.html:955-957`): ticket id, else
/// `PR #<n>` when a PR is linked, else `NONE_KEY`.
fn ticket_group_key(ticket: Option<&str>, pr_number: Option<u64>) -> String {
    match (ticket, pr_number) {
        (Some(t), _) => t.to_string(),
        (None, Some(n)) => format!("PR #{n}"),
        (None, None) => overview_grouping::NONE_KEY.to_string(),
    }
}

/// `CATEGORY_META` labels (`session_canvas.html:921-928`).
fn category_label(key: &str) -> String {
    match key {
        "implementing" => "Implementing".into(),
        "pr_review" => "PR review".into(),
        "research" => "Research".into(),
        "planning" => "Planning".into(),
        "quick_question" => "Quick question".into(),
        "other" => "Other".into(),
        other => other.replace('_', " "),
    }
}

/// `ORIGIN_META` labels (`session_canvas.html:933-940`). Zeron has no
/// Cursor import, but the label is kept so the legend/table stay a 1:1 port.
fn origin_label(key: &str) -> String {
    match key {
        "agent_mode" => "agent-mode.sh".into(),
        "cursor" => "Cursor".into(),
        "sdk_driven" => "SDK-driven".into(),
        "claude_desktop" => "Claude Desktop".into(),
        "bare_cli" => "Bare CLI".into(),
        "unknown" => "Unknown".into(),
        other => other.replace('_', " "),
    }
}

/// Display label for a raw group key (`PartitionGroup::key`) on `dim` —
/// the reference's per-dimension `getMeta().label`: fixed tables for
/// Category/Origin, "No repo"/"No ticket / PR" for the dynamic dimensions'
/// `NONE_KEY` bucket (`repoGroupMeta`/`ticketGroupMeta`), the raw key (a
/// folder name, a ticket id) otherwise.
fn group_label(dim: GroupDimension, key: &str) -> String {
    match dim {
        GroupDimension::Category => category_label(key),
        GroupDimension::Origin => origin_label(key),
        GroupDimension::Repo if key == overview_grouping::NONE_KEY => "No repo".into(),
        GroupDimension::Ticket if key == overview_grouping::NONE_KEY => "No ticket / PR".into(),
        GroupDimension::Repo | GroupDimension::Ticket => key.to_string(),
    }
}

/// `session_canvas.html`'s `HASH_COLOR_PALETTE` (`~888`) — Repo/Ticket group
/// colors have no fixed lookup table (unlike Category/Origin), so the same
/// key always lands on the same hue via a deterministic hash instead.
const HASH_COLOR_PALETTE: [u32; 8] = [
    0x3987e5, 0xd95926, 0x199e70, 0xc98500, 0xd55181, 0x008300, 0x9085e9, 0xe66767,
];

/// Exact port of `session_canvas.html`'s `hashColor` (`~889-893`):
/// `hash = (hash * 31 + charCode) >>> 0` per char, then `PALETTE[hash %
/// PALETTE.length]`. `wrapping_mul`/`wrapping_add` on `u32` reproduces JS's
/// `>>> 0` unsigned-32-bit truncation exactly — this must NOT be `i32` math,
/// which would diverge on overflow.
fn hash_color(key: &str) -> gpui::Hsla {
    let mut hash: u32 = 0;
    for ch in key.chars() {
        hash = hash.wrapping_mul(31).wrapping_add(ch as u32);
    }
    let idx = (hash as usize) % HASH_COLOR_PALETTE.len();
    gpui::rgb(HASH_COLOR_PALETTE[idx]).into()
}

/// Exact `--status-*` hex values from `session_canvas.html`'s `:root` CSS
/// (`~20-22`) and `STATUS_STYLES` (`~837-846`) — replaces the app theme's
/// own approximation so tile tint/status colors genuinely match the
/// reference instead of merely resembling it.
fn reference_status_color(status: ChatIndicator) -> gpui::Hsla {
    match status {
        ChatIndicator::Working => gpui::rgb(0x3987e5).into(), // --accent (working/running)
        // Reference's `needs_input` is --status-critical (red), not a
        // distinct blue — a real semantic difference from this app's prior
        // theme-based choice, ported faithfully rather than kept.
        ChatIndicator::AwaitingInput => gpui::rgb(0xd03b3b).into(),
        ChatIndicator::Errored => gpui::rgb(0xd03b3b).into(), // --status-critical (blocked)
        ChatIndicator::Completed => gpui::rgb(0x0ca30c).into(), // --status-good (done)
        ChatIndicator::Idle => gpui::rgb(0x898781).into(),    // --text-muted
    }
}

/// `STATUS_STYLES` icon + label (`session_canvas.html:892-905`) mapped from
/// Zeron's real `ChatIndicator` vocabulary. What Zeron can and can't produce:
/// - `pending_approval` (#fab219 "⚡"): no equivalent. `SessionStatus` has no
///   permission-wait state — an `InputRequested` event (the only "blocked on
///   the human" signal) maps to `AwaitingInput`, which is the web's
///   `needs_input`. Not invented here.
/// - `blocked` ("✕"): Zeron's `Errored` (an unseen failed run) — labeled
///   "error", which is what it actually is.
/// - `not_running` ("○"): an `Idle` chat that came in through external
///   import (`not_running == true`, i.e. it has an import classification) —
///   a real Claude Code session found on disk with no live run in Zeron,
///   the web's exact meaning. A Zeron-native idle chat stays "idle" ("–").
/// - `running`/`unknown`: aliases/fallbacks with no distinct Zeron source.
fn status_style(status: ChatIndicator, not_running: bool) -> (&'static str, &'static str) {
    match status {
        ChatIndicator::Working => ("●", "working"),
        ChatIndicator::AwaitingInput => ("●", "needs input"),
        ChatIndicator::Errored => ("✕", "error"),
        ChatIndicator::Completed => ("✓", "done"),
        ChatIndicator::Idle if not_running => ("○", "not running"),
        ChatIndicator::Idle => ("–", "idle"),
    }
}

/// `CHECKS_META` (`session_canvas.html:912-916`): color + icon per checks
/// rollup.
fn checks_style(checks: ChecksStatus) -> (gpui::Hsla, &'static str, &'static str) {
    match checks {
        ChecksStatus::Passing => (gpui::rgb(0x0ca30c).into(), "✓", "passing"),
        ChecksStatus::Pending => (gpui::rgb(0xfab219).into(), "…", "pending"),
        ChecksStatus::Failing => (gpui::rgb(0xd03b3b).into(), "✕", "failing"),
    }
}

/// Web `isUnreadDone` reconciliation (`session_canvas.html:1289-1298`): a
/// chat that is no longer `done` drops out of the acknowledged set so its
/// NEXT completion starts unread again. Returns whether the set changed (so
/// the caller persists it).
fn reconcile_acknowledged(acknowledged: &mut BTreeSet<String>, chat_id: &str, is_done: bool) -> bool {
    !is_done && acknowledged.remove(chat_id)
}

/// Whether a tile should carry the unread-done ribbon.
fn is_unread_done(acknowledged: &BTreeSet<String>, chat_id: &str, is_done: bool) -> bool {
    is_done && !acknowledged.contains(chat_id)
}

/// `sessionMatchesSearch` (`session_canvas.html:1464-1470`): case-insensitive
/// substring over whichever fields the row carries. `query_lower` must
/// already be trimmed + lowercased; empty matches everything.
fn matches_search(query_lower: &str, fields: &[Option<&str>]) -> bool {
    if query_lower.is_empty() {
        return true;
    }
    fields
        .iter()
        .flatten()
        .any(|field| field.to_lowercase().contains(query_lower))
}

/// `updateMyPrsButtonBadge` (`session_canvas.html:2323-2328`): PRs with at
/// least one action reason, EXCEPT those whose every reason is
/// `ready_to_merge` (good news alone shouldn't turn the button red).
fn actionable_pr_count<'a>(reasons: impl IntoIterator<Item = &'a [PrAction]>) -> usize {
    reasons
        .into_iter()
        .filter(|r| !r.is_empty() && !r.iter().all(|a| *a == PrAction::ReadyToMerge))
        .count()
}

/// PR-sidebar search: the toolbar's same query (trimmed + lowercased, empty
/// matches everything) over repo (`owner/name`), title, number — matched as
/// `#<n>`, so both `123` and `#123` hit — and head branch when the detail
/// sweep has it.
fn pr_matches_search(query_lower: &str, item: &MyPrItem) -> bool {
    let number = format!("#{}", item.summary.number);
    matches_search(
        query_lower,
        &[
            item.summary.repo.as_deref(),
            item.summary.title.as_deref(),
            Some(number.as_str()),
            item.detail.as_ref().and_then(|d| d.branch.as_deref()),
        ],
    )
}

/// Restore the sidebar's open state from the persisted UI-flag set. Unknown
/// entries (a future flag, or junk) are ignored rather than failing the load.
/// No view mode was ever persisted — `ViewMode` (formerly with a `MyPrs`
/// variant) is in-memory only and every overview starts in List — so there
/// is no stale "my PRs view" value to migrate.
fn pr_sidebar_open_from_flags(flags: &BTreeSet<String>) -> bool {
    flags.contains(PR_SIDEBAR_FLAG)
}

/// Jump-to-tile framing for a PR row click: center `rect` in the viewport,
/// zooming IN to at least 100% when it still fits (so the target is
/// readable) but never out from the user's current zoom unless the tile
/// wouldn't fit otherwise. `viewport` is the canvas container's own measured
/// size, which already excludes both side panels.
fn focus_view_on_rect(rect: (f32, f32, f32, f32), viewport: (f32, f32), current_zoom: f32) -> (f32, (f32, f32)) {
    let (x, y, w, h) = rect;
    let fit_zoom = (viewport.0 / (w + FIT_PADDING * 2.0).max(1.0))
        .min(viewport.1 / (h + FIT_PADDING * 2.0).max(1.0));
    let zoom = current_zoom.max(1.0).min(fit_zoom).clamp(ZOOM_MIN, ZOOM_MAX);
    let pan = (
        viewport.0 / 2.0 - (x + w / 2.0) * zoom,
        viewport.1 / 2.0 - (y + h / 2.0) * zoom,
    );
    (zoom, pan)
}

/// `fitViewToCards` (`session_canvas.html:1703-1721`): given board-space
/// rects `(x, y, w, h)` and the viewport size, the `(zoom, pan)` that frames
/// them with `FIT_PADDING` on every side, zoom clamped to
/// `[ZOOM_MIN, ZOOM_MAX]`, centered on the bbox midpoint. `None` for no
/// rects.
fn fit_view_to_rects(rects: &[(f32, f32, f32, f32)], viewport: (f32, f32)) -> Option<(f32, (f32, f32))> {
    if rects.is_empty() {
        return None;
    }
    let (mut min_x, mut min_y) = (f32::INFINITY, f32::INFINITY);
    let (mut max_x, mut max_y) = (f32::NEG_INFINITY, f32::NEG_INFINITY);
    for &(x, y, w, h) in rects {
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x + w);
        max_y = max_y.max(y + h);
    }
    let content_w = (max_x - min_x + FIT_PADDING * 2.0).max(1.0);
    let content_h = (max_y - min_y + FIT_PADDING * 2.0).max(1.0);
    let zoom = (viewport.0 / content_w)
        .min(viewport.1 / content_h)
        .clamp(ZOOM_MIN, ZOOM_MAX);
    let pan = (
        viewport.0 / 2.0 - ((min_x + max_x) / 2.0) * zoom,
        viewport.1 / 2.0 - ((min_y + max_y) / 2.0) * zoom,
    );
    Some((zoom, pan))
}

/// Whether the off-screen rescue pill should show this frame: at least one
/// tile exists on the canvas (post-filter), but the per-tile viewport-
/// intersection check the render loop already runs while culling
/// (`Overview::tile_in_viewport`, against each tile's CURRENT animated
/// position — not a separate target-position check, so a tile mid-
/// organize-move can't cause a false positive) found none of them on
/// screen. A board with no tiles at all (everything filtered out) is the
/// ordinary empty state, not a rescue case, so it's `false` too. Kept as a
/// plain function of the render loop's own tally so it's directly testable
/// without a live `Overview`/`Context`.
fn no_tiles_visible(tile_count: usize, any_tile_visible: bool) -> bool {
    tile_count > 0 && !any_tile_visible
}

/// Repo-filter button label (`updateRepoFilterBtnState`,
/// `session_canvas.html:2101-2105`).
fn repo_filter_label(hidden: usize) -> String {
    if hidden > 0 {
        format!("Repos ({hidden} hidden)")
    } else {
        "Repos".to_string()
    }
}

/// Toggle one repo's visibility in the hidden set (a checkbox click).
fn toggle_repo_hidden(hidden: &mut BTreeSet<String>, repo: &str) {
    if !hidden.remove(repo) {
        hidden.insert(repo.to_string());
    }
}

/// Repo-filter list order (`renderRepoFilterList`): alphabetical, `NONE_KEY`
/// last.
fn sorted_repo_keys(known: &BTreeSet<String>) -> Vec<String> {
    let mut keys: Vec<String> = known
        .iter()
        .filter(|k| k.as_str() != overview_grouping::NONE_KEY)
        .cloned()
        .collect();
    if known.contains(overview_grouping::NONE_KEY) {
        keys.push(overview_grouping::NONE_KEY.to_string());
    }
    keys
}

/// Web's `.worktree` badge rule (`is_worktree`, `session_canvas_server.py:
/// 300-304`): cwd under `/.worktrees/` or `/.agent-worktrees/`. UI-side
/// fallback until/unless the engine's link status carries its own flag.
fn cwd_is_worktree(cwd: Option<&str>) -> bool {
    cwd.is_some_and(|c| c.contains("/.worktrees/") || c.contains("/.agent-worktrees/"))
}

/// `shortenCwd` (`session_canvas.html:1045-1049`): more than 3 segments
/// shows `.../<last two>`.
fn shorten_cwd(cwd: &str) -> String {
    let parts: Vec<&str> = cwd.split('/').collect();
    if parts.len() > 3 {
        format!(".../{}", parts[parts.len() - 2..].join("/"))
    } else {
        cwd.to_string()
    }
}

/// `session_canvas.html`'s fixed `CATEGORY_META` colors (`~862-869`), keyed
/// by the real per-chat category `ensure_classification` fetches (imported
/// chats only — a chat without one never gets a badge, and groups into
/// `other`).
fn category_color(key: &str) -> gpui::Hsla {
    let hex = match key {
        "implementing" => 0x3987e5,
        "pr_review" => 0xd95926,
        "research" => 0x199e70,
        "planning" => 0xd55181,
        "quick_question" => 0xc98500,
        _ => 0x898781, // "other" / unrecognized
    };
    gpui::rgb(hex).into()
}

/// Per-tile category badge — `renderTile`'s own dot+label (`~1054-1165`)
/// shown on every tile regardless of grouping state, distinct from the
/// GROUP header color a previous pass already wired up. Only rendered once a
/// real classification is known (`row.category.is_some()`) — no placeholder
/// badge for a pending fetch or a chat with no classification to show.
fn category_badge(row: &OverviewRow, zoom: f32) -> Option<gpui::AnyElement> {
    let category = row.category.as_deref()?;
    let color = category_color(category);
    Some(
        div()
            .flex()
            .items_center()
            .gap(px(3.0 * zoom))
            .child(div().flex_none().size(px(5.0 * zoom)).rounded_full().bg(color))
            .child(
                div()
                    .text_size(crate::typography::ui_rems(9.0 * zoom))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(color)
                    .child(SharedString::from(category_label(category).to_uppercase())),
            )
            .into_any_element(),
    )
}

/// `session_canvas.html`'s fixed `ORIGIN_META` colors (`~874-881`), keyed by
/// the real per-chat origin `ensure_classification` fetches.
fn origin_color(key: &str) -> gpui::Hsla {
    let hex = match key {
        "agent_mode" => 0x3987e5,
        "cursor" => 0x199e70,
        "sdk_driven" => 0x9085e9,
        "claude_desktop" => 0xc98500,
        "bare_cli" => 0xd55181,
        _ => 0x898781, // "unknown"
    };
    gpui::rgb(hex).into()
}

/// A group's display color for `dim`/`key` — `NONE_KEY` is always
/// `--text-muted` (matches `ticketGroupMeta`/`repoGroupMeta`'s explicit
/// special-case, `session_canvas.html:~899,~914`); Category/Origin use their
/// fixed lookup tables; Repo/Ticket hash the key since they have no bounded
/// value set.
fn group_color(dim: GroupDimension, key: &str) -> gpui::Hsla {
    if key == overview_grouping::NONE_KEY {
        return gpui::rgb(0x898781).into(); // --text-muted
    }
    match dim {
        GroupDimension::Category => category_color(key),
        GroupDimension::Origin => origin_color(key),
        GroupDimension::Repo | GroupDimension::Ticket => hash_color(key),
    }
}

#[cfg(test)]
mod color_tests {
    use super::*;

    /// Pins `hash_color` against `session_canvas.html`'s `hashColor`
    /// (`~889-893`) computed independently in Python with the same
    /// `(hash * 31 + charCode) & 0xFFFFFFFF` truncation JS's `>>> 0`
    /// performs, to catch a signed/unsigned overflow divergence rather than
    /// just eyeballing the Rust output.
    #[test]
    fn hash_color_matches_reference_js_algorithm() {
        let cases: &[(&str, u32)] = &[
            ("zeron", 6),
            ("ui", 4),
            ("ENG-1234", 1),
            ("agent-mode-tools", 1),
            ("a", 1),
        ];
        for (key, expected_idx) in cases {
            let expected: gpui::Hsla = gpui::rgb(HASH_COLOR_PALETTE[*expected_idx as usize]).into();
            assert_eq!(
                hash_color(key),
                expected,
                "hash_color({key:?}) should land on palette index {expected_idx}"
            );
        }
    }

    #[test]
    fn hash_color_empty_key_is_index_zero() {
        let expected: gpui::Hsla = gpui::rgb(HASH_COLOR_PALETTE[0]).into();
        assert_eq!(hash_color(""), expected);
    }

    #[test]
    fn none_key_is_always_text_muted_regardless_of_dimension() {
        let expected: gpui::Hsla = gpui::rgb(0x898781).into();
        for dim in [
            GroupDimension::Category,
            GroupDimension::Origin,
            GroupDimension::Repo,
            GroupDimension::Ticket,
        ] {
            assert_eq!(group_color(dim, overview_grouping::NONE_KEY), expected);
        }
    }
}

/// Total leaf-row count under a `Partition` node, for a group header's
/// `(N)` count — a `Groups` node's own count is the sum of its children's,
/// recursed all the way down to the `Leaves` at the bottom.
fn count_rows<T>(partition: &Partition<T>) -> usize {
    match partition {
        Partition::Leaves(rows) => rows.len(),
        Partition::Groups { groups, .. } => groups.iter().map(|g| count_rows(&g.items)).sum(),
    }
}

/// One flattened item for List mode's indented grouped rendering — a group
/// header (label/nesting-depth/leaf-count) or a row, in the depth-first order
/// [`flatten_partition`] visits the tree.
enum ListItem<'a> {
    Header { label: String, color: gpui::Hsla, depth: usize, count: usize },
    Row(&'a OverviewRow),
}

/// Depth-first flatten of a `Partition` tree into `ListItem`s: a header
/// immediately followed by everything in its subtree (nested headers and
/// rows alike), each nested level one `depth` deeper — what List mode
/// indents by. The flat/no-groups case never reaches this function (see
/// `render_list_body`'s `active_groups.is_empty()` branch).
fn flatten_partition<'a>(
    partition: &Partition<&'a OverviewRow>,
    depth: usize,
    out: &mut Vec<ListItem<'a>>,
) {
    match partition {
        Partition::Leaves(rows) => {
            for &row in rows {
                out.push(ListItem::Row(row));
            }
        }
        Partition::Groups { dimension, groups } => {
            for g in groups {
                out.push(ListItem::Header {
                    label: group_label(*dimension, &g.key),
                    color: group_color(*dimension, &g.key),
                    depth,
                    count: count_rows(&g.items),
                });
                flatten_partition(&g.items, depth + 1, out);
            }
        }
    }
}

/// The result of [`layout_partition`]: every leaf row's LOCAL position
/// (relative to this node's own top-left `(0, 0)`) and size, every group
/// label's LOCAL position, and this node's own total content size — a
/// caller combines these with the node's own screen offset (see
/// `render_canvas_grouped`) to get final absolute canvas coordinates.
#[derive(Clone)]
struct LaidOutPartition {
    /// chat_id -> local (x, y).
    positions: HashMap<String, (f32, f32)>,
    /// (label, local x, local y, nesting depth) for every `Groups` level in
    /// this subtree, including nested ones — a caller renders one text
    /// element per entry.
    labels: Vec<(String, gpui::Hsla, f32, f32, usize)>,
    /// (local x, local y, width, height, nesting depth) — one bounding box
    /// per `Groups` level (label + all its content), including nested ones.
    /// A caller draws a background/border rect per entry so groups read as
    /// distinct visual sections, not just a floating text label with no
    /// boundary (the reported "doesn't look like blocks or sections" gap).
    group_boxes: Vec<(f32, f32, f32, f32, usize)>,
    width: f32,
    height: f32,
}

/// Recursively lays out a `Partition` tree, matching `partitionAndPlace`'s
/// structure (session_canvas.html:1499-1541): depth 0's groups (and every
/// EVEN depth below it) lay out side by side as columns; odd depths stack
/// top-to-bottom as rows nested inside their parent column — alternating by
/// depth is the direct equivalent of the reference always alternating
/// columns/rows per nesting level. `Leaves` get packed via
/// `overview_layout::pack_rects` (free-rectangle packing, not a fixed grid)
/// — this is the LOCAL per-group pack; a caller runs a second, GLOBAL
/// `overview_layout::repair_overlaps` pass across every leaf's combined
/// absolute position afterward, matching the reference's two-tier repair
/// (packRects' own embedded local repair, then organizeByDimensions' final
/// cross-group sweep).
fn layout_partition(partition: &Partition<&OverviewRow>, depth: usize) -> LaidOutPartition {
    match partition {
        Partition::Leaves(rows) => {
            // Each tile's real context-% size — the web's `measureItem`
            // equivalent, known up front here instead of read off the DOM.
            let items: Vec<PackItem> = rows
                .iter()
                .map(|r| {
                    let (width, height) = r.tile_size();
                    PackItem {
                        id: r.chat.id.clone(),
                        width,
                        height,
                    }
                })
                .collect();
            let packed = overview_layout::pack_rects(&items, TILE_GAP);
            LaidOutPartition {
                positions: packed
                    .positions
                    .into_iter()
                    .map(|(id, p)| (id, (p.x, p.y)))
                    .collect(),
                labels: Vec::new(),
                group_boxes: Vec::new(),
                width: packed.width,
                height: packed.height,
            }
        }
        Partition::Groups { dimension, groups } => {
            let horizontal = depth % 2 == 0;
            let mut positions: HashMap<String, (f32, f32)> = HashMap::new();
            let mut labels: Vec<(String, gpui::Hsla, f32, f32, usize)> = Vec::new();
            let mut group_boxes: Vec<(f32, f32, f32, f32, usize)> = Vec::new();
            let mut cursor = 0.0f32;
            let mut cross = 0.0f32;
            for g in groups {
                let child = layout_partition(&g.items, depth + 1);
                let (label_x, label_y) = if horizontal { (cursor, 0.0) } else { (0.0, cursor) };
                let (content_x, content_y) = if horizontal {
                    (cursor, GROUP_LABEL_H)
                } else {
                    (0.0, cursor + GROUP_LABEL_H)
                };
                // `makeLaneHeader`'s `${meta.label} (${count})`.
                labels.push((
                    format!("{} ({})", group_label(*dimension, &g.key), count_rows(&g.items)),
                    group_color(*dimension, &g.key),
                    label_x,
                    label_y,
                    depth,
                ));
                for (id, (x, y)) in child.positions {
                    positions.insert(id, (x + content_x, y + content_y));
                }
                for (lbl, lcolor, lx, ly, ldepth) in child.labels {
                    labels.push((lbl, lcolor, lx + content_x, ly + content_y, ldepth));
                }
                // Floor for a degenerate (empty) child; a real leaf group's
                // packed width already spans its widest tile.
                let child_w = child.width.max(TILE_MIN_W);
                let child_h = child.height + GROUP_LABEL_H;
                group_boxes.push((label_x, label_y, child_w, child_h, depth));
                for (bx, by, bw, bh, bdepth) in child.group_boxes {
                    group_boxes.push((bx + content_x, by + content_y, bw, bh, bdepth));
                }
                if horizontal {
                    cursor += child_w + GROUP_GAP;
                    cross = cross.max(child_h);
                } else {
                    cursor += child_h + GROUP_GAP;
                    cross = cross.max(child_w);
                }
            }
            let (width, height) = if horizontal { (cursor, cross) } else { (cross, cursor) };
            LaidOutPartition { positions, labels, group_boxes, width, height }
        }
    }
}

struct LinkStatusEntry {
    status: ChatLinkStatus,
    /// Uncommitted diff stat, if the engine's `CHAT_LINK_STATUS` response
    /// carries a `diffStat` object (`{filesChanged, linesAdded,
    /// linesRemoved}`, the reference's `diff_stat`). Read off the raw JSON
    /// rather than a `ChatLinkStatus` field so this lights up the moment the
    /// engine adds it, with no UI change — absent today.
    diff_stat: Option<DiffStat>,
    /// Engine-provided `isWorktree`, when present; otherwise the detail pane
    /// falls back to [`cwd_is_worktree`].
    is_worktree: Option<bool>,
    fetched_at: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiffStat {
    files_changed: u64,
    lines_added: u64,
    lines_removed: u64,
}

/// What of a landed link status feeds `OverviewRow`/grouping/search — a
/// refetch whose fingerprint is unchanged must not dirty rows or drop the
/// grouped layout cache (badges/detail read `link_status` live at render).
/// Includes [`link_has_badge_row`] because that bool is ALSO baked onto
/// `OverviewRow` now (`has_badge_row`, feeding `tile_size`) — a PR's action
/// reasons can flip the badge row's presence (e.g. checks start failing)
/// without the ticket identifier/PR number/PR title changing at all, and a
/// stale `has_badge_row` would mean stale packing.
fn link_fingerprint(status: &ChatLinkStatus) -> (Option<String>, Option<u64>, Option<String>, bool) {
    (
        status.ticket.as_ref().map(|t| t.identifier.clone()),
        status.pr.as_ref().map(|p| p.number),
        status.pr.as_ref().and_then(|p| p.title.clone()),
        link_has_badge_row(status),
    )
}

/// Whether `badges_for` would render a PR/ticket badge row for this link
/// status: a linked ticket, or a PR with at least one action reason. Shared
/// between `badges_for` (what to draw) and `rows()`/`link_fingerprint`
/// (baking presence into `OverviewRow::has_badge_row`, which feeds
/// `tile_size`) so the two can never disagree about whether a row exists.
fn link_has_badge_row(status: &ChatLinkStatus) -> bool {
    status.ticket.is_some() || status.pr.as_ref().is_some_and(|pr| !pr.action_reasons().is_empty())
}

/// Which durable link slot a `SET_CHAT_LINK` write targets — the RPC's
/// `kind: "pr" | "ticket"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinkKind {
    Pr,
    Ticket,
}

impl LinkKind {
    fn wire(self) -> &'static str {
        match self {
            LinkKind::Pr => "pr",
            LinkKind::Ticket => "ticket",
        }
    }
}

/// What the "Link PR / ticket…" input parsed to: the RPC `kind` plus the
/// normalized value to store.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedLink {
    kind: LinkKind,
    value: String,
}

/// Error text shown under the link input when nothing parses.
const LINK_PARSE_ERROR: &str = "Paste a GitHub PR URL (github.com/owner/repo/pull/123) or a ticket id like ENG-1234.";

/// The link input's one-box smart parse:
/// - a `github.com/<owner>/<repo>/pull/<n>` URL (scheme and `www.`
///   optional, any trailing `/files`, `?query` or `#fragment` ignored) →
///   `pr`, normalized to `https://github.com/<owner>/<repo>/pull/<n>` —
///   the exact shape `gh` reports, which the PR sidebar matches chats by. A
///   bare `#123` is NOT accepted: it doesn't say which repo.
/// - an `XX-1234`-shaped id (2-6 letters, `-`, digits — the engine's own
///   `extract_ticket_id` shape) → `ticket`, uppercased. A Linear issue URL
///   (`linear.app/<ws>/issue/ENG-1234/...`) is accepted too and reduced to
///   its id.
/// - anything else → [`LINK_PARSE_ERROR`].
fn parse_link_input(raw: &str) -> Result<ParsedLink, &'static str> {
    let input = raw.trim();
    if input.is_empty() {
        return Err(LINK_PARSE_ERROR);
    }
    let without_scheme = input
        .strip_prefix("https://")
        .or_else(|| input.strip_prefix("http://"))
        .unwrap_or(input);
    let without_www = without_scheme.strip_prefix("www.").unwrap_or(without_scheme);
    // Drop `?query` / `#fragment` before splitting into path segments.
    let path = without_www.split(['?', '#']).next().unwrap_or_default();
    let segments: Vec<&str> = path.split('/').collect();
    match segments.first().map(|host| host.to_ascii_lowercase()).as_deref() {
        Some("github.com") => {
            if let [_, owner, repo, "pull", number, ..] = segments.as_slice()
                && !owner.is_empty()
                && !repo.is_empty()
                && !number.is_empty()
                && number.bytes().all(|b| b.is_ascii_digit())
            {
                return Ok(ParsedLink {
                    kind: LinkKind::Pr,
                    value: format!("https://github.com/{owner}/{repo}/pull/{number}"),
                });
            }
            Err(LINK_PARSE_ERROR)
        }
        Some("linear.app") => match segments.as_slice() {
            [_, _, "issue", id, ..] => ticket_id_shape(id)
                .map(|value| ParsedLink { kind: LinkKind::Ticket, value })
                .ok_or(LINK_PARSE_ERROR),
            _ => Err(LINK_PARSE_ERROR),
        },
        _ => ticket_id_shape(input)
            .map(|value| ParsedLink { kind: LinkKind::Ticket, value })
            .ok_or(LINK_PARSE_ERROR),
    }
}

/// `Some(uppercased)` when the WHOLE of `s` is a ticket id: 2-6 ASCII
/// letters, `-`, one or more digits (the engine's `extract_ticket_id`
/// shape, anchored instead of searched).
fn ticket_id_shape(s: &str) -> Option<String> {
    let (letters, digits) = s.split_once('-')?;
    let ok = (2..=6).contains(&letters.len())
        && letters.bytes().all(|b| b.is_ascii_alphabetic())
        && !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit());
    ok.then(|| s.to_ascii_uppercase())
}

/// Provenance tooltip for a PR/ticket line. `None` source = no durable link
/// stored; the shown PR/ticket came from branch/title inference.
fn link_source_label(kind: LinkKind, source: Option<ChatLinkSource>) -> &'static str {
    match (kind, source) {
        (_, Some(ChatLinkSource::Manual)) => "Linked manually",
        (LinkKind::Pr, Some(ChatLinkSource::CreatedInChat)) => "PR created in this chat",
        (LinkKind::Ticket, Some(ChatLinkSource::CreatedInChat)) => "Created in this chat",
        (_, Some(ChatLinkSource::Mentioned)) => "Mentioned in conversation",
        (_, None) => "Inferred from branch",
    }
}

/// `SET_CHAT_LINK`'s reply: the chat row's durable link fields after the
/// write.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetChatLinkReply {
    linked_pr_url: Option<String>,
    linked_pr_source: Option<ChatLinkSource>,
    linked_ticket_id: Option<String>,
    linked_ticket_source: Option<ChatLinkSource>,
}

/// PR number off a `.../pull/<n>` URL (`0` if it has none).
fn pr_number_from_url(url: &str) -> u64 {
    url.rsplit_once("/pull/")
        .and_then(|(_, rest)| rest.split(['/', '?', '#']).next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// Stand-in `PrStatus` for a just-linked URL whose detail the engine's
/// cache hasn't fetched yet (`CHAT_LINK_STATUS` is a pure cache read — a
/// miss comes back `pr: null` until the background sweep fills it). No
/// action reasons (`has_reviewer_requested: true`), so it never paints a
/// false "Needs reviewer" badge.
fn placeholder_pr(url: &str) -> PrStatus {
    PrStatus {
        number: pr_number_from_url(url),
        url: Some(url.to_string()),
        state: "loading…".to_string(),
        is_draft: false,
        review_decision: None,
        reviewers: Vec::new(),
        checks: None,
        title: None,
        branch: None,
        mergeable: String::new(),
        has_reviewer_requested: true,
    }
}

/// Optimistic local update after a successful `SET_CHAT_LINK` — the badge/
/// detail change the same frame instead of waiting on the refetch (and on
/// the engine's sweep to cache the new PR/ticket's detail). A set whose
/// value already matches what's shown keeps the real detail; a different
/// value gets a placeholder; a clear drops the slot (the follow-up refetch
/// brings back whatever inference finds).
fn apply_link_reply(status: &mut ChatLinkStatus, kind: LinkKind, reply: &SetChatLinkReply) {
    match kind {
        LinkKind::Pr => match reply.linked_pr_url.as_deref() {
            Some(url) => {
                if status.pr.as_ref().and_then(|p| p.url.as_deref()) != Some(url) {
                    status.pr = Some(placeholder_pr(url));
                }
                status.pr_source = reply.linked_pr_source;
            }
            None => {
                status.pr = None;
                status.pr_source = None;
            }
        },
        LinkKind::Ticket => match reply.linked_ticket_id.as_deref() {
            Some(id) => {
                if status.ticket.as_ref().map(|t| t.identifier.as_str()) != Some(id) {
                    status.ticket = Some(TicketStatus {
                        identifier: id.to_string(),
                        title: None,
                        url: None,
                        status: None,
                    });
                }
                status.ticket_source = reply.linked_ticket_source;
            }
            None => {
                status.ticket = None;
                status.ticket_source = None;
            }
        },
    }
}

/// A landed `CHAT_LINK_STATUS` refetch, merged over what was shown: when a
/// durable link exists (`*_source` set) but the engine's cache hasn't got
/// its detail yet (`pr`/`ticket` null), keep the previous value (typically
/// [`apply_link_reply`]'s placeholder) instead of blanking the badge until
/// the sweep catches up.
fn merge_refetched_link(previous: Option<&ChatLinkStatus>, mut next: ChatLinkStatus) -> ChatLinkStatus {
    if let Some(prev) = previous {
        if next.pr.is_none() && next.pr_source.is_some() && prev.pr_source.is_some() {
            next.pr = prev.pr.clone();
        }
        if next.ticket.is_none() && next.ticket_source.is_some() && prev.ticket_source.is_some() {
            next.ticket = prev.ticket.clone();
        }
    }
    next
}

/// Tiny hover tooltip carrying one short text (link provenance, a compact
/// subagent tile's full name). Wraps past `max_w` (web `.name-tooltip`'s
/// `max-width` + `white-space: normal`).
struct LinkSourceTooltip(SharedString);

impl Render for LinkSourceTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div()
            .px(px(8.0))
            .py(px(5.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border_strong)
            .bg(theme.surface_raised)
            .shadow_md()
            .max_w(px(280.0))
            .text_size(px(11.0))
            .text_color(theme.text)
            .child(self.0.clone())
    }
}

/// Attach the provenance tooltip ([`link_source_label`]) to a PR/ticket
/// element.
fn link_source_hover(
    el: gpui::Stateful<gpui::Div>,
    kind: LinkKind,
    source: Option<ChatLinkSource>,
) -> gpui::Stateful<gpui::Div> {
    let label = SharedString::from(link_source_label(kind, source));
    el.tooltip(move |_, cx| cx.new(|_| LinkSourceTooltip(label.clone())).into())
}

/// The tiny pin glyph marking a manually linked PR/ticket.
fn manual_link_pin(color: gpui::Hsla, zoom: f32) -> gpui::Svg {
    crate::icons::icon(crate::icons::PIN)
        .size(px(9.0 * zoom))
        .text_color(color.opacity(0.8))
}

/// Port of `session_canvas_server.py`'s staleness spirit ("quiet AND not
/// currently doing anything"), not its literal cursor-liveness mechanism —
/// see `STALE_AFTER`'s doc comment.
fn is_stale(chat: &Chat, status: ChatIndicator, now: chrono::DateTime<Utc>) -> bool {
    if matches!(status, ChatIndicator::Working | ChatIndicator::AwaitingInput) {
        return false;
    }
    let last_activity = chat.last_message_at.unwrap_or(chat.created_at);
    now.signed_duration_since(last_activity) > STALE_AFTER
}

/// Label + severity color for a PR action reason — exact `ACTION_META`
/// (`session_canvas.html:2206-2212`) labels and `--status-*` hex values.
fn pr_action_badge(action: PrAction) -> (&'static str, gpui::Hsla) {
    let critical: gpui::Hsla = gpui::rgb(0xd03b3b).into();
    match action {
        PrAction::CiFailing => ("CI failing", critical),
        PrAction::MergeConflict => ("Merge conflict", critical),
        PrAction::ChangesRequested => ("Changes requested", critical),
        PrAction::NeedsReviewer => ("Needs reviewer", gpui::rgb(0xfab219).into()),
        PrAction::ReadyToMerge => ("Ready to merge", gpui::rgb(0x0ca30c).into()),
    }
}

/// The main column's rendering mode. "My PRs" used to be a third mode that
/// replaced the whole body; it is now the independent left sidebar
/// ([`Overview::pr_sidebar_open`]) shown alongside either of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    List,
    Canvas,
}

/// A PR-row jump waiting for the layout it will be framed in. Opening the
/// chat panel narrows the canvas in the SAME frame the jump is requested,
/// but `canvas_bounds` is only captured at paint — so the focus waits
/// `settle_frames` painted frames before reading the viewport, or it would
/// center against the pre-panel width.
struct PendingFocus {
    chat_id: String,
    settle_frames: u8,
}

/// How long a fetched "my open PRs" list stays good before this view
/// refetches it — the pane's own poll cadence, independent of the server
/// cache's own TTL/sweep.
const MY_PRS_REFRESH: Duration = Duration::from_secs(30);

/// Logical (unzoomed, unpanned) tile position on the canvas.
#[derive(Debug, Clone, Copy)]
struct TilePos {
    x: f32,
    y: f32,
}

impl TilePos {
    /// Sub-pixel layout jitter (float noise from a repack that lands in the
    /// same place) must not count as a move worth animating.
    fn approx_eq(self, other: TilePos) -> bool {
        (self.x - other.x).abs() < 0.5 && (self.y - other.y).abs() < 0.5
    }
}

/// Web `.card { transition: left/top 0.55s ... }`.
const TILE_MOVE_DURATION: Duration = Duration::from_millis(550);

/// CSS `cubic-bezier(0.34, 1.56, 0.64, 1)` — the web card-move curve: an
/// ease-out that overshoots (y peaks ~1.1) before settling. Deliberately NOT
/// `motion::CubicBezier::eval`, which clamps its output to `[0,1]` (gpui's
/// animation element asserts that range) and would flatten the overshoot
/// away; this is only ever used as a position lerp factor, where >1 is the
/// point.
fn tile_move_ease(x: f32) -> f32 {
    const X1: f32 = 0.34;
    const Y1: f32 = 1.56;
    const X2: f32 = 0.64;
    const Y2: f32 = 1.0;
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    // Bernstein form with P0=(0,0), P3=(1,1).
    let bez = |p1: f32, p2: f32, t: f32| {
        let u = 1.0 - t;
        3.0 * u * u * t * p1 + 3.0 * u * t * t * p2 + t * t * t
    };
    // x(t) is monotonic (x1, x2 ∈ [0,1]) — plain bisection is exact enough
    // and branch-free of Newton's degenerate-derivative cases.
    let (mut lo, mut hi) = (0.0_f32, 1.0_f32);
    for _ in 0..24 {
        let mid = (lo + hi) * 0.5;
        if bez(X1, X2, mid) < x {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    bez(Y1, Y2, (lo + hi) * 0.5)
}

/// One tile's in-flight organize move, in LOGICAL board coords (so pan/zoom
/// mid-flight just transform it like any other position).
#[derive(Debug, Clone, Copy)]
struct TileMove {
    from: TilePos,
    to: TilePos,
    started: Instant,
}

impl TileMove {
    /// Displayed position at `now` (eased; may overshoot `to`), or `None`
    /// once the move has finished — the caller then renders `to` exactly and
    /// drops the move.
    fn sample(&self, now: Instant, duration: Duration) -> Option<TilePos> {
        let elapsed = now.saturating_duration_since(self.started).as_secs_f32();
        let raw = elapsed / duration.as_secs_f32().max(f32::EPSILON);
        if raw >= 1.0 {
            return None;
        }
        Some(lerp_pos(self.from, self.to, tile_move_ease(raw)))
    }
}

fn lerp_pos(from: TilePos, to: TilePos, t: f32) -> TilePos {
    TilePos {
        x: from.x + (to.x - from.x) * t,
        y: from.y + (to.y - from.y) * t,
    }
}

/// The animate-or-snap decision for one tile in one layout pass. Returns the
/// move to keep (or start) and the position to render this frame.
///
/// - `prev_target`: the position the last canvas pass assigned this tile.
///   `None` = first appearance (new chat, filter re-reveal, entering canvas
///   mode) → no animation, never fly in from anywhere.
/// - `live_drag`: the user is dragging this tile → its position is already
///   the cursor's; animating would make it lag the pointer. Drag release
///   produces no target change, so it doesn't animate either.
/// - Otherwise a target change (grouping applied/changed/removed, repack,
///   overlap repair) starts a move from wherever the tile is DISPLAYED right
///   now — mid-flight retargets continue from the current (possibly
///   overshooting) spot instead of jumping back to the old target.
fn plan_tile_motion(
    prev_target: Option<TilePos>,
    current: Option<TileMove>,
    target: TilePos,
    live_drag: bool,
    reduced_motion: bool,
    now: Instant,
    duration: Duration,
) -> (Option<TileMove>, TilePos) {
    let Some(prev) = prev_target else {
        return (None, target);
    };
    if live_drag || reduced_motion {
        return (None, target);
    }
    let displayed_now = current.and_then(|m| m.sample(now, duration));
    if prev.approx_eq(target) {
        // Unchanged target: keep playing an in-flight move toward it, if any.
        return match (current, displayed_now) {
            (Some(m), Some(pos)) if m.to.approx_eq(target) => (Some(m), pos),
            _ => (None, target),
        };
    }
    let from = displayed_now.unwrap_or(prev);
    let next = TileMove {
        from,
        to: target,
        started: now,
    };
    (Some(next), from)
}

/// In-flight tile press/drag state — see `Overview::dragging_tile`'s doc
/// comment for why this replaced a bare `Option<(String, (f32, f32))>`.
#[derive(Debug, Clone)]
struct TileDrag {
    chat_id: String,
    offset: (f32, f32),
    down_cursor: (f32, f32),
    committed: bool,
}

/// Exact port of `session_canvas.html`'s `DRAG_THRESHOLD_PX` (`~1196`).
/// Movement below this, measured in board/logical space (screen movement
/// divided by zoom, matching the reference's own `/zoom` scaling so a drag
/// feels 1:1 regardless of zoom level), is a click; at or above it, a drag.
const DRAG_THRESHOLD_PX: f32 = 4.0;

/// `renderTile(..., 160, 260)`'s `minPx`/`maxPx` (`session_canvas.html:
/// ~1851`): main-tile width at 0% and 100% context.
const TILE_MIN_W: f32 = 160.0;
const TILE_MAX_W: f32 = 260.0;
/// `div.style.minHeight = Math.round(size * 0.62)` (`~1128`).
const TILE_ASPECT: f32 = 0.62;
/// Height floor for the tile face's ALWAYS-present rows (title, cwd,
/// context bar + label, status row, open button/expand-hint footer). The
/// web's tile grows to fit its content and gets measured for packing; gpui
/// tiles here need a size known up front, so this is the content-fit floor
/// that both 0.62 × width ([`tile_size_for_pct`]) and the row's OWN variable
/// content ([`tile_content_height_estimate`]) sit on top of. At 160px
/// (0.62 × 160 = 99) the floor wins; at 260px (161) the aspect ratio wins.
/// Does NOT bake in badges/subagent tiles any more — those are only present
/// on some rows, so [`tile_content_height_estimate`] adds them on top of
/// this floor exactly when a row actually has them, instead of every tile
/// paying for space it may not use (or, the bug this replaces, every tile
/// NOT paying for space it needs).
const TILE_CONTENT_MIN_H: f32 = 140.0;
const TILE_GAP: f32 = 16.0;
/// `tile_body`'s own flex-column `gap(3)` between each direct child row —
/// shared here so [`tile_content_height_estimate`]'s per-optional-row
/// addition matches the real spacing instead of a duplicated magic number.
const TILE_ROW_GAP: f32 = 3.0;
/// Height `badges_for`'s PR/ticket badge row adds when present: an ~18px
/// badge-chip row (`py(1)` × 2 + ~9.5px text, rounded up) plus the
/// flex-column gap separating it from its neighbors in `tile_body`.
const BADGE_ROW_ADDED_H: f32 = 18.0 + TILE_ROW_GAP;
/// One compact nested subagent tile (`.tile.compact`,
/// `session_canvas.html:441-517`): `padding: 5px 7px` around a single
/// 10px/1.4 name line (14px) plus a 1px border top and bottom — 26px. Set as
/// an explicit height on the rendered tile (`subagent_tile`) so this
/// estimate and the real layout can never drift apart.
const SUBAGENT_TILE_H: f32 = 26.0;
/// `.subagents { gap: 6px }` — between adjacent compact subagent tiles.
const SUBAGENT_TILE_GAP: f32 = 6.0;
/// `.connector { margin-top: 10px }` — the space between the parent card's
/// own face and its subagent stack. `tile_body`'s flex-column gap already
/// supplies [`TILE_ROW_GAP`] of it; the stack adds the rest as a top margin.
const SUBAGENT_STACK_TOP: f32 = 10.0;
/// How many subagent tiles the flat canvas's fixed default-slot stride
/// ([`tile_max_size`] / [`flat_grid_stride`]) budgets for. Subagent tiles
/// are uncapped, so no fixed stride can absorb every tile (36 subagents is a
/// ~1.3k-px card); see [`tile_max_size`] for what happens past this bound.
const FLAT_STRIDE_SUBAGENT_TILES: usize = 4;

/// Port of `tileSize(pct, 160, 260)` (`session_canvas.html:1040-1043`):
/// `round(min + pct * (max - min))`, null → 0. Clamped to `0.0..=1.0` (and a
/// non-finite value treated as null) so a bad engine value can't produce a
/// negative or huge tile. The web doesn't clamp, but the engine promises
/// that range anyway.
fn tile_width_for_pct(pct: Option<f32>) -> f32 {
    let p = pct.filter(|p| p.is_finite()).unwrap_or(0.0).clamp(0.0, 1.0);
    (TILE_MIN_W + p * (TILE_MAX_W - TILE_MIN_W)).round()
}

/// `(width, height)` for a main tile at `pct`, from the context-%-driven
/// aspect ratio ALONE: width per [`tile_width_for_pct`], height
/// `round(width * 0.62)` floored at [`TILE_CONTENT_MIN_H`]. Doesn't know
/// about a row's own content (subagent tiles, a badge row) — see
/// `OverviewRow::tile_size`, the actual per-row size every layout consumer
/// uses, which takes the `max` of this and [`tile_content_height_estimate`].
fn tile_size_for_pct(pct: Option<f32>) -> (f32, f32) {
    let w = tile_width_for_pct(pct);
    (w, (w * TILE_ASPECT).round().max(TILE_CONTENT_MIN_H))
}

/// Deterministic tile CONTENT-height estimate — the packing-time stand-in
/// for the web reference's real per-card DOM measurement (`measureItem`),
/// since gpui tiles need a size known up front and there's no "measure this
/// offscreen" primitive to ask for one instead. [`TILE_CONTENT_MIN_H`]
/// already covers the tile face's rows every tile always renders; this adds
/// exactly what varies per row, matching what `tile_body` conditionally
/// renders:
/// - a PR/ticket badge row (`badges_for`), when `has_badge_row`
/// - the subagent stack (`subagent_tiles_for`): EVERY subagent as its own
///   fixed-height compact tile ([`SUBAGENT_TILE_H`], [`SUBAGENT_TILE_GAP`]
///   apart, [`SUBAGENT_STACK_TOP`] below the face) — uncapped, like the web
///   reference, so the card grows to fit all of them.
///
/// Monotonic in both `subagent_count` and `has_badge_row`: more content
/// never produces a smaller estimate. Must be kept in lockstep with
/// `tile_body`'s actual children — a new always-shown or conditionally-shown
/// row there needs a matching term here, or this silently under-estimates
/// again (the defect this function exists to fix in the first place).
fn tile_content_height_estimate(subagent_count: usize, has_badge_row: bool) -> f32 {
    let mut height = TILE_CONTENT_MIN_H;
    if has_badge_row {
        height += BADGE_ROW_ADDED_H;
    }
    height += subagent_stack_height(subagent_count);
    height
}

/// Height the subagent stack adds below the tile face (0 for none) — also
/// what `tile_body` reserves for it, so the two share one formula.
fn subagent_stack_height(count: usize) -> f32 {
    if count == 0 {
        return 0.0;
    }
    SUBAGENT_STACK_TOP
        + count as f32 * SUBAGENT_TILE_H
        + count.saturating_sub(1) as f32 * SUBAGENT_TILE_GAP
}

/// The flat grid's slot stride basis (see `position_for`): the largest
/// context-% width, and a height covering a badge row plus
/// [`FLAT_STRIDE_SUBAGENT_TILES`] subagent tiles.
///
/// Deliberately BOUNDED, not the true worst case: subagent tiles are
/// uncapped, so the true worst-case height is unbounded (36 subagents ≈
/// 1.3k px) and a stride sized for it would spread every ordinary tile
/// kilometres apart. A default-slotted flat-mode tile with more subagents
/// than the bound extends into the slot below it — the same tolerance the
/// web reference's own flat mode has (`defaultPosition`'s fixed 420px row
/// stride, no reflow). Flat mode has no overlap-repair pass (positions are
/// user-placed/persisted, and a repair would fight dragging), so the user
/// drags it clear; grouped mode is exact regardless, since `pack_rects` +
/// `repair_overlaps` pack each tile's real `tile_size`.
fn tile_max_size() -> (f32, f32) {
    let (w, aspect_h) = tile_size_for_pct(Some(1.0));
    let bounded_content_h = tile_content_height_estimate(FLAT_STRIDE_SUBAGENT_TILES, true);
    (w, aspect_h.max(bounded_content_h))
}

/// The fill bar's filled fraction: `Math.round((pct || 0) * 100)%`.
fn context_fill_fraction(pct: Option<f32>) -> f32 {
    context_pct_percent(pct).unwrap_or(0) as f32 / 100.0
}

/// Rounded whole percent, `None` for null. Everything this view renders
/// from the pct (width, fill bar, label) is determined by this value, so
/// it's also the refetch no-op key (see [`context_pct_changed`]).
fn context_pct_percent(pct: Option<f32>) -> Option<i32> {
    pct.filter(|p| p.is_finite())
        .map(|p| (p.clamp(0.0, 1.0) * 100.0).round() as i32)
}

/// `.pct-label`: `${pctLabel} context`, `pctLabel` = `N%` or `—` for null.
fn context_pct_label(pct: Option<f32>) -> String {
    match context_pct_percent(pct) {
        Some(n) => format!("{n}% context"),
        None => "— context".to_string(),
    }
}

/// Whether a landed context fetch changes anything visible — only then may
/// it dirty rows and drop the grouped layout. A first fetch that comes back
/// null is no change (the row already rendered as null), and sub-percent
/// drift on a live chat is no change either (see [`context_pct_percent`]).
fn context_pct_changed(previous: Option<Option<f32>>, next: Option<f32>) -> bool {
    context_pct_percent(previous.flatten()) != context_pct_percent(next)
}

/// Flat-canvas default grid slot stride: the max tile size plus the gap, so
/// no two default slots can overlap at any context %.
fn flat_grid_stride() -> (f32, f32) {
    let (w, h) = tile_max_size();
    (w + TILE_GAP, h + TILE_GAP)
}

/// The context bar + `N% context` label from `renderTile`'s tile face
/// (`.fill-bar` 4px/radius 2 on `--gridline` with an `--accent` fill,
/// `.pct-label` 10px muted tabular). Shared by canvas tiles and list rows.
fn context_meter(pct: Option<f32>, theme: &Theme, zoom: f32) -> gpui::Div {
    let accent: gpui::Hsla = gpui::rgb(0x3987e5).into(); // --accent
    div()
        .flex()
        .flex_col()
        .gap(px(2.0 * zoom))
        .child(
            div()
                .w_full()
                .h(px(4.0 * zoom))
                .rounded(px(2.0 * zoom))
                .overflow_hidden()
                .bg(theme.border)
                .child(
                    div()
                        .h_full()
                        .w(gpui::relative(context_fill_fraction(pct)))
                        .bg(accent),
                ),
        )
        .child(
            div()
                .text_size(crate::typography::ui_rems(10.0 * zoom))
                .text_color(theme.text_muted)
                .child(SharedString::from(context_pct_label(pct))),
        )
}
const GRID_COLS: usize = 4;
/// Matches `session_canvas.html`'s `MIN_ZOOM`/`MAX_ZOOM` exactly.
const ZOOM_MIN: f32 = 0.25;
const ZOOM_MAX: f32 = 2.0;
/// Grouped-canvas layout only (see `render_canvas_body`'s grouped branch).
const GROUP_LABEL_H: f32 = 30.0;
const GROUP_GAP: f32 = 36.0;
/// `GROUP_BOX_PADDING` (`session_canvas.html:1308-1319`): how far a group's
/// tinted box extends past its header + content.
const GROUP_BOX_PADDING: f32 = 10.0;

/// Width of the interactive chat panel. Wide enough that the embedded
/// composer's footer (pickers, send) isn't crushed — the composer itself
/// caps at `COMPOSER_MAX_WIDTH` (768).
const CHAT_PANEL_W: f32 = 540.0;
/// Horizontal inset of the composer inside the chat panel.
const CHAT_PANEL_COMPOSER_PAD: f32 = 10.0;
/// Width of the left "My PRs" sidebar (see [`Overview::pr_sidebar_open`]).
/// Fixed, like `CHAT_PANEL_W`; the list/canvas column between the two
/// panels is the only thing that flexes.
const PR_SIDEBAR_W: f32 = 340.0;
/// Entry in `overview_positions::UI_FLAGS_FILE` meaning "the My PRs sidebar
/// is open".
const PR_SIDEBAR_FLAG: &str = "prSidebarOpen";
/// The embedded transcript's top inset (`Transcript::set_top_inset`): the
/// panel's own header sits ABOVE the transcript rather than overlaying it,
/// so only a little breathing room replaces the chat route's titlebar inset.
pub(crate) const CHAT_PANEL_TRANSCRIPT_TOP_INSET: f32 = 8.0;

/// The chat the interactive panel shows: always the current selection, and
/// only while the panel is open. `None` while open means the selection went
/// away (deleted/archived/new-session) and the panel should close itself.
fn panel_chat_id(panel_open: bool, selected_chat: Option<&str>) -> Option<String> {
    if !panel_open {
        return None;
    }
    selected_chat.filter(|id| !id.is_empty()).map(str::to_string)
}

pub struct Overview {
    state: Entity<AppState>,
    shell: gpui::WeakEntity<Shell>,
    scroll: ScrollHandle,
    view_mode: ViewMode,
    /// Logical grid/drag positions, keyed by chat id. Populated lazily: a
    /// chat not yet here gets a default grid slot the first time canvas mode
    /// renders it.
    tile_positions: HashMap<String, TilePos>,
    canvas_pan: (f32, f32),
    canvas_zoom: f32,
    /// Armed on tile press, resolved on release. `offset`: cursor offset
    /// from the tile's logical top-left at press (used to compute a live
    /// drag position). `down_cursor`: window cursor at press, to measure
    /// cumulative movement. `committed`: whether movement has exceeded
    /// `DRAG_THRESHOLD_PX` — ports `session_canvas.html`'s own
    /// press/move/release click-vs-drag disambiguation (`makeCard`,
    /// `~1711-1761`: `moved < DRAG_THRESHOLD_PX` on release means "this was
    /// a click", not a drag). Previously this app set a single
    /// "dragging" flag unconditionally on press and tried to disambiguate
    /// via a separate `on_click` handler racing against the canvas root's
    /// `on_mouse_up` clearing that flag — a real, evidenced bug (the fix
    /// commit's message covers why): depending on gpui's dispatch order
    /// that race resolved differently call to call, which is exactly why
    /// clicking a tile was reported as unreliable ("sometimes it does
    /// bring up chat" — read: the race happened to clear the flag before
    /// the stale `on_click` fired). This single state machine, resolved
    /// only on release, removes the race entirely.
    dragging_tile: Option<TileDrag>,
    /// Last layout-assigned LOGICAL position per tile rendered by the most
    /// recent canvas pass (flat or grouped) — the "previous position" the
    /// organize animation moves from. Pruned to the tiles actually present
    /// each pass and cleared outside canvas mode, so a tile that (re)appears
    /// has no entry and never flies in.
    tile_targets: HashMap<String, TilePos>,
    /// In-flight organize moves (see `plan_tile_motion`). Purely a render-time
    /// position offset: never touches `rows_dirty` or the layout cache. While
    /// non-empty, `render` requests one more animation frame; once every move
    /// settles it's empty and nothing is scheduled — zero cost at rest.
    tile_moves: HashMap<String, TileMove>,
    /// Tiles currently showing their expanded detail (toggled by a real
    /// click — movement under `DRAG_THRESHOLD_PX` — matching the
    /// reference's `expandedIds`/`toggleExpanded`). Never causes navigation;
    /// only the explicit "Open chat" affordance does that.
    expanded: HashSet<String>,
    /// (cursor position at pan-start, pan offset at pan-start).
    panning: Option<((f32, f32), (f32, f32))>,
    link_status: HashMap<String, LinkStatusEntry>,
    link_status_pending: HashSet<String>,
    /// Subagents per chat id. Fetched once per chat (not refreshed on an
    /// interval like `link_status` — a session's own subagent set is
    /// settled once its transcript exists, unlike a PR's live checks/review
    /// state) and only for chats that came through the external-import flow
    /// (anything else has no transcript path to scan, per
    /// `SCAN_CHAT_SUBAGENTS`'s contract).
    subagents: HashMap<String, Vec<SubagentItem>>,
    subagent_pending: HashSet<String>,
    /// Last attempt time per chat (success OR failure), so a chat with
    /// nothing to scan retries on `SUBAGENTS_REFRESH` instead of on every
    /// row rebuild.
    subagents_fetched_at: HashMap<String, Instant>,
    /// Category/origin classification per chat id. Refetched on
    /// `CLASSIFICATION_REFRESH` (an imported chat can keep growing).
    classification: HashMap<String, ClassificationInfo>,
    classification_pending: HashSet<String>,
    classification_fetched_at: HashMap<String, Instant>,
    /// `CHAT_CONTEXT_USAGE` per chat id (`None` = engine had no usage).
    /// Refetched on `CONTEXT_USAGE_REFRESH`.
    context_usage: HashMap<String, Option<f32>>,
    context_usage_pending: HashSet<String>,
    context_usage_fetched_at: HashMap<String, Instant>,
    /// Chat ids whose "done" has been acknowledged (tile expanded / chat
    /// opened) — the web's `acknowledgedDone`, persisted via
    /// `overview_positions::ACKNOWLEDGED_DONE_FILE`.
    acknowledged_done: BTreeSet<String>,
    /// Repo keys the repo filter hides (web `hiddenRepos`), persisted via
    /// `overview_positions::HIDDEN_REPOS_FILE`.
    hidden_repos: BTreeSet<String>,
    /// Every repo key seen across rows before the repo/search/stale filters
    /// — the repo filter lists these so unchecking a repo never makes its
    /// own checkbox vanish (web: built from `lastSessions`).
    known_repos: BTreeSet<String>,
    /// Whether `acknowledged_done`/`hidden_repos` have been read from disk
    /// (lazily, once `AppState.data_dir` exists).
    prefs_loaded: bool,
    /// The Repos dropdown — the app's standard `Popup` lifecycle (exit
    /// animation + the trigger-press note that keeps a trigger click from
    /// close-then-reopening).
    repo_filter: popover::Popup<()>,
    legend_open: bool,
    /// Rows before any filter (archived-inclusion aside) — lets the empty
    /// state tell "no chats at all" from "everything filtered out".
    unfiltered_count: usize,
    /// Set by a search edit; the next canvas render auto-fits to the matches
    /// when there are 1..=`FIT_MAX_MATCHES` of them (web search handler).
    pending_fit_to_matches: bool,
    refresh_ticks: u32,
    _refresh_task: gpui::Task<()>,
    /// Composable grouping dimensions, in activation (click) order — the
    /// order they appear here IS the nesting order, matching the reference's
    /// `activeGroups` insertion-order semantics (`overview_grouping::partition`'s
    /// doc comment). Empty = flat/ungrouped, the default.
    active_groups: Vec<GroupDimension>,
    /// "Hide stale" toggle — default off, matches the reference tool's own
    /// `hideStale` default (`session_canvas.html`).
    hide_stale: bool,
    /// "Show archived" toggle — default off, matches the reference's own
    /// `showArchived` default. Archived tiles render heavily dimmed when on.
    show_archived: bool,
    my_prs: Vec<MyPrItem>,
    my_prs_pending: bool,
    my_prs_fetched_at: Option<Instant>,
    /// Left "My PRs" sidebar (toolbar toggle). Rendered as the overview's
    /// leftmost fixed-width column — `[sidebar | list/canvas | chat panel]`
    /// — so the canvas simply measures narrower (its `canvas_bounds` come
    /// from paint, so culling/fit/zoom math follow automatically). The
    /// toolbar search filters its rows too ([`pr_matches_search`]).
    /// Persisted via `overview_positions::UI_FLAGS_FILE`.
    pr_sidebar_open: bool,
    /// The sidebar's own scroll (it is visible alongside the list, which
    /// owns `scroll`).
    pr_sidebar_scroll: ScrollHandle,
    /// See [`PendingFocus`].
    pending_focus: Option<PendingFocus>,
    /// The chat a canvas/list pass should bring into view THIS frame —
    /// set in `render` once `pending_focus` has settled, consumed by the
    /// body renderer, and cleared after it either way.
    focus_due: Option<String>,
    /// The Canvas container's own on-screen bounds, captured each paint via
    /// a `gpui::canvas` overlay (`mouse events report `position` in WINDOW
    /// coordinates, not element-local — this is what converts a scroll/click
    /// event's cursor into canvas-local coordinates for zoom-to-cursor math).
    /// `None` until the first paint.
    canvas_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// Whether `tile_positions` has had persisted positions merged in yet —
    /// loaded lazily on flat canvas mode's first render (not in `new`, which
    /// has no `cx` to read `AppState.data_dir` through, and `data_dir` may
    /// still be `None` immediately after construction, before engine
    /// bootstrap completes).
    positions_loaded: bool,
    /// Set whenever something that could change `rows()`'s OUTPUT happens
    /// (the underlying chat list, `hide_stale`/`show_archived`, or a
    /// link-status/subagent/classification/context-usage fetch landing with
    /// changed data) — `false` the rest
    /// of the time, including every pan/zoom/drag frame. `rows()` skips its
    /// expensive rebuild (walking `AppState`, cloning every `Chat`,
    /// re-deriving staleness) on a `false` read and returns the cache
    /// instead — this was the primary reported cause of "slow / low
    /// framerate" during canvas interaction: pan/zoom/drag call `cx.notify()`
    /// on every single pointer-move, and without this flag `rows()` (and, in
    /// grouped mode, a full rectangle-repack) reran from scratch on every one
    /// of those frames regardless of whether the row set had actually
    /// changed.
    rows_dirty: bool,
    rows_cache: Vec<OverviewRow>,
    /// Grouped-canvas layout (packing + the global overlap-repair pass) is
    /// genuinely expensive — real rectangle-packing plus a second full sweep
    /// across every tile — and its OUTPUT depends only on which chat ids are
    /// present, their tile sizes, and `active_groups`, never on
    /// `canvas_pan`/`canvas_zoom` (those apply as a pure `to_screen`
    /// transform at paint time, over already-computed LOGICAL positions).
    /// Cached here, keyed by `(row ids, tile widths, active_groups)`;
    /// pan/zoom/drag frames reuse it
    /// untouched.
    grouped_layout_cache: Option<GroupedLayoutCache>,
    /// Filters `rows()` by title/cwd/preview substring (case-insensitive) —
    /// there was no way at all to search/filter across a real 227+-chat
    /// grid before this. Same `ComposerInput` pattern `pickers.rs` already
    /// uses for its own search boxes.
    search: Entity<ComposerInput>,
    /// "View full chat →" opens an interactive chat panel alongside the
    /// overview instead of navigating away to `Route::Chat` — the reference
    /// tool's own `openTranscriptPanel` shape (an in-page panel, never
    /// leaving the canvas), but live: it SELECTS the chat
    /// (`Shell::preview_chat_in_overview`, route untouched) and renders the
    /// Shell's own `transcript`/`composer` entities, so sending, queueing,
    /// the question wizard, interrupt, attachments and resume all work
    /// exactly as on the chat route (they key off `AppState.selected_chat`,
    /// not the route). The chat shown is always `selected_chat` — no id is
    /// stored here, so a sidebar click while the panel is open retargets it.
    ///
    /// Invariant: those entities must never render in two places in one
    /// frame. This view only renders on `Route::Overview`, and the Shell's
    /// chat stack only renders on `Route::Chat` (`render_main`'s early
    /// return), so the routes keep them exclusive.
    chat_panel_open: bool,
    /// The Shell's own conversation transcript (see `chat_panel_open`).
    transcript: Entity<Transcript>,
    /// The Shell's own composer (see `chat_panel_open`).
    composer: Entity<crate::composer::Composer>,
    /// The chat the panel shows this frame (`panel_chat_id`), snapshotted at
    /// the top of `render` so `tile_body`/`render_list_row` can outline it
    /// without a `cx`.
    panel_highlight: Option<String>,
    /// The chat whose expanded detail shows the inline "Link PR / ticket…"
    /// input (at most one at a time — `link_input` is a single entity and
    /// must render in one place per frame).
    link_editor_chat: Option<String>,
    /// The link editor's own input (`PALETTE_SEARCH_CONTEXT`, so editing
    /// keys work and Enter/Escape bubble to the wrapper's key handler).
    link_input: Entity<ComposerInput>,
    /// A `SET_CHAT_LINK` from the editor is in flight (ignore repeat Enter).
    link_submitting: bool,
    /// `(chat_id, message)`: parse or RPC error to show in that chat's
    /// expanded detail (editor or unlink).
    link_error: Option<(String, String)>,
    /// The right-side panel's read-only subagent-transcript mode (see
    /// [`SubagentPeek`]). Mutually exclusive with `chat_panel_open`: opening
    /// either replaces the other (`open_subagent_peek`/`open_chat_panel`),
    /// and closing a peek returns to the bare canvas/list, never to a chat
    /// panel it replaced.
    subagent_peek: Option<SubagentPeek>,
    /// Monotonic id for peek fetches — web `transcriptRequestId`.
    peek_request_seq: u64,
    /// The peek body's scroll, pinned to the bottom when its turns land.
    peek_scroll: ScrollHandle,
    /// Focused when a peek opens, so Escape reaches the panel's key handler
    /// wherever the pointer is.
    peek_focus: gpui::FocusHandle,
    _observe: Subscription,
    _search_events: Subscription,
    _link_input_events: Subscription,
}

/// See [`Overview::grouped_layout_cache`].
struct GroupedLayoutCache {
    key_ids: Vec<String>,
    /// Each row's full tile `(width, height)`, parallel to `key_ids`. Used to
    /// be width-only ("width alone determines height") back when height was
    /// purely `TILE_ASPECT` × width — no longer true now that a row's real
    /// content (subagent count, a badge row) can change its height with its
    /// width unchanged (see `OverviewRow::tile_size`), so the full size has
    /// to be the key or a content-only change would silently reuse a
    /// too-short cached layout.
    key_sizes: Vec<(f32, f32)>,
    key_groups: Vec<GroupDimension>,
    laid_out: LaidOutPartition,
    repaired: HashMap<String, overview_layout::PackedPosition>,
}

impl Overview {
    pub fn new(
        state: Entity<AppState>,
        shell: gpui::WeakEntity<Shell>,
        transcript: Entity<Transcript>,
        composer: Entity<crate::composer::Composer>,
        cx: &mut Context<Self>,
    ) -> Self {
        let observe = cx.observe(&state, |this, _, cx| {
            this.rows_dirty = true;
            this.grouped_layout_cache = None;
            cx.notify();
        });
        let search = cx.new(|cx| {
            // Must be `PALETTE_SEARCH_CONTEXT`: editing keys (backspace, ⌥⌫, ⌘⌫,
            // ⌘A/C/V/Z…) are bound only to the contexts `input_bindings`
            // registers; a bespoke context got typing but no deletion.
            ComposerInput::with_context("Search chats…", crate::composer::PALETTE_SEARCH_CONTEXT, cx)
                .with_accessibility_role(gpui::Role::SearchInput)
        });
        let search_events = cx.subscribe(&search, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Edited) {
                this.rows_dirty = true;
                this.grouped_layout_cache = None;
                this.pending_fit_to_matches = true;
                cx.notify();
            }
        });
        let link_input = cx.new(|cx| {
            ComposerInput::with_context(
                "GitHub PR URL or ticket id (ENG-1234)",
                crate::composer::PALETTE_SEARCH_CONTEXT,
                cx,
            )
            .with_single_line()
        });
        let link_input_events = cx.subscribe(&link_input, |this: &mut Self, _, event, cx| {
            // A fresh edit clears a stale parse/RPC error for that editor.
            if matches!(event, ComposerInputEvent::Edited)
                && this.link_error.is_some()
                && this.link_error.as_ref().map(|(id, _)| id) == this.link_editor_chat.as_ref()
            {
                this.link_error = None;
                cx.notify();
            }
        });
        // Idle refresh — same `cx.spawn` + `background_executor().timer`
        // loop idiom as the composer's queue-lease renewal. Ends on its own
        // once the entity is dropped (`update` fails).
        let refresh_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(IDLE_REFRESH).await;
                if this.update(cx, |this, cx| this.refresh_tick(cx)).is_err() {
                    break;
                }
            }
        });
        Self {
            state,
            shell,
            scroll: ScrollHandle::new(),
            view_mode: ViewMode::List,
            tile_positions: HashMap::new(),
            canvas_pan: (0.0, 0.0),
            canvas_zoom: 1.0,
            dragging_tile: None,
            tile_targets: HashMap::new(),
            tile_moves: HashMap::new(),
            expanded: HashSet::new(),
            panning: None,
            link_status: HashMap::new(),
            link_status_pending: HashSet::new(),
            subagents: HashMap::new(),
            subagent_pending: HashSet::new(),
            subagents_fetched_at: HashMap::new(),
            classification: HashMap::new(),
            classification_pending: HashSet::new(),
            classification_fetched_at: HashMap::new(),
            context_usage: HashMap::new(),
            context_usage_pending: HashSet::new(),
            context_usage_fetched_at: HashMap::new(),
            acknowledged_done: BTreeSet::new(),
            hidden_repos: BTreeSet::new(),
            known_repos: BTreeSet::new(),
            prefs_loaded: false,
            repo_filter: popover::Popup::default(),
            legend_open: false,
            unfiltered_count: 0,
            pending_fit_to_matches: false,
            refresh_ticks: 0,
            _refresh_task: refresh_task,
            active_groups: Vec::new(),
            hide_stale: false,
            show_archived: false,
            my_prs: Vec::new(),
            my_prs_pending: false,
            my_prs_fetched_at: None,
            pr_sidebar_open: false,
            pr_sidebar_scroll: ScrollHandle::new(),
            pending_focus: None,
            focus_due: None,
            canvas_bounds: Rc::new(Cell::new(None)),
            positions_loaded: false,
            rows_dirty: true,
            rows_cache: Vec::new(),
            grouped_layout_cache: None,
            search,
            chat_panel_open: false,
            transcript,
            composer,
            panel_highlight: None,
            link_editor_chat: None,
            link_input,
            link_submitting: false,
            link_error: None,
            subagent_peek: None,
            peek_request_seq: 0,
            peek_scroll: ScrollHandle::new(),
            peek_focus: cx.focus_handle(),
            _observe: observe,
            _search_events: search_events,
            _link_input_events: link_input_events,
        }
    }

    /// Click-to-expand (tile or list row). Expanding also acknowledges an
    /// unread done — web `toggleExpanded` → `acknowledgeDone`.
    fn toggle_expanded(&mut self, chat_id: &str, cx: &Context<Self>) {
        if !self.expanded.remove(chat_id) {
            self.expanded.insert(chat_id.to_string());
            self.acknowledge_done(chat_id, cx);
        }
    }

    /// Web `acknowledgeDone`: only meaningful while the chat is actually
    /// done — acknowledging a non-done chat would be reconciled straight
    /// back out on the next row rebuild anyway, so it's skipped up front.
    fn acknowledge_done(&mut self, chat_id: &str, cx: &Context<Self>) {
        let is_done = self
            .rows_cache
            .iter()
            .any(|r| r.chat.id == chat_id && r.status == ChatIndicator::Completed);
        if is_done && self.acknowledged_done.insert(chat_id.to_string()) {
            self.save_string_set(overview_positions::ACKNOWLEDGED_DONE_FILE, &self.acknowledged_done, cx);
        }
    }

    /// Read the persisted acknowledged/hidden-repo sets once `data_dir`
    /// exists (it's `None` until engine bootstrap finishes — retried each
    /// render until then, unlike positions' one-shot flag).
    fn ensure_prefs_loaded(&mut self, cx: &mut Context<Self>) {
        if self.prefs_loaded {
            return;
        }
        let Some(data_dir) = self.state.read(cx).data_dir.clone() else {
            return;
        };
        self.prefs_loaded = true;
        let flags = overview_positions::load_string_set(&data_dir, overview_positions::UI_FLAGS_FILE);
        // Only ever turns it ON: a toggle before `data_dir` existed (engine
        // still bootstrapping) must not be overwritten by the older file.
        if pr_sidebar_open_from_flags(&flags) && !self.pr_sidebar_open {
            self.set_pr_sidebar_open(true, false, cx);
        }
        self.acknowledged_done
            .extend(overview_positions::load_string_set(&data_dir, overview_positions::ACKNOWLEDGED_DONE_FILE));
        self.hidden_repos
            .extend(overview_positions::load_string_set(&data_dir, overview_positions::HIDDEN_REPOS_FILE));
        if !self.hidden_repos.is_empty() {
            self.rows_dirty = true;
            self.grouped_layout_cache = None;
        }
    }

    fn save_string_set(&self, file_name: &str, set: &BTreeSet<String>, cx: &Context<Self>) {
        let Some(data_dir) = self.state.read(cx).data_dir.clone() else {
            return;
        };
        if let Err(err) = overview_positions::save_string_set(&data_dir, file_name, set) {
            tracing::warn!(error = %err, file = file_name, "saving overview state failed");
        }
    }

    /// Open/close the left My PRs sidebar. `persist`: write the UI-flag file
    /// (false only when applying the value just loaded from it). The search
    /// placeholder advertises that the query now reaches PRs too.
    fn set_pr_sidebar_open(&mut self, open: bool, persist: bool, cx: &mut Context<Self>) {
        self.pr_sidebar_open = open;
        let placeholder = if open { "Search chats & PRs…" } else { "Search chats…" };
        self.search
            .update(cx, |search, cx| search.set_placeholder(placeholder, cx));
        if open {
            self.ensure_my_prs(cx);
        }
        if persist && let Some(data_dir) = self.state.read(cx).data_dir.clone() {
            // Read-modify-write so any other flag in the file survives.
            let mut flags = overview_positions::load_string_set(&data_dir, overview_positions::UI_FLAGS_FILE);
            if open {
                flags.insert(PR_SIDEBAR_FLAG.to_string());
            } else {
                flags.remove(PR_SIDEBAR_FLAG);
            }
            self.save_string_set(overview_positions::UI_FLAGS_FILE, &flags, cx);
        }
        cx.notify();
    }

    /// A PR row's jump: open the chat in the right-hand panel (which also
    /// outlines its tile/row via `panel_highlight`) and bring it into view
    /// in whichever body mode is showing — see [`PendingFocus`] for why the
    /// framing waits a painted frame.
    fn jump_to_chat_from_pr(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.open_chat_panel(chat_id.clone(), cx);
        self.pending_focus = Some(PendingFocus { chat_id, settle_frames: 1 });
        cx.notify();
    }

    /// Any repo-filter change: persist, and re-derive rows + grouped layout
    /// (web `reflowLayout`).
    fn repo_filter_changed(&mut self, cx: &mut Context<Self>) {
        self.save_string_set(overview_positions::HIDDEN_REPOS_FILE, &self.hidden_repos, cx);
        self.rows_dirty = true;
        self.grouped_layout_cache = None;
        cx.notify();
    }

    /// One idle-refresh tick (see [`IDLE_REFRESH`]). Skipped entirely while
    /// the overview entity is alive but another route is on screen. Re-runs
    /// every visible row's TTL-gated fetches (no-ops when fresh) and the My
    /// PRs fetch (the toolbar badge needs it in every view), then repaints
    /// for relative times. Does NOT mark rows dirty (except the ~60s
    /// staleness re-derive) — a fetch that lands with changed data does that
    /// itself.
    fn refresh_tick(&mut self, cx: &mut Context<Self>) {
        let visible = self
            .shell
            .upgrade()
            .is_some_and(|shell| shell.read(cx).is_overview_route());
        if !visible {
            return;
        }
        self.refresh_ticks = self.refresh_ticks.wrapping_add(1);
        let targets: Vec<(String, bool)> = self
            .rows_cache
            .iter()
            .map(|r| (r.chat.id.clone(), r.chat.harness_session_id.is_some()))
            .collect();
        for (chat_id, imported) in targets {
            self.ensure_row_fetches(chat_id, imported, cx);
        }
        self.ensure_my_prs(cx);
        if self.refresh_ticks.is_multiple_of(STALENESS_REDERIVE_EVERY_TICKS) {
            self.rows_dirty = true;
        }
        cx.notify();
    }

    /// Every per-row TTL-gated lookup, in one place so `rows()` and
    /// `refresh_tick` can't drift. Subagents/classification only apply to
    /// chats with a harness session (anything else has no transcript to
    /// scan, per `SCAN_CHAT_SUBAGENTS`/`CHAT_CLASSIFICATION`'s contract).
    /// Context usage applies to every chat: its live source is the engine's
    /// own doc store, not an import cursor.
    fn ensure_row_fetches(&mut self, chat_id: String, has_harness_session: bool, cx: &mut Context<Self>) {
        if has_harness_session {
            self.ensure_subagents(chat_id.clone(), cx);
            self.ensure_classification(chat_id.clone(), cx);
        }
        self.ensure_context_usage(chat_id.clone(), cx);
        self.ensure_link_status(chat_id, cx);
    }

    fn toggle_group(&mut self, dim: GroupDimension) {
        if let Some(ix) = self.active_groups.iter().position(|&d| d == dim) {
            self.active_groups.remove(ix);
        } else {
            self.active_groups.push(dim);
        }
        // Cache key includes `active_groups`, so this alone would naturally
        // miss on the next read anyway — dropped explicitly for clarity.
        self.grouped_layout_cache = None;
    }

    /// The current rows AND, as a side effect, kicks off any missing/stale
    /// PR-ticket-link and subagent lookups for what's visible right now.
    /// Combined into one pass rather than a separate "ensure" sweep so a
    /// freshly-appeared chat (new import, new sidebar row) gets its lookups
    /// queued the same render it first appears in, not one frame later.
    fn rows(&mut self, cx: &mut Context<Self>) -> Vec<OverviewRow> {
        // See `rows_dirty`'s doc comment: pan/zoom/drag call `cx.notify()`
        // (hence `render()`, hence this) on every pointer-move, and without
        // this short-circuit every one of those frames re-walked `AppState`,
        // cloned every `Chat`, and re-derived staleness for all 227+ rows —
        // the confirmed primary cause of the reported sluggishness. A stale
        // cache is bounded by whatever next marks `rows_dirty` (any real
        // `AppState` change already does, via the `cx.observe` in `new`, so
        // this isn't "never refreshes" — it's "doesn't redundantly rebuild
        // on frames where nothing relevant changed").
        if !self.rows_dirty {
            return self.rows_cache.clone();
        }

        let now = Utc::now();
        let raw: Vec<(ChatIndicator, Chat)> = self
            .state
            .read(cx)
            .overview_chats_with_archived(now, self.show_archived)
            .into_iter()
            .map(|(status, chat)| (status, chat.clone()))
            .collect();
        self.unfiltered_count = raw.len();
        let hide_stale = self.hide_stale;
        let query = self.search.read(cx).text().trim().to_lowercase();
        let mut known_repos: BTreeSet<String> = BTreeSet::new();
        let mut acknowledged_changed = false;
        let mut all: Vec<OverviewRow> = Vec::with_capacity(raw.len());
        for (status, chat) in raw {
            let stale = is_stale(&chat, status, now);
            self.ensure_row_fetches(chat.id.clone(), chat.harness_session_id.is_some(), cx);
            let link = self.link_status.get(&chat.id).map(|e| &e.status);
            let ticket = link
                .and_then(|s| s.ticket.as_ref())
                .map(|t| t.identifier.clone());
            let pr_number = link.and_then(|s| s.pr.as_ref()).map(|p| p.number);
            let pr_title = link.and_then(|s| s.pr.as_ref()).and_then(|p| p.title.clone());
            // Feeds `OverviewRow::tile_size` — see `has_badge_row`/
            // `subagent_count`'s own doc comments for why these are copied
            // in here rather than read off `self` inside `tile_size`.
            let has_badge_row = link.is_some_and(link_has_badge_row);
            let subagent_count = self.subagents.get(&chat.id).map_or(0, Vec::len);
            let (category, origin) = self
                .classification
                .get(&chat.id)
                .map(|info| (Some(info.category.clone()), Some(info.origin.clone())))
                .unwrap_or((None, None));
            let context_pct = self.context_usage.get(&chat.id).copied().flatten();
            acknowledged_changed |= reconcile_acknowledged(
                &mut self.acknowledged_done,
                &chat.id,
                status == ChatIndicator::Completed,
            );
            let repo = repo_key(chat.cwd.as_deref());
            known_repos.insert(repo.clone());
            all.push(OverviewRow {
                status,
                chat,
                stale,
                ticket,
                category,
                origin,
                repo,
                pr_number,
                pr_title,
                context_pct,
                subagent_count,
                has_badge_row,
            });
        }
        self.known_repos = known_repos;
        if acknowledged_changed {
            self.save_string_set(overview_positions::ACKNOWLEDGED_DONE_FILE, &self.acknowledged_done, cx);
        }
        let hidden_repos = &self.hidden_repos;
        let rows: Vec<OverviewRow> = all
            .into_iter()
            .filter(|row| !(hide_stale && row.stale))
            .filter(|row| !hidden_repos.contains(&row.repo))
            .filter(|row| {
                // Web `sessionMatchesSearch` fields that exist here: name
                // (title), branch, last message (preview), cwd, ticket id,
                // PR title. Zeron has no separate `summary` field.
                matches_search(
                    &query,
                    &[
                        row.chat.title.as_deref(),
                        row.chat.branch.as_deref(),
                        row.chat.last_message_preview.as_deref(),
                        row.chat.cwd.as_deref(),
                        row.ticket.as_deref(),
                        row.pr_title.as_deref(),
                    ],
                )
            })
            .collect();
        self.rows_cache = rows.clone();
        self.rows_dirty = false;
        rows
    }

    fn open(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.acknowledge_done(&chat_id, cx);
        let _ = self.shell.update(cx, |shell, cx| {
            shell.open_chat(chat_id, cx);
        });
    }

    /// Open the interactive chat panel for `chat_id` alongside the overview,
    /// instead of navigating away — see `chat_panel_open`'s doc comment.
    /// Selecting the chat is what wires the Shell's transcript/composer to
    /// it; re-clicking the already-shown chat just refocuses the composer.
    fn open_chat_panel(&mut self, chat_id: String, cx: &mut Context<Self>) {
        // Web `openTranscriptPanel` → `acknowledgeDone`.
        self.acknowledge_done(&chat_id, cx);
        let _ = self.shell.update(cx, |shell, cx| {
            shell.preview_chat_in_overview(chat_id, cx);
        });
        // The live panel replaces a subagent peek (one right-side panel).
        self.subagent_peek = None;
        self.chat_panel_open = true;
        cx.notify();
    }

    /// Clicking a compact subagent tile: open the right-side panel in its
    /// read-only transcript mode (web `openTranscriptPanel(session.id,
    /// sub.id, title)`), replacing whatever the panel showed — including the
    /// live chat panel, which is closed, not stacked underneath. Issues a
    /// fresh `READ_SUBAGENT_TRANSCRIPT` under a new request id every time
    /// (re-clicking the same subagent reloads it), so a slower earlier reply
    /// can never overwrite a newer peek ([`peek_reply_is_current`]).
    fn open_subagent_peek(&mut self, chat_id: String, sub: &SubagentItem, window: &mut Window, cx: &mut Context<Self>) {
        self.peek_request_seq += 1;
        let request_id = self.peek_request_seq;
        self.chat_panel_open = false;
        self.subagent_peek = Some(SubagentPeek {
            chat_id: chat_id.clone(),
            agent_id: sub.agent_id.clone(),
            name: sub.display_name(),
            agent_type: sub.agent_type.clone(),
            request_id,
            load: PeekLoad::Loading,
            open_tools: HashSet::new(),
        });
        window.focus(&self.peek_focus, cx);
        cx.notify();
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.apply_peek_reply(request_id, None, cx);
            return;
        };
        let agent_id = sub.agent_id.clone();
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::READ_SUBAGENT_TRANSCRIPT,
                    serde_json::json!({ "chatId": chat_id, "agentId": agent_id }),
                )
                .await;
            let decoded = result
                .ok()
                .and_then(|value| serde_json::from_value::<SubagentTranscript>(value).ok());
            this.update(cx, |this, cx| this.apply_peek_reply(request_id, decoded, cx))
                .ok();
        })
        .detach();
    }

    /// Land a peek fetch (`None` = RPC or decode failure) — dropped unless
    /// it's still the current peek's request.
    fn apply_peek_reply(&mut self, request_id: u64, reply: Option<SubagentTranscript>, cx: &mut Context<Self>) {
        if !peek_reply_is_current(self.subagent_peek.as_ref(), request_id) {
            return;
        }
        let Some(peek) = self.subagent_peek.as_mut() else {
            return;
        };
        peek.load = match reply {
            Some(transcript) => PeekLoad::Loaded(transcript),
            None => PeekLoad::Failed,
        };
        // Web `renderTurns` → `scrollTop = scrollHeight`: open at the end.
        self.peek_scroll.scroll_to_bottom();
        cx.notify();
    }

    /// ✕ / Escape on a peek: back to the bare canvas/list.
    fn close_subagent_peek(&mut self, cx: &mut Context<Self>) {
        if self.subagent_peek.take().is_some() {
            cx.notify();
        }
    }

    /// Hide the panel. The chat stays selected (like any sidebar pick) —
    /// deselecting would drop the user back onto the blank new-session
    /// canvas the next time they visit the chat route.
    fn close_chat_panel(&mut self, cx: &mut Context<Self>) {
        if self.chat_panel_open {
            self.chat_panel_open = false;
            cx.notify();
        }
    }

    /// The chat the panel currently shows — `Some` only while it's open and
    /// something is selected (see [`panel_chat_id`]).
    fn panel_chat_id(&self, cx: &Context<Self>) -> Option<String> {
        panel_chat_id(self.chat_panel_open, self.state.read(cx).selected_chat.as_deref())
    }

    fn set_archived(&mut self, chat_id: String, archived: bool, cx: &mut Context<Self>) {
        let _ = self.shell.update(cx, |shell, cx| {
            shell.set_chat_archived(chat_id, archived, cx);
        });
    }

    fn ensure_link_status(&mut self, chat_id: String, cx: &mut Context<Self>) {
        let fresh_enough = self
            .link_status
            .get(&chat_id)
            .is_some_and(|e| e.fetched_at.elapsed() < LINK_STATUS_REFRESH);
        if fresh_enough || self.link_status_pending.contains(&chat_id) {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.link_status_pending.insert(chat_id.clone());
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::CHAT_LINK_STATUS,
                    serde_json::json!({ "chatId": chat_id }),
                )
                .await;
            this.update(cx, |this, cx| {
                this.link_status_pending.remove(&chat_id);
                if let Ok(value) = result
                    && let Ok(status) = serde_json::from_value::<ChatLinkStatus>(value.clone())
                {
                    let diff_stat = value
                        .get("diffStat")
                        .and_then(|v| serde_json::from_value::<DiffStat>(v.clone()).ok());
                    let is_worktree = value.get("isWorktree").and_then(|v| v.as_bool());
                    // `prSource`/`ticketSource` ride the `ChatLinkStatus`
                    // decode itself (both `Option`, so an older engine that
                    // omits them reads as "inferred"). Merged over the
                    // previous entry so a just-set manual link's placeholder
                    // survives the engine's detail-cache miss.
                    let status = merge_refetched_link(
                        this.link_status.get(&chat_id).map(|e| &e.status),
                        status,
                    );
                    let fingerprint = link_fingerprint(&status);
                    let previous = this
                        .link_status
                        .insert(
                            chat_id,
                            LinkStatusEntry {
                                status,
                                diff_stat,
                                is_worktree,
                                fetched_at: Instant::now(),
                            },
                        )
                        .map(|old| link_fingerprint(&old.status));
                    // `rows()`'s cache bakes ticket/PR number/PR title in
                    // (grouping key + search fields) — only a CHANGE to
                    // those invalidates rows + the grouped layout; a
                    // same-data TTL refetch just repaints (badges/detail
                    // read `link_status` live).
                    if previous.as_ref() != Some(&fingerprint) {
                        this.rows_dirty = true;
                        this.grouped_layout_cache = None;
                    }
                }
                // Fetch failure: the stale entry (if any) keeps rendering;
                // its expired `fetched_at` retries on the next tick.
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Open the inline "Link PR / ticket…" editor in `chat_id`'s expanded
    /// detail (expanding it if needed), focused and empty. Also the entry
    /// point for the sidebar chat menu's "Link PR / ticket…", which switches
    /// to the overview first; `reveal` then brings the row/tile into view
    /// the same way a PR-sidebar jump does.
    pub(crate) fn open_link_editor(
        &mut self,
        chat_id: String,
        reveal: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.expanded.insert(chat_id.clone()) {
            self.acknowledge_done(&chat_id, cx);
        }
        self.link_editor_chat = Some(chat_id.clone());
        self.link_submitting = false;
        self.link_error = None;
        self.link_input.update(cx, |input, cx| input.set_text("", cx));
        window.focus(&gpui::Focusable::focus_handle(self.link_input.read(cx), cx), cx);
        if reveal {
            self.pending_focus = Some(PendingFocus { chat_id, settle_frames: 1 });
        }
        cx.notify();
    }

    fn close_link_editor(&mut self, cx: &mut Context<Self>) {
        if let Some(chat_id) = self.link_editor_chat.take() {
            if self.link_error.as_ref().is_some_and(|(id, _)| *id == chat_id) {
                self.link_error = None;
            }
            self.link_submitting = false;
            cx.notify();
        }
    }

    /// Enter in the link editor: parse ([`parse_link_input`]) and, if it's a
    /// PR URL or ticket id, write it via `SET_CHAT_LINK`; otherwise show the
    /// parse error inline and keep the editor open.
    fn submit_link_editor(&mut self, cx: &mut Context<Self>) {
        let Some(chat_id) = self.link_editor_chat.clone() else {
            return;
        };
        if self.link_submitting {
            return;
        }
        match parse_link_input(self.link_input.read(cx).text()) {
            Err(message) => {
                self.link_error = Some((chat_id, message.to_string()));
                cx.notify();
            }
            Ok(parsed) => {
                self.link_submitting = true;
                self.link_error = None;
                self.set_chat_link(chat_id, parsed.kind, Some(parsed.value), cx);
            }
        }
    }

    /// `SET_CHAT_LINK` (always a manual write; `value: None` unlinks). On
    /// success: optimistic local update ([`apply_link_reply`]), rows/layout
    /// invalidated (ticket/PR feed grouping, search and the badge row), the
    /// entry's TTL expired and an immediate `CHAT_LINK_STATUS` refetch
    /// kicked (which [`merge_refetched_link`] keeps from blanking the
    /// placeholder). Closes the editor if it was this chat's. On failure:
    /// inline error in the chat's detail.
    fn set_chat_link(&mut self, chat_id: String, kind: LinkKind, value: Option<String>, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.link_submitting = false;
            self.link_error = Some((chat_id, "Engine not connected".to_string()));
            cx.notify();
            return;
        };
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::SET_CHAT_LINK,
                    serde_json::json!({ "chatId": chat_id, "kind": kind.wire(), "value": value }),
                )
                .await;
            this.update(cx, |this, cx| {
                let editor_was_this_chat = this.link_editor_chat.as_deref() == Some(chat_id.as_str());
                if editor_was_this_chat {
                    this.link_submitting = false;
                }
                let reply = result.map_err(|err| err.to_string()).and_then(|value| {
                    serde_json::from_value::<SetChatLinkReply>(value).map_err(|err| err.to_string())
                });
                match reply {
                    Ok(reply) => {
                        let expired = Instant::now()
                            .checked_sub(LINK_STATUS_REFRESH)
                            .unwrap_or_else(Instant::now);
                        let entry = this.link_status.entry(chat_id.clone()).or_insert_with(|| LinkStatusEntry {
                            status: ChatLinkStatus::default(),
                            diff_stat: None,
                            is_worktree: None,
                            fetched_at: expired,
                        });
                        apply_link_reply(&mut entry.status, kind, &reply);
                        entry.fetched_at = expired;
                        this.rows_dirty = true;
                        this.grouped_layout_cache = None;
                        if this.link_error.as_ref().is_some_and(|(id, _)| *id == chat_id) {
                            this.link_error = None;
                        }
                        if editor_was_this_chat && value.is_some() {
                            this.link_editor_chat = None;
                        }
                        this.ensure_link_status(chat_id, cx);
                    }
                    Err(err) => {
                        tracing::warn!(chat = %chat_id, error = %err, "SetChatLink failed");
                        this.link_error = Some((chat_id, format!("Couldn't save link: {err}")));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn ensure_subagents(&mut self, chat_id: String, cx: &mut Context<Self>) {
        let ttl = if self
            .subagents
            .get(&chat_id)
            .is_some_and(|subs| subs.iter().any(|s| s.status == SubagentStatus::Running))
        {
            SUBAGENTS_RUNNING_REFRESH
        } else {
            SUBAGENTS_REFRESH
        };
        let fresh_enough = self
            .subagents_fetched_at
            .get(&chat_id)
            .is_some_and(|at| at.elapsed() < ttl);
        if fresh_enough || self.subagent_pending.contains(&chat_id) {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.subagent_pending.insert(chat_id.clone());
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::SCAN_CHAT_SUBAGENTS,
                    serde_json::json!({ "chatId": chat_id }),
                )
                .await;
            this.update(cx, |this, cx| {
                this.subagent_pending.remove(&chat_id);
                this.subagents_fetched_at.insert(chat_id.clone(), Instant::now());
                if let Ok(value) = result
                    && let Ok(subagents) = serde_json::from_value::<Vec<SubagentItem>>(value)
                {
                    // `subagent_count` is baked onto `OverviewRow` and feeds
                    // `tile_size` (every subagent is its own tile row — see
                    // `tile_content_height_estimate`), so only a COUNT change
                    // relayouts; a status flip (running → done) or a renamed
                    // description just repaints, matching how
                    // `ensure_context_usage`/`ensure_classification` only
                    // dirty rows on a layout-visible change.
                    let previous_count = this.subagents.get(&chat_id).map_or(0, Vec::len);
                    let next_count = subagents.len();
                    this.subagents.insert(chat_id, subagents);
                    if previous_count != next_count {
                        this.rows_dirty = true;
                        this.grouped_layout_cache = None;
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn ensure_classification(&mut self, chat_id: String, cx: &mut Context<Self>) {
        let fresh_enough = self
            .classification_fetched_at
            .get(&chat_id)
            .is_some_and(|at| at.elapsed() < CLASSIFICATION_REFRESH);
        if fresh_enough || self.classification_pending.contains(&chat_id) {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.classification_pending.insert(chat_id.clone());
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::CHAT_CLASSIFICATION,
                    serde_json::json!({ "chatId": chat_id }),
                )
                .await;
            this.update(cx, |this, cx| {
                this.classification_pending.remove(&chat_id);
                this.classification_fetched_at.insert(chat_id.clone(), Instant::now());
                if let Ok(value) = result
                    && let Some(category) = value.get("category").and_then(|v| v.as_str())
                    && let Some(origin) = value.get("origin").and_then(|v| v.as_str())
                {
                    let tool_counts = value
                        .get("toolCounts")
                        .and_then(|v| serde_json::from_value(v.clone()).ok())
                        .unwrap_or_default();
                    let skills_loaded = value
                        .get("skillsLoaded")
                        .and_then(|v| serde_json::from_value(v.clone()).ok())
                        .unwrap_or_default();
                    let previous = this.classification.insert(
                        chat_id,
                        ClassificationInfo {
                            category: category.to_string(),
                            origin: origin.to_string(),
                            tool_counts,
                            skills_loaded,
                        },
                    );
                    // `category`/`origin` are baked into `OverviewRow` and
                    // used as grouping keys — only a CHANGE invalidates rows
                    // + the grouped layout (tool/skill chips read
                    // `classification` live at render).
                    let changed = previous.is_none_or(|old| old.category != category || old.origin != origin);
                    if changed {
                        this.rows_dirty = true;
                        this.grouped_layout_cache = None;
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Fetch/refresh one chat's context-window fraction on
    /// `CONTEXT_USAGE_REFRESH`. The attempt time is recorded on success and
    /// failure alike (like `ensure_classification`), so an engine without the
    /// method just retries on the TTL. A landed value that rounds to the same
    /// whole percent is a no-op: nothing visible changes, so rows and the
    /// grouped layout stay cached (see [`context_pct_changed`]). A real
    /// change dirties both, because tile size feeds packing.
    fn ensure_context_usage(&mut self, chat_id: String, cx: &mut Context<Self>) {
        let fresh_enough = self
            .context_usage_fetched_at
            .get(&chat_id)
            .is_some_and(|at| at.elapsed() < CONTEXT_USAGE_REFRESH);
        if fresh_enough || self.context_usage_pending.contains(&chat_id) {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.context_usage_pending.insert(chat_id.clone());
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::CHAT_CONTEXT_USAGE,
                    serde_json::json!({ "chatId": chat_id }),
                )
                .await;
            this.update(cx, |this, cx| {
                this.context_usage_pending.remove(&chat_id);
                this.context_usage_fetched_at.insert(chat_id.clone(), Instant::now());
                if let Ok(value) = result {
                    let pct = value
                        .get("contextPct")
                        .and_then(|v| v.as_f64())
                        .map(|f| f as f32);
                    let previous = this.context_usage.insert(chat_id, pct);
                    if context_pct_changed(previous, pct) {
                        this.rows_dirty = true;
                        this.grouped_layout_cache = None;
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Fetch/refresh the "my open PRs" list on `MY_PRS_REFRESH`. Called from
    /// every overview render and idle tick, not just while the My PRs
    /// sidebar is open — the
    /// toolbar's "My PRs (N)" badge needs it in every view (the web polls
    /// `/my_prs` every tick regardless of pane state for the same reason).
    /// The engine side is cache-backed, so this is not a live `gh` call per
    /// request. Never runs while the overview isn't the visible route.
    fn ensure_my_prs(&mut self, cx: &mut Context<Self>) {
        let fresh_enough = self
            .my_prs_fetched_at
            .is_some_and(|at| at.elapsed() < MY_PRS_REFRESH);
        if fresh_enough || self.my_prs_pending {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.my_prs_pending = true;
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::MY_OPEN_PRS, serde_json::json!({}))
                .await;
            this.update(cx, |this, cx| {
                this.my_prs_pending = false;
                this.my_prs_fetched_at = Some(Instant::now());
                if let Ok(value) = result
                    && let Ok(items) = serde_json::from_value::<Vec<MyPrItem>>(value)
                {
                    this.my_prs = items;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// A chat (id, title) whose cached link status carries this exact PR
    /// (matched by URL — the only field both `MyPrItem` and a chat's linked
    /// `PrStatus` carry that's actually unique across repos; PR *numbers*
    /// collide between repos constantly). `None` when no currently-visible
    /// chat links to it, or its PR data hasn't been fetched/cached yet.
    fn chat_for_pr_url(&self, rows: &[OverviewRow], url: Option<&str>) -> Option<(String, String)> {
        let url = url?;
        let chat_id = self.link_status.iter().find_map(|(chat_id, entry)| {
            let pr_url = entry.status.pr.as_ref()?.url.as_deref()?;
            (pr_url == url).then(|| chat_id.clone())
        })?;
        let title = rows
            .iter()
            .find(|r| r.chat.id == chat_id)
            .map(|r| r.chat.title.clone().unwrap_or_else(|| "New session".to_string()))
            .unwrap_or_else(|| "chat".to_string());
        Some((chat_id, title))
    }

    /// Compact badge row for one chat: PR action (if any, highest-severity
    /// reason only), ticket identifier (if any). `None` when there's nothing
    /// to show yet (no data fetched, or fetched and genuinely nothing
    /// linked) — callers skip rendering the row. Subagents are no longer a
    /// badge here — see `subagent_tiles_for` for real nested tiles, matching
    /// the reference's actual visual nesting instead of a summary count.
    fn badges_for(&self, chat_id: &str, theme: &Theme, zoom: f32) -> Option<gpui::AnyElement> {
        let link = self.link_status.get(chat_id).map(|e| &e.status);
        let pr_badge = link
            .and_then(|s| s.pr.as_ref())
            .and_then(|pr| pr.action_reasons().into_iter().next())
            .map(pr_action_badge);
        let ticket_label = link
            .and_then(|s| s.ticket.as_ref())
            .map(|t| t.identifier.clone());
        // Equivalent to `pr_badge.is_none() && ticket_label.is_none()`, but
        // routed through the shared helper so this can never drift from
        // `OverviewRow::has_badge_row` (see `link_has_badge_row`'s doc
        // comment).
        if !link.is_some_and(link_has_badge_row) {
            return None;
        }
        let pr_source = link.and_then(|s| s.pr_source);
        let ticket_source = link.and_then(|s| s.ticket_source);
        let mut row = div().flex().items_center().gap(px(6.0 * zoom)).flex_wrap();
        if let Some((label, color)) = pr_badge {
            row = row.child(
                link_source_hover(
                    div()
                        .id(SharedString::from(format!("overview-badge-pr-{chat_id}")))
                        .flex()
                        .items_center()
                        .gap(px(3.0 * zoom))
                        .px(px(6.0 * zoom))
                        .py(px(1.0 * zoom))
                        .rounded(px(3.0 * zoom))
                        .bg(color.opacity(0.15))
                        .text_size(crate::typography::ui_rems(9.5 * zoom))
                        .text_color(color),
                    LinkKind::Pr,
                    pr_source,
                )
                .when(pr_source == Some(ChatLinkSource::Manual), |el| el.child(manual_link_pin(color, zoom)))
                .child(SharedString::from(label)),
            );
        }
        if let Some(identifier) = ticket_label {
            row = row.child(
                link_source_hover(
                    div()
                        .id(SharedString::from(format!("overview-badge-ticket-{chat_id}")))
                        .flex()
                        .items_center()
                        .gap(px(3.0 * zoom))
                        .px(px(6.0 * zoom))
                        .py(px(1.0 * zoom))
                        .rounded(px(3.0 * zoom))
                        .bg(theme.element_hover)
                        .text_size(crate::typography::ui_rems(9.5 * zoom))
                        .text_color(theme.text_muted),
                    LinkKind::Ticket,
                    ticket_source,
                )
                .when(ticket_source == Some(ChatLinkSource::Manual), |el| {
                    el.child(manual_link_pin(theme.text_muted, zoom))
                })
                .child(SharedString::from(identifier)),
            );
        }
        Some(row.into_any_element())
    }

    /// One compact nested tile per subagent — `session_canvas.html`'s
    /// `.tile.compact` sub-agent tiles (`~441-517`, `renderTile`'s compact
    /// branch `~1150-1160`, the stack itself `~1864-1906`). ALL of them, no
    /// cap: the parent card grows to fit (`tile_content_height_estimate`
    /// budgets every one). Each is a single fixed-height row — status dot
    /// (running = accent, done = green), truncated name, muted agent type,
    /// and a "⋯" affordance — with the full name in a hover tooltip (gpui's
    /// deferred tooltip layer: floats above everything, never reflows or
    /// gets clipped by the parent tile's `overflow_hidden`). Spans the
    /// parent's width; never sized by its own context (the web's rule).
    ///
    /// Clicking one opens its read-only transcript peek
    /// (`open_subagent_peek`). The press stops propagation at mouse-DOWN —
    /// like the web's `subTile.addEventListener("pointerdown", e =>
    /// e.stopPropagation())` — so the parent tile never arms its drag /
    /// click-to-expand (flat mode resolves that on the canvas's mouse-up
    /// from a drag armed at mouse-down; grouped mode's parent `on_click`
    /// needs its own mouse-down to register), and the click itself stops
    /// again for good measure.
    ///
    /// Shared by Canvas tiles (`zoom` = canvas zoom) and List rows (`1.0`).
    /// Empty when the chat has no subagents (or they haven't been fetched
    /// yet) — callers skip rendering.
    fn subagent_tiles_for(
        &self,
        chat_id: &str,
        theme: &Theme,
        zoom: f32,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let Some(subagents) = self.subagents.get(chat_id) else {
            return Vec::new();
        };
        subagents
            .iter()
            .map(|sub| {
                let color = sub.status.color();
                let name = sub.display_name();
                let peeking = self
                    .subagent_peek
                    .as_ref()
                    .is_some_and(|p| p.chat_id == chat_id && p.agent_id == sub.agent_id);
                let tooltip: SharedString = match sub.agent_type.as_deref() {
                    Some(t) if !t.is_empty() => format!("{name} · {t} · {}", sub.status.label()),
                    _ => format!("{name} · {}", sub.status.label()),
                }
                .into();
                let click_chat = chat_id.to_string();
                let click_sub = sub.clone();
                div()
                    .id(SharedString::from(format!("overview-subagent-{chat_id}-{}", sub.agent_id)))
                    .debug_selector(|| "overview-subagent-tile".into())
                    .flex_none()
                    .w_full()
                    .h(px(SUBAGENT_TILE_H * zoom))
                    .flex()
                    .items_center()
                    .gap(px(4.0 * zoom))
                    .px(px(7.0 * zoom))
                    .rounded(px(8.0 * zoom))
                    .border_1()
                    // Same status tint as the parent card (`renderTile`'s
                    // 14% background / 30% border color-mix).
                    .border_color(theme.border.blend(color.opacity(0.30)))
                    .bg(theme.surface_raised.blend(color.opacity(0.14)))
                    .cursor_pointer()
                    .hover(|el| el.border_color(color.opacity(0.6)))
                    .when(peeking, |el| el.border_color(theme.accent))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.open_subagent_peek(click_chat.clone(), &click_sub, window, cx);
                    }))
                    .tooltip(move |_, cx| cx.new(|_| LinkSourceTooltip(tooltip.clone())).into())
                    .child(div().flex_none().size(px(8.0 * zoom)).rounded_full().bg(color))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(20.0 * zoom))
                            .truncate()
                            .text_size(crate::typography::ui_rems(10.0 * zoom))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child(SharedString::from(name)),
                    )
                    .when_some(sub.agent_type.clone().filter(|t| !t.is_empty()), |el, agent_type| {
                        el.child(
                            div()
                                .flex_shrink(1.0)
                                .min_w_0()
                                .max_w(px(120.0 * zoom))
                                .truncate()
                                .text_size(crate::typography::ui_rems(9.0 * zoom))
                                .text_color(theme.text_muted)
                                .child(SharedString::from(agent_type)),
                        )
                    })
                    .child(
                        div()
                            .flex_none()
                            .size(px(15.0 * zoom))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(4.0 * zoom))
                            .text_size(crate::typography::ui_rems(11.0 * zoom))
                            .text_color(theme.text_muted)
                            .child(SharedString::from("⋯")),
                    )
                    .into_any_element()
            })
            .collect()
    }

    /// Small archive affordance — reused by both the list row and the tile.
    /// Always visible rather than hover-gated: simpler, and a stray click
    /// only archives (reversible via the sidebar's own archived-chats view),
    /// not a destructive delete. `zoom` scales it in Canvas mode (`1.0` from
    /// List mode, which has no zoom concept) so it shrinks/grows with the
    /// tile instead of staying a fixed pixel size while everything around it
    /// scales.
    fn archive_button(&self, chat_id: String, theme: &Theme, cx: &mut Context<Self>, zoom: f32) -> gpui::AnyElement {
        div()
            .id(SharedString::from(format!("overview-archive-{chat_id}")))
            .flex_none()
            .size(px(16.0 * zoom))
            .rounded(px(3.0 * zoom))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .text_size(crate::typography::ui_rems(11.0 * zoom))
            .text_color(theme.text_muted.opacity(0.5))
            .hover(|el| el.bg(theme.element_hover).text_color(theme.text_muted))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _, _, cx| {
                this.set_archived(chat_id.clone(), true, cx);
            }))
            .child(SharedString::from("×"))
            .into_any_element()
    }

    /// Explicit "View full chat" affordance — opens the interactive chat
    /// panel (`session_canvas.html`'s separate "View chat" button,
    /// `~1137-1143`: its own `pointerdown` stop-propagation keeps it from
    /// also triggering the card's own drag/click-to-expand gesture). Only
    /// the panel's "Open full chat →" actually leaves the overview.
    /// Tile/row body clicks toggle expand in place instead — see
    /// `toggle_expanded`.
    fn open_chat_button(&self, chat_id: String, theme: &Theme, cx: &mut Context<Self>, zoom: f32) -> gpui::AnyElement {
        div()
            .id(SharedString::from(format!("overview-open-{chat_id}")))
            .flex_none()
            .px(px(8.0 * zoom))
            .py(px(2.0 * zoom))
            .rounded(px(4.0 * zoom))
            .cursor_pointer()
            .text_size(crate::typography::ui_rems(10.0 * zoom))
            .text_color(theme.accent)
            .bg(theme.accent.opacity(0.1))
            .hover(|el| el.bg(theme.accent.opacity(0.2)))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _, _, cx| {
                this.open_chat_panel(chat_id.clone(), cx);
            }))
            .child(SharedString::from("View full chat →"))
            .into_any_element()
    }

    /// The interactive chat panel — `Some` only while `chat_panel_open` is
    /// set and a chat is selected. Header ("Open full chat →" / ×), the
    /// Shell's live transcript, and the Shell's composer docked BELOW it in
    /// a flex column (not overlaid, unlike the chat route — so the
    /// transcript's bottom clearance is zero here; `Shell::render` sets that
    /// per frame on this route).
    fn render_chat_panel(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let Some(chat_id) = self.panel_chat_id(cx) else {
            // Selection cleared under the open panel (chat deleted/archived,
            // new-session): nothing to show, so the panel closes itself.
            self.chat_panel_open = false;
            return None;
        };
        let title: SharedString = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|c| c.id == chat_id)
            .and_then(|c| c.title.clone())
            .unwrap_or_else(|| "Chat".to_string())
            .into();
        // The chat route drives these per frame (`render_main`); it doesn't
        // run on this route, so the panel does. Without the settled-docked
        // frame, arriving from the blank new-session screen would leave the
        // composer in its big centered hero layout.
        self.composer.update(cx, |composer, cx| {
            composer.set_dock_frame(crate::composer_dock::DockFrame::settled(true), cx);
            composer.set_available_width(CHAT_PANEL_W - 2.0 * CHAT_PANEL_COMPOSER_PAD, cx);
        });
        let jump_pill = self.transcript.read(cx).jump_button_shown().then(|| {
            let transcript = self.transcript.clone();
            div()
                .absolute()
                .bottom(px(10.0))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .id("overview-chat-panel-jump")
                        .h(px(26.0))
                        .px(px(11.0))
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .rounded_full()
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.surface_raised)
                        .shadow_md()
                        .cursor_pointer()
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.text)
                        .hover(|el| el.bg(theme.element_hover))
                        .on_click(move |_, _, cx| {
                            transcript.update(cx, |transcript, cx| transcript.jump_to_bottom(cx));
                        })
                        .child(SharedString::from("↓ Scroll to bottom")),
                )
        });
        let full_chat_id = chat_id.clone();
        Some(
            div()
                .id("overview-chat-panel")
                .flex_none()
                .w(px(CHAT_PANEL_W))
                .h_full()
                .flex()
                .flex_col()
                .border_l_1()
                .border_color(theme.border)
                .bg(theme.surface_raised)
                // Same attachment path as the chat route's dropzone.
                .on_drop(cx.listener(|this, paths: &gpui::ExternalPaths, _, cx| {
                    let paths = paths.paths().to_vec();
                    this.composer
                        .update(cx, |composer, cx| composer.add_paths(paths, cx));
                    cx.notify();
                }))
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_between()
                        .px(px(12.0))
                        .py(px(10.0))
                        .border_b_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(crate::typography::ui_rems(12.0))
                                .text_color(theme.text)
                                .child(title),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(10.0))
                                .child(
                                    div()
                                        .id("overview-chat-panel-open-full")
                                        .cursor_pointer()
                                        .text_size(crate::typography::ui_rems(10.5))
                                        .text_color(theme.accent)
                                        .child(SharedString::from("Open full chat →"))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.close_chat_panel(cx);
                                            this.open(full_chat_id.clone(), cx);
                                        })),
                                )
                                .child(
                                    div()
                                        .id("overview-chat-panel-close")
                                        .cursor_pointer()
                                        .text_color(theme.text_muted)
                                        .child(SharedString::from("×"))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.close_chat_panel(cx);
                                        })),
                                ),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .relative()
                        .child(self.transcript.clone())
                        .children(jump_pill),
                )
                .child(
                    div()
                        .flex_none()
                        .w_full()
                        .px(px(CHAT_PANEL_COMPOSER_PAD))
                        .pb(px(8.0))
                        .child(self.composer.clone()),
                )
                .into_any_element(),
        )
    }

    /// The right-side panel's read-only subagent-transcript mode — `Some`
    /// only while `subagent_peek` is set. Web `#transcript-panel` opened for
    /// a sub-agent: header = the subagent's name + agent type (+ model once
    /// loaded) and a ✕; body = its turns (role label + time, text, tool
    /// chips), scrolled to the end on load. No composer: a subagent can't be
    /// continued. Same width/chrome as the live chat panel so switching
    /// between the two modes doesn't jump the canvas.
    fn render_subagent_peek(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let peek = self.subagent_peek.as_ref()?;
        let code_family = crate::typography::code_effective_family_name(cx);
        let mut subtitle = String::from("sub-agent transcript");
        if let PeekLoad::Loaded(t) = &peek.load
            && let Some(model) = t.model.as_deref().filter(|m| !m.is_empty())
        {
            subtitle.push_str(" · ");
            subtitle.push_str(model);
        }
        let hint = |text: &'static str| {
            div()
                .text_size(crate::typography::ui_rems(11.0))
                .text_color(theme.text_muted.opacity(0.7))
                .child(SharedString::from(text))
                .into_any_element()
        };
        let body: Vec<gpui::AnyElement> = match &peek.load {
            PeekLoad::Loading => vec![hint("Loading…")],
            PeekLoad::Failed => vec![hint("Failed to load transcript.")],
            PeekLoad::Loaded(t) if t.turns.is_empty() => vec![hint("No turns found in this transcript.")],
            PeekLoad::Loaded(t) => t
                .turns
                .iter()
                .enumerate()
                .map(|(ti, turn)| self.render_peek_turn(ti, turn, &peek.open_tools, theme, &code_family, cx))
                .collect(),
        };
        let name: SharedString = peek.name.clone().into();
        let agent_type = peek.agent_type.clone().filter(|t| !t.is_empty());
        Some(
            div()
                .id("overview-subagent-peek")
                .debug_selector(|| "overview-subagent-peek".into())
                .track_focus(&self.peek_focus)
                .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                    if event.keystroke.key == "escape" {
                        this.close_subagent_peek(cx);
                        cx.stop_propagation();
                    }
                }))
                .flex_none()
                .w(px(CHAT_PANEL_W))
                .h_full()
                .flex()
                .flex_col()
                .border_l_1()
                .border_color(theme.border)
                .bg(theme.surface_raised)
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap(px(10.0))
                        .px(px(12.0))
                        .py(px(10.0))
                        .border_b_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap(px(2.0))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(6.0))
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .truncate()
                                                .text_size(crate::typography::ui_rems(12.0))
                                                .text_color(theme.text)
                                                .child(name),
                                        )
                                        .when_some(agent_type, |el, agent_type| {
                                            el.child(
                                                div()
                                                    .flex_none()
                                                    .max_w(px(160.0))
                                                    .truncate()
                                                    .text_size(crate::typography::ui_rems(10.5))
                                                    .text_color(theme.text_muted)
                                                    .child(SharedString::from(agent_type)),
                                            )
                                        }),
                                )
                                .child(
                                    div()
                                        .truncate()
                                        .text_size(crate::typography::ui_rems(10.0))
                                        .text_color(theme.text_muted.opacity(0.7))
                                        .child(SharedString::from(subtitle)),
                                ),
                        )
                        .child(
                            div()
                                .id("overview-subagent-peek-close")
                                .flex_none()
                                .cursor_pointer()
                                .text_color(theme.text_muted)
                                .hover(|el| el.text_color(theme.text))
                                .child(SharedString::from("✕"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.close_subagent_peek(cx);
                                })),
                        ),
                )
                .child(
                    div()
                        .id("overview-subagent-peek-body")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .track_scroll(&self.peek_scroll)
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(14.0))
                                .px(px(14.0))
                                .py(px(12.0))
                                .children(body),
                        ),
                )
                .into_any_element(),
        )
    }

    /// One transcript turn (web `renderTurns`' `.turn`): role label + time,
    /// the text (newlines preserved; ``` fences as monospace blocks — see
    /// [`split_fenced_code`]), then its tool chips. A chip with a result
    /// preview is a disclosure (web `<details class="tool-detail">`):
    /// clicking toggles the result open underneath.
    fn render_peek_turn(
        &self,
        ti: usize,
        turn: &TranscriptTurn,
        open_tools: &HashSet<(usize, usize)>,
        theme: &Theme,
        code_family: &SharedString,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let is_user = turn.role == "user";
        let time = format_turn_time(turn.timestamp.as_deref());
        let text_blocks: Vec<gpui::AnyElement> = turn
            .text
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .map(split_fenced_code)
            .unwrap_or_default()
            .into_iter()
            .map(|(is_code, chunk)| {
                if is_code {
                    div()
                        .w_full()
                        .px(px(8.0))
                        .py(px(6.0))
                        .rounded(px(5.0))
                        .bg(theme.element_hover.opacity(0.6))
                        .font_family(code_family.clone())
                        .text_size(crate::typography::ui_rems(11.0))
                        .text_color(theme.text)
                        .child(SharedString::from(chunk))
                        .into_any_element()
                } else {
                    div()
                        .w_full()
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.text)
                        .child(SharedString::from(chunk))
                        .into_any_element()
                }
            })
            .collect();
        let tools: Vec<gpui::AnyElement> = turn
            .tools
            .iter()
            .enumerate()
            .map(|(tj, tool)| {
                let open = open_tools.contains(&(ti, tj));
                let has_result = tool.result_preview.as_deref().is_some_and(|r| !r.is_empty());
                let summary = if tool.input_preview.is_empty() {
                    tool.name.clone()
                } else {
                    format!("{} {}", tool.name, tool.input_preview)
                };
                let chip = div()
                    .id(SharedString::from(format!("overview-peek-tool-{ti}-{tj}")))
                    .max_w_full()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .px(px(7.0))
                    .py(px(2.0))
                    .rounded(px(4.0))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.element_hover.opacity(0.4))
                    .font_family(code_family.clone())
                    .text_size(crate::typography::ui_rems(10.5))
                    .text_color(theme.text_muted)
                    .when(has_result, |el| {
                        el.cursor_pointer()
                            .hover(|el| el.text_color(theme.text))
                            .child(SharedString::from(if open { "▾" } else { "▸" }))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(peek) = this.subagent_peek.as_mut()
                                    && !peek.open_tools.remove(&(ti, tj))
                                {
                                    peek.open_tools.insert((ti, tj));
                                }
                                cx.notify();
                            }))
                    })
                    .child(div().min_w_0().truncate().child(SharedString::from(summary)));
                div()
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .child(chip)
                    .when(open, |el| {
                        el.when_some(tool.result_preview.clone(), |el, result| {
                            el.child(
                                div()
                                    .ml(px(10.0))
                                    .px(px(8.0))
                                    .py(px(5.0))
                                    .rounded(px(4.0))
                                    .border_l_2()
                                    .border_color(theme.border)
                                    .font_family(code_family.clone())
                                    .text_size(crate::typography::ui_rems(10.5))
                                    .text_color(theme.text_muted)
                                    .child(SharedString::from(result)),
                            )
                        })
                    })
                    .into_any_element()
            })
            .collect();
        div()
            .flex()
            .flex_col()
            .gap(px(5.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .text_size(crate::typography::ui_rems(10.5))
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(if is_user { theme.accent } else { theme.text_muted })
                            .child(SharedString::from(turn_role_label(&turn.role))),
                    )
                    .when(!time.is_empty(), |el| {
                        el.child(
                            div()
                                .text_color(theme.text_muted.opacity(0.6))
                                .child(SharedString::from(time)),
                        )
                    }),
            )
            .children(text_blocks)
            .when(!tools.is_empty(), |el| {
                el.child(div().flex().flex_col().gap(px(4.0)).children(tools))
            })
            .into_any_element()
    }

    /// Inline expanded detail — toggled by a real tile/row click (not a
    /// drag), never navigates. Ports `renderDetails`
    /// (`session_canvas.html:1092-1113`) field by field, from data actually
    /// available here:
    /// - branch + worktree badge (engine `isWorktree` when the link status
    ///   carries it, else the reference's own cwd rule, [`cwd_is_worktree`])
    /// - changes (diff stat) — only when the engine's link status carries a
    ///   `diffStat` (see `LinkStatusEntry::diff_stat`); absent today
    /// - launched via (origin label), pull request (title/state/draft,
    ///   review decision, checks, reviewer chips — click opens GitHub),
    ///   linear ticket (identifier/title/status — click opens Linear)
    /// - last message (Zeron has no separate `summary` field)
    /// - tools used / skills loaded chip rows (imported chats only — that's
    ///   where `CHAT_CLASSIFICATION`'s tally comes from)
    /// - "Open chat →" + Archive/Unarchive actions. The web's "Open in
    ///   Terminal/Cursor" has no Zeron equivalent (the chat IS here) —
    ///   "Open full chat →" in the preview panel is the analog.
    fn expanded_detail_for(
        &self,
        row: &OverviewRow,
        theme: &Theme,
        chat_id: String,
        zoom: f32,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let entry = self.link_status.get(&row.chat.id);
        let link = entry.map(|e| &e.status);
        let classification = self.classification.get(&row.chat.id);
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        let label_el = |label: &'static str| {
            div()
                .flex_none()
                .w(px(78.0 * zoom))
                .text_size(crate::typography::ui_rems(10.0 * zoom))
                .text_color(theme.text_muted.opacity(0.6))
                .child(SharedString::from(label))
        };
        let detail_row = |label: &'static str, value: gpui::AnyElement| {
            div()
                .flex()
                .gap(px(6.0 * zoom))
                .text_size(crate::typography::ui_rems(10.0 * zoom))
                .child(label_el(label))
                .child(div().flex_1().min_w_0().text_color(theme.text_muted).child(value))
                .into_any_element()
        };
        let text = |value: String| SharedString::from(value).into_any_element();
        let chip = |text: String, color: gpui::Hsla, bg: gpui::Hsla| {
            div()
                .px(px(6.0 * zoom))
                .py(px(1.0 * zoom))
                .rounded(px(3.0 * zoom))
                .bg(bg)
                .text_size(crate::typography::ui_rems(9.5 * zoom))
                .text_color(color)
                .child(SharedString::from(text))
        };
        // `renderChips`/`renderToolChips`: an empty list renders a single
        // "none" chip rather than hiding the row.
        let chip_row = |chips: Vec<String>| {
            let chips = if chips.is_empty() { vec!["none".to_string()] } else { chips };
            div()
                .flex()
                .flex_wrap()
                .gap(px(4.0 * zoom))
                .children(
                    chips
                        .into_iter()
                        .map(|t| chip(t, theme.text_muted, theme.element_hover)),
                )
                .into_any_element()
        };

        // branch
        let is_worktree = entry
            .and_then(|e| e.is_worktree)
            .unwrap_or_else(|| cwd_is_worktree(row.chat.cwd.as_deref()));
        let branch_value = match &row.chat.branch {
            Some(branch) => div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(5.0 * zoom))
                .child(SharedString::from(format!("⎇ {branch}")))
                .when(is_worktree, |el| {
                    el.child(chip("worktree".into(), theme.text_muted, theme.element_hover))
                })
                .into_any_element(),
            None => text("—".into()),
        };
        rows.push(detail_row("branch", branch_value));

        // changes
        if let Some(diff) = entry.and_then(|e| e.diff_stat) {
            let files = if diff.files_changed == 1 { "file" } else { "files" };
            rows.push(detail_row(
                "changes",
                div()
                    .flex()
                    .gap(px(4.0 * zoom))
                    .child(
                        div()
                            .text_color(gpui::rgb(0x0ca30c))
                            .child(SharedString::from(format!("+{}", diff.lines_added))),
                    )
                    .child(
                        div()
                            .text_color(gpui::rgb(0xd03b3b))
                            .child(SharedString::from(format!("-{}", diff.lines_removed))),
                    )
                    .child(SharedString::from(format!("· {} {files}", diff.files_changed)))
                    .into_any_element(),
            ));
        }

        if let Some(origin) = &row.origin {
            rows.push(detail_row("launched via", text(origin_label(origin))));
        }

        if let Some(pr) = link.and_then(|s| s.pr.as_ref()) {
            let mut summary = format!("#{} {}", pr.number, pr.title.clone().unwrap_or_default());
            summary.push_str(" — ");
            summary.push_str(if pr.is_draft { "draft" } else { &pr.state });
            if let Some(decision) = &pr.review_decision {
                summary.push_str(", ");
                summary.push_str(&decision.to_lowercase().replace('_', " "));
            }
            let checks = pr.checks.map(checks_style);
            if let Some((_, _, checks_label)) = checks {
                summary.push_str(&format!(", checks {checks_label}"));
            }
            let url = pr.url.clone();
            let pr_source = link.and_then(|s| s.pr_source);
            let value = div()
                .flex()
                .flex_col()
                .gap(px(4.0 * zoom))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.0 * zoom))
                        .when(pr_source == Some(ChatLinkSource::Manual), |el| {
                            el.child(manual_link_pin(theme.text_muted, zoom))
                        })
                        .child(
                            link_source_hover(
                                div()
                                    .id(SharedString::from(format!("overview-detail-pr-{chat_id}")))
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_wrap()
                                    .gap(px(4.0 * zoom)),
                                LinkKind::Pr,
                                pr_source,
                            )
                            .when(url.is_some(), |el| {
                                el.cursor_pointer().text_color(theme.accent).hover(|el| el.underline())
                            })
                            .child(SharedString::from(summary))
                            .when_some(checks, |el, (color, icon, _)| {
                                el.child(div().text_color(color).child(SharedString::from(icon)))
                            })
                            .when_some(url, |el, url| {
                                el.on_click(move |_, _, cx| cx.open_url(&url))
                            }),
                        )
                        .when(pr_source.is_some(), |el| {
                            el.child(self.unlink_button(chat_id.clone(), LinkKind::Pr, theme, zoom, cx))
                        }),
                )
                .when(!pr.reviewers.is_empty(), |el| el.child(chip_row(pr.reviewers.clone())))
                .into_any_element();
            rows.push(detail_row("pull request", value));
        }

        if let Some(ticket) = link.and_then(|s| s.ticket.as_ref()) {
            let mut summary = ticket.identifier.clone();
            if let Some(title) = &ticket.title {
                summary.push_str(&format!(" — {title}"));
            }
            if let Some(status) = &ticket.status {
                summary.push_str(&format!(" ({status})"));
            }
            let url = ticket.url.clone();
            let ticket_source = link.and_then(|s| s.ticket_source);
            rows.push(detail_row(
                "linear ticket",
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.0 * zoom))
                    .when(ticket_source == Some(ChatLinkSource::Manual), |el| {
                        el.child(manual_link_pin(theme.text_muted, zoom))
                    })
                    .child(
                        link_source_hover(
                            div()
                                .id(SharedString::from(format!("overview-detail-ticket-{chat_id}")))
                                .flex_1()
                                .min_w_0(),
                            LinkKind::Ticket,
                            ticket_source,
                        )
                        .when(url.is_some(), |el| {
                            el.cursor_pointer().text_color(theme.accent).hover(|el| el.underline())
                        })
                        .child(SharedString::from(summary))
                        .when_some(url, |el, url| el.on_click(move |_, _, cx| cx.open_url(&url))),
                    )
                    .when(ticket_source.is_some(), |el| {
                        el.child(self.unlink_button(chat_id.clone(), LinkKind::Ticket, theme, zoom, cx))
                    })
                    .into_any_element(),
            ));
        }

        if let Some(preview) = &row.chat.last_message_preview {
            rows.push(detail_row("last message", text(preview.clone())));
        }

        if let Some(info) = classification {
            // Sorted by count descending — "most-used tool first" reads
            // better than the web's insertion order; formatted `Name×N`
            // exactly like `renderToolChips`.
            let mut tools: Vec<(&String, &usize)> = info.tool_counts.iter().collect();
            tools.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
            let tool_chips: Vec<String> = tools
                .into_iter()
                .map(|(name, count)| format!("{name}×{count}"))
                .collect();
            rows.push(detail_row("tools used", chip_row(tool_chips)));
            rows.push(detail_row("skills loaded", chip_row(info.skills_loaded.clone())));
        }

        let archived = row.chat.archived;
        let archive_id = chat_id.clone();
        let archive_toggle = div()
            .id(SharedString::from(format!("overview-detail-archive-{chat_id}")))
            .flex_none()
            .px(px(8.0 * zoom))
            .py(px(2.0 * zoom))
            .rounded(px(4.0 * zoom))
            .cursor_pointer()
            .text_size(crate::typography::ui_rems(10.0 * zoom))
            .text_color(theme.text_muted)
            .border_1()
            .border_color(theme.border)
            .hover(|el| el.bg(theme.element_hover).text_color(theme.text))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _, _, cx| {
                this.set_archived(archive_id.clone(), !archived, cx);
            }))
            .child(SharedString::from(if archived { "Unarchive" } else { "Archive" }));

        let editor_open = self.link_editor_chat.as_deref() == Some(chat_id.as_str());
        let link_toggle_id = chat_id.clone();
        let link_toggle = div()
            .id(SharedString::from(format!("overview-detail-link-{chat_id}")))
            .flex_none()
            .px(px(8.0 * zoom))
            .py(px(2.0 * zoom))
            .rounded(px(4.0 * zoom))
            .cursor_pointer()
            .text_size(crate::typography::ui_rems(10.0 * zoom))
            .text_color(theme.text_muted)
            .border_1()
            .border_color(theme.border)
            .when(editor_open, |el| el.bg(theme.element_active).text_color(theme.text))
            .hover(|el| el.bg(theme.element_hover).text_color(theme.text))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _, window, cx| {
                if this.link_editor_chat.as_deref() == Some(link_toggle_id.as_str()) {
                    this.close_link_editor(cx);
                } else {
                    this.open_link_editor(link_toggle_id.clone(), false, window, cx);
                }
            }))
            .child(SharedString::from(if editor_open { "Cancel link" } else { "Link PR / ticket…" }));

        let link_error = self
            .link_error
            .as_ref()
            .filter(|(id, _)| *id == chat_id)
            .map(|(_, message)| message.clone());
        let editor = editor_open.then(|| {
            let hint = if self.link_submitting {
                "Linking…".to_string()
            } else {
                "Enter to link · Esc to cancel".to_string()
            };
            div()
                .flex()
                .flex_col()
                .gap(px(2.0 * zoom))
                .child(
                    div()
                        .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                            match event.keystroke.key.as_str() {
                                "enter" => this.submit_link_editor(cx),
                                "escape" => this.close_link_editor(cx),
                                _ => return,
                            }
                            cx.stop_propagation();
                            window.prevent_default();
                        }))
                        .child(popover::search_input_frame(theme, self.link_input.clone().into_any_element())),
                )
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(9.5 * zoom))
                        .text_color(theme.text_muted.opacity(0.6))
                        .child(SharedString::from(hint)),
                )
                .into_any_element()
        });
        let critical: gpui::Hsla = gpui::rgb(0xd03b3b).into();
        let error_line = link_error.map(|message| {
            div()
                .text_size(crate::typography::ui_rems(9.5 * zoom))
                .text_color(critical)
                .child(SharedString::from(message))
                .into_any_element()
        });

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(4.0 * zoom))
            .p(px(8.0 * zoom))
            .mt(px(4.0 * zoom))
            .rounded(px(6.0 * zoom))
            // Was `theme.surface.opacity(0.6)` — see-through enough to hurt
            // contrast against whatever else is on the canvas underneath.
            // Solid background, same as the tile itself.
            .bg(theme.surface_raised)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .children(rows)
            .child(
                div()
                    .pt(px(4.0 * zoom))
                    .flex()
                    .items_center()
                    .gap(px(6.0 * zoom))
                    .child(self.open_chat_button(chat_id, theme, cx, zoom))
                    .child(archive_toggle)
                    .child(link_toggle),
            )
            .children(editor)
            .children(error_line)
            .into_any_element()
    }

    /// Small "✕" that clears a stored (any-source) PR/ticket link —
    /// `SET_CHAT_LINK` with `value: null`. Only rendered when the slot has a
    /// stored source; an inference-derived link has nothing to clear.
    fn unlink_button(
        &self,
        chat_id: String,
        kind: LinkKind,
        theme: &Theme,
        zoom: f32,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let tooltip = SharedString::from(match kind {
            LinkKind::Pr => "Unlink this PR",
            LinkKind::Ticket => "Unlink this ticket",
        });
        div()
            .id(SharedString::from(format!("overview-unlink-{}-{chat_id}", kind.wire())))
            .flex_none()
            .px(px(4.0 * zoom))
            .rounded(px(3.0 * zoom))
            .cursor_pointer()
            .text_size(crate::typography::ui_rems(10.0 * zoom))
            .text_color(theme.text_muted.opacity(0.5))
            .hover(|el| el.bg(theme.element_hover).text_color(theme.text))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _, _, cx| {
                this.set_chat_link(chat_id.clone(), kind, None, cx);
            }))
            .tooltip(move |_, cx| cx.new(|_| LinkSourceTooltip(tooltip.clone())).into())
            .child(SharedString::from("✕"))
            .into_any_element()
    }

    /// Consume the search-edit auto-fit request: `true` only when the query
    /// is non-empty and narrowed things to 1..=`FIT_MAX_MATCHES` rows (web:
    /// broad one-character queries must not yank the view around).
    fn take_pending_fit(&mut self, match_count: usize, cx: &Context<Self>) -> bool {
        if !std::mem::take(&mut self.pending_fit_to_matches) {
            return false;
        }
        let has_query = !self.search.read(cx).text().trim().is_empty();
        has_query && (1..=FIT_MAX_MATCHES).contains(&match_count)
    }

    /// Apply [`fit_view_to_rects`] against the captured canvas viewport. A
    /// no-op before the first paint (no bounds to fit into yet).
    fn fit_view(&mut self, rects: &[(f32, f32, f32, f32)]) {
        let Some(bounds) = self.canvas_bounds.get() else {
            return;
        };
        let viewport = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        if let Some((zoom, pan)) = fit_view_to_rects(rects, viewport) {
            self.canvas_zoom = zoom;
            self.canvas_pan = pan;
        }
    }

    /// Frame one tile for a PR-row jump ([`focus_view_on_rect`]) against the
    /// captured canvas viewport — measured at paint, so it is already the
    /// width left over between the PR sidebar and the chat panel. `rect` is
    /// the tile's layout TARGET, so an in-flight organize move settles into
    /// the framed spot.
    fn focus_view(&mut self, rect: (f32, f32, f32, f32)) {
        let Some(bounds) = self.canvas_bounds.get() else {
            return;
        };
        let viewport = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        let (zoom, pan) = focus_view_on_rect(rect, viewport, self.canvas_zoom);
        self.canvas_zoom = zoom;
        self.canvas_pan = pan;
    }

    /// Zoom toward a specific WINDOW-coordinate cursor position, keeping the
    /// board-space point currently under the cursor stationary — ports
    /// `session_canvas.html`'s `setZoom` exactly (`~1620-1631`: convert
    /// cursor to board-space at the OLD zoom, apply the new zoom, then solve
    /// pan so that same board point lands back under the cursor). Falls back
    /// to treating the cursor as already canvas-local if bounds haven't been
    /// captured yet (first-frame edge case — `canvas_bounds` is `None` until
    /// the bounds-capture `canvas()` overlay's first paint).
    fn zoom_toward(&mut self, target_zoom: f32, cursor_window: (f32, f32)) {
        let origin = self
            .canvas_bounds
            .get()
            .map(|b| (f32::from(b.origin.x), f32::from(b.origin.y)))
            .unwrap_or((0.0, 0.0));
        let cursor = (cursor_window.0 - origin.0, cursor_window.1 - origin.1);
        let old_zoom = self.canvas_zoom;
        let board = (
            (cursor.0 - self.canvas_pan.0) / old_zoom,
            (cursor.1 - self.canvas_pan.1) / old_zoom,
        );
        let new_zoom = target_zoom.clamp(ZOOM_MIN, ZOOM_MAX);
        self.canvas_zoom = new_zoom;
        self.canvas_pan = (cursor.0 - board.0 * new_zoom, cursor.1 - board.1 * new_zoom);
    }

    /// Viewport culling: gpui has real paint-time clipping (`ContentMask`,
    /// applied via `.overflow_hidden()`, already on the canvas container) but
    /// that only clips the final painted pixels — `request_layout`/`prepaint`
    /// still run unconditionally for every child element regardless of
    /// visibility (confirmed by reading gpui's own `div.rs` directly: no
    /// bounds check gates those calls). At real scale (~227 tiles, each with
    /// several text runs) that's exactly the "scene too large" GPU buffer
    /// growth seen in real logs (28889 text-glyph quads in one frame) — text
    /// shaping for every tile happens whether or not it's on-screen. This
    /// skips constructing a tile's `AnyElement` at all when its screen-space
    /// bounds don't intersect the viewport (plus a margin so tiles don't pop
    /// in abruptly while panning), catching the cost before layout/prepaint
    /// ever run for it, not just before paint.
    fn tile_in_viewport(&self, screen_x: f32, screen_y: f32, tile_w: f32, tile_h: f32) -> bool {
        const MARGIN_PX: f32 = 200.0;
        let Some(bounds) = self.canvas_bounds.get() else {
            return true; // first frame, bounds not captured yet — render everything once
        };
        let (vw, vh) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        screen_x + tile_w >= -MARGIN_PX
            && screen_x <= vw + MARGIN_PX
            && screen_y + tile_h >= -MARGIN_PX
            && screen_y <= vh + MARGIN_PX
    }

    /// The canvas viewport's own center in window coordinates, for the +/-
    /// zoom buttons (which zoom toward viewport center, unlike scroll-to-
    /// zoom which targets the cursor) — `session_canvas.html`'s zoom-in/
    /// zoom-out buttons do the same (`~1665-1672`).
    fn canvas_center_window(&self) -> (f32, f32) {
        match self.canvas_bounds.get() {
            Some(b) => (
                f32::from(b.origin.x) + f32::from(b.size.width) / 2.0,
                f32::from(b.origin.y) + f32::from(b.size.height) / 2.0,
            ),
            None => (0.0, 0.0),
        }
    }

    /// The zoom-percentage label + `+`/`-`/fit-all controls, overlaid on the
    /// canvas (bottom-right) — ports `session_canvas.html`'s `#zoom-in`/
    /// `#zoom-out`/`zoomLabel` (`~1665-1676`): buttons zoom toward viewport
    /// center, clicking the percentage resets to 100% zoom / origin pan in
    /// one click. `fit_all_rects` is the bounding-box input for the "Fit
    /// all" button — every currently on-canvas tile's board-space rect
    /// (post-filter, this mode's own positions), the same set the search
    /// auto-fit (`fit_view`/`take_pending_fit`) already frames; empty is a
    /// no-op ([`fit_view_to_rects`] returns `None` for an empty slice).
    fn render_zoom_controls(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
        fit_all_rects: Vec<(f32, f32, f32, f32)>,
    ) -> gpui::AnyElement {
        let zoom_pct = (self.canvas_zoom * 100.0).round() as i32;
        let btn = |label: &'static str| {
            div()
                .id(SharedString::from(format!("overview-zoom-{label}")))
                .flex_none()
                .size(px(20.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.0))
                .cursor_pointer()
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.text_muted)
                .hover(|el| el.bg(theme.element_hover).text_color(theme.text))
                .child(SharedString::from(label))
        };
        div()
            .id("overview-zoom-controls")
            .absolute()
            .bottom(px(10.0))
            .right(px(10.0))
            .flex()
            .items_center()
            .gap(px(2.0))
            .p(px(3.0))
            .rounded(px(6.0))
            .bg(theme.surface_raised.opacity(0.9))
            .border_1()
            .border_color(theme.border)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(btn("−").on_click(cx.listener(|this, _, _, cx| {
                let center = this.canvas_center_window();
                this.zoom_toward(this.canvas_zoom / 1.2, center);
                cx.notify();
            })))
            .child(
                div()
                    .id("overview-zoom-pct")
                    .flex_none()
                    .px(px(4.0))
                    .cursor_pointer()
                    .text_size(crate::typography::ui_rems(10.5))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(format!("{zoom_pct}%")))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.canvas_zoom = 1.0;
                        this.canvas_pan = (0.0, 0.0);
                        cx.notify();
                    })),
            )
            .child(btn("+").on_click(cx.listener(|this, _, _, cx| {
                let center = this.canvas_center_window();
                this.zoom_toward(this.canvas_zoom * 1.2, center);
                cx.notify();
            })))
            .child(
                div()
                    .id("overview-zoom-fit-all")
                    .flex_none()
                    .size(px(20.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .text_color(theme.text_muted)
                    .hover(|el| el.bg(theme.element_hover).text_color(theme.text))
                    .child(crate::icons::icon(crate::icons::EXPAND_ARROWS).size(px(11.0)))
                    .tooltip(|_, cx| cx.new(|_| LinkSourceTooltip("Fit all cards".into())).into())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.fit_view(&fit_all_rects);
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    /// Off-screen rescue pill ("N cards off-screen — Fit view"): shown when
    /// every currently on-canvas tile (post-filter) is outside the
    /// viewport — i.e. the user panned/zoomed away and is staring at empty
    /// board space, not merely that all tiles happen to be filtered out.
    /// Detection reuses the exact same [`Self::tile_in_viewport`] result the
    /// render loop already computed against each tile's CURRENT (animated)
    /// position for viewport culling — not a separate check against target
    /// positions — so a tile mid-organize-move that happens to be off its
    /// straight-line path for one frame can never trip a false positive:
    /// whatever this says is "visible" is exactly what painted this frame.
    /// Clicking the pill runs the identical [`fit_view`] bbox math as the
    /// zoom cluster's "Fit all" button.
    fn render_rescue_pill(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
        count: usize,
        rects: Vec<(f32, f32, f32, f32)>,
    ) -> gpui::AnyElement {
        let label = format!("{count} card{} off-screen — Fit view", if count == 1 { "" } else { "s" });
        div()
            .id("overview-rescue-wrap")
            .absolute()
            .top(px(14.0))
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .child(
                div()
                    .id("overview-rescue-pill")
                    .h(px(28.0))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .rounded_full()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.surface_raised)
                    .shadow_md()
                    .cursor_pointer()
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text)
                    .hover(|el| el.bg(theme.element_hover))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.fit_view(&rects);
                        cx.notify();
                    }))
                    .child(SharedString::from(label)),
            )
            .into_any_element()
    }

    /// A zero-size `gpui::canvas` overlay whose only job is capturing the
    /// parent canvas container's own on-screen bounds into `canvas_bounds`
    /// each paint (see that field's doc comment for why this is needed —
    /// mouse events report window coordinates, not element-local ones).
    fn bounds_capture(&self) -> gpui::AnyElement {
        let bounds_cell = self.canvas_bounds.clone();
        gpui::canvas(
            move |bounds, _, _| {
                bounds_cell.set(Some(bounds));
            },
            |_, _, _, _| {},
        )
        .absolute()
        .inset_0()
        .into_any_element()
    }

    /// The logical position for `chat_id`, assigning (and caching) a default
    /// grid slot if this is the first time canvas mode has seen it (either
    /// truly new, or no persisted position exists for it yet). The slot
    /// stride is the LARGEST tile size ([`flat_grid_stride`]), not this
    /// tile's current one: a slot is assigned once and kept, while the tile
    /// keeps growing as its context fills, so a stride sized to today's
    /// width would overlap its neighbor later. The web's `defaultPosition`
    /// does the same with a fixed stride larger than any tile.
    /// Where to DRAW a tile whose layout target this pass is `target` —
    /// records the target and starts/continues/drops its organize move via
    /// `plan_tile_motion`. Called for every present tile (culled or not)
    /// before viewport culling, so culling sees the interpolated position
    /// and an off-screen tile's move still tracks its target.
    fn animated_tile_pos(&mut self, chat_id: &str, target: TilePos, now: Instant, reduced: bool) -> TilePos {
        let live_drag = self
            .dragging_tile
            .as_ref()
            .is_some_and(|d| d.committed && d.chat_id == chat_id);
        let duration = TILE_MOVE_DURATION.mul_f32(crate::motion::speed_scale());
        let prev = self.tile_targets.insert(chat_id.to_string(), target);
        let current = self.tile_moves.get(chat_id).copied();
        let (next, pos) = plan_tile_motion(prev, current, target, live_drag, reduced, now, duration);
        match next {
            Some(m) => {
                self.tile_moves.insert(chat_id.to_string(), m);
            }
            None => {
                self.tile_moves.remove(chat_id);
            }
        }
        pos
    }

    /// End of a canvas pass: forget tiles that weren't present, so their
    /// next appearance counts as a first appearance (no fly-in).
    fn prune_tile_motion(&mut self, present: &HashSet<&str>) {
        self.tile_targets.retain(|id, _| present.contains(id.as_str()));
        self.tile_moves.retain(|id, _| present.contains(id.as_str()));
    }

    fn clear_tile_motion(&mut self) {
        self.tile_targets.clear();
        self.tile_moves.clear();
    }

    fn position_for(&mut self, chat_id: &str, slot: usize) -> TilePos {
        if let Some(pos) = self.tile_positions.get(chat_id) {
            return *pos;
        }
        let (stride_w, stride_h) = flat_grid_stride();
        let col = (slot % GRID_COLS) as f32;
        let row = (slot / GRID_COLS) as f32;
        let pos = TilePos {
            x: col * stride_w,
            y: row * stride_h,
        };
        self.tile_positions.insert(chat_id.to_string(), pos);
        pos
    }

    /// Merge persisted tile positions (`overview_positions::load_positions`)
    /// into `tile_positions` — the client-side analog of the reference tool's
    /// `localStorage`-backed free-form layout surviving a reload. Runs once,
    /// lazily, the first time flat canvas mode renders (not in `new`, which
    /// has no `cx` to read `AppState.data_dir` through). A chat already
    /// present in `tile_positions` (assigned a default grid slot earlier this
    /// session, before this load ran) keeps that slot rather than jumping —
    /// `HashMap::entry().or_insert()` only fills gaps, never overwrites.
    fn ensure_positions_loaded(&mut self, cx: &Context<Self>) {
        if self.positions_loaded {
            return;
        }
        self.positions_loaded = true;
        let Some(data_dir) = self.state.read(cx).data_dir.clone() else {
            return; // engine not bootstrapped yet; nothing to load from
        };
        for (id, (x, y)) in overview_positions::load_positions(&data_dir) {
            self.tile_positions.entry(id).or_insert(TilePos { x, y });
        }
    }

    /// Persist the current flat-mode layout — called on tile-drag release.
    /// Grouped-mode positions are algorithm-computed every render, never
    /// user-placed, so they're deliberately never saved here.
    fn save_positions(&self, cx: &Context<Self>) {
        let Some(data_dir) = self.state.read(cx).data_dir.clone() else {
            return;
        };
        let positions: overview_positions::Positions = self
            .tile_positions
            .iter()
            .map(|(id, pos)| (id.clone(), (pos.x, pos.y)))
            .collect();
        if let Err(err) = overview_positions::save_positions(&data_dir, &positions) {
            tracing::warn!(error = %err, "saving overview tile positions failed");
        }
    }
}

impl Render for Overview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        self.ensure_prefs_loaded(cx);
        // The chat open in the side panel — or, in peek mode, the parent of
        // the subagent being peeked.
        self.panel_highlight = self
            .subagent_peek
            .as_ref()
            .map(|p| p.chat_id.clone())
            .or_else(|| self.panel_chat_id(cx));
        let rows = self.rows(cx);
        // Toolbar badge data — TTL-gated, so this is a no-op most frames.
        self.ensure_my_prs(cx);

        let toggle = |mode: ViewMode, label: String, current: ViewMode| {
            let active = mode == current;
            div()
                .id(SharedString::from(format!("overview-mode-{mode:?}")))
                .px(px(8.0))
                .py(px(3.0))
                .rounded(px(4.0))
                .cursor_pointer()
                .text_size(crate::typography::ui_rems(11.0))
                .when(active, |el| el.bg(theme.element_active).text_color(theme.text))
                .when(!active, |el| el.text_color(theme.text_muted))
                .child(SharedString::from(label))
        };

        // `updateMyPrsButtonBadge`: count + critical styling, except when
        // every actionable PR is only "ready to merge".
        let pr_action_count = actionable_pr_count(self.my_prs.iter().filter_map(|item| {
            item.detail.as_ref().map(|d| d.action_reasons())
        }).collect::<Vec<_>>().iter().map(Vec::as_slice));
        let my_prs_label = if pr_action_count > 0 {
            format!("My PRs ({pr_action_count})")
        } else {
            "My PRs".to_string()
        };
        let critical: gpui::Hsla = gpui::rgb(0xd03b3b).into();
        let new_chat_button = self.render_new_chat_button(&theme, cx);

        let title_row = div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(10.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(14.0))
                            .text_color(theme.text)
                            .child(SharedString::from("Overview")),
                    )
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(11.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(format!("{} chats", rows.len()))),
                    )
                    .child(
                        div()
                            .w(px(260.0))
                            .child(popover::search_input_frame(&theme, self.search.clone().into_any_element())),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    // Leftmost of the header's action cluster — the overview's
                    // one clearly-primary button (see `render_new_chat_button`).
                    .child(new_chat_button)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(2.0))
                            .p(px(2.0))
                            .rounded(px(6.0))
                            .bg(theme.element_hover.opacity(0.5))
                            .child(
                                toggle(ViewMode::List, "List".into(), self.view_mode).on_mouse_up(
                                    MouseButton::Left,
                                    cx.listener(|this, _, _, cx| {
                                        this.view_mode = ViewMode::List;
                                        cx.notify();
                                    }),
                                ),
                            )
                            .child(
                                toggle(ViewMode::Canvas, "Canvas".into(), self.view_mode).on_mouse_up(
                                    MouseButton::Left,
                                    cx.listener(|this, _, _, cx| {
                                        this.view_mode = ViewMode::Canvas;
                                        cx.notify();
                                    }),
                                ),
                            ),
                    )
                    .child(
                        // A sidebar toggle, not a view mode: its own segment so it
                        // reads as independent of List/Canvas.
                        div()
                            .p(px(2.0))
                            .rounded(px(6.0))
                            .bg(theme.element_hover.opacity(0.5))
                            .child(
                                div()
                                    .id("overview-my-prs-toggle")
                                    .px(px(8.0))
                                    .py(px(3.0))
                                    .rounded(px(4.0))
                                    .cursor_pointer()
                                    .text_size(crate::typography::ui_rems(11.0))
                                    .when(self.pr_sidebar_open, |el| el.bg(theme.element_active).text_color(theme.text))
                                    .when(!self.pr_sidebar_open, |el| el.text_color(theme.text_muted))
                                    // `#my-prs-btn.has-action`: critical color +
                                    // border + bold (regardless of open state).
                                    .when(pr_action_count > 0, |el| {
                                        el.text_color(critical)
                                            .font_weight(gpui::FontWeight::BOLD)
                                            .border_1()
                                            .border_color(critical)
                                    })
                                    .child(SharedString::from(my_prs_label))
                                    .on_mouse_up(
                                        MouseButton::Left,
                                        cx.listener(|this, _, _, cx| {
                                            let open = !this.pr_sidebar_open;
                                            this.set_pr_sidebar_open(open, true, cx);
                                        }),
                                    ),
                            ),
                    ),
            );

        let pill = |id: &'static str, label: String, active: bool| {
            div()
                .id(id)
                .px(px(8.0))
                .py(px(3.0))
                .rounded(px(4.0))
                .cursor_pointer()
                .text_size(crate::typography::ui_rems(11.0))
                .when(active, |el| el.bg(theme.element_active).text_color(theme.text))
                .when(!active, |el| el.text_color(theme.text_muted))
                .hover(|el| el.text_color(theme.text))
                .child(SharedString::from(label))
        };

        // "Hide stale" / "Show archived" — the reference's own default-off
        // toggles (`hideStale`/`showArchived`).
        let filters = div()
            .flex()
            .items_center()
            .gap(px(2.0))
            .p(px(2.0))
            .rounded(px(6.0))
            .bg(theme.element_hover.opacity(0.5))
            .child(pill("overview-hide-stale", "Hide stale".into(), self.hide_stale).on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.hide_stale = !this.hide_stale;
                    this.rows_dirty = true;
                    this.grouped_layout_cache = None;
                    cx.notify();
                }),
            ))
            .child(pill("overview-show-archived", "Show archived".into(), self.show_archived).on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.show_archived = !this.show_archived;
                    this.rows_dirty = true;
                    this.grouped_layout_cache = None;
                    cx.notify();
                }),
            ));

        let repo_filter = self.render_repo_filter(&theme, cx);
        let key_toggle = pill("overview-legend-toggle", "Key".into(), self.legend_open).on_mouse_up(
            MouseButton::Left,
            cx.listener(|this, _, _, cx| {
                this.legend_open = !this.legend_open;
                cx.notify();
            }),
        );

        let mut toolbar = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(6.0))
            .child(filters)
            .child(repo_filter)
            .child(key_toggle);
        {
            // Composable dimension chips: click toggles membership, order
            // clicked = nesting order (`self.active_groups`). A chip shows
            // its position in that order (1-based) while active, so the
            // nesting is legible without opening a menu.
            let dims: [(GroupDimension, &'static str); 4] = [
                (GroupDimension::Category, "Task"),
                (GroupDimension::Origin, "Origin"),
                (GroupDimension::Repo, "Repo"),
                (GroupDimension::Ticket, "PR / ticket"),
            ];
            let chips = dims.into_iter().map(|(dim, label)| {
                let order = self.active_groups.iter().position(|&d| d == dim);
                let active = order.is_some();
                let text = match order {
                    Some(ix) => format!("{label} {}", ix + 1),
                    None => label.to_string(),
                };
                div()
                    .id(SharedString::from(format!("overview-group-{label}")))
                    .px(px(7.0))
                    .py(px(2.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .text_size(crate::typography::ui_rems(10.5))
                    .when(active, |el| el.bg(theme.element_active).text_color(theme.text))
                    .when(!active, |el| el.text_color(theme.text_muted))
                    .child(SharedString::from(text))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.toggle_group(dim);
                            cx.notify();
                        }),
                    )
            });
            toolbar = toolbar.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(2.0))
                    .p(px(2.0))
                    .rounded(px(6.0))
                    .bg(theme.element_hover.opacity(0.3))
                    .child(
                        div()
                            .px(px(5.0))
                            .text_size(crate::typography::ui_rems(10.5))
                            .text_color(theme.text_muted.opacity(0.7))
                            .child(SharedString::from("Group by")),
                    )
                    .children(chips),
            );
        }

        let header = div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .px(px(16.0))
            .py(px(12.0))
            .child(title_row)
            .child(toolbar);

        let legend = self.legend_open.then(|| self.render_legend(&theme));

        if self.view_mode != ViewMode::Canvas {
            // Auto-fit is a canvas-only reaction to the edit that caused it —
            // never a deferred jump on a later mode switch.
            self.pending_fit_to_matches = false;
            // Re-entering canvas mode is a first appearance for every tile —
            // never animate from wherever they were before leaving.
            self.clear_tile_motion();
        }
        // Settle a PR-row jump (see `PendingFocus`): wait out the frame(s)
        // whose paint re-measures the canvas at its new width, then hand the
        // target to this frame's body renderer.
        if let Some(pending) = self.pending_focus.as_mut() {
            if pending.settle_frames > 0 {
                pending.settle_frames -= 1;
                window.request_animation_frame();
            } else {
                self.focus_due = self.pending_focus.take().map(|p| p.chat_id);
            }
        }
        // Built before the body, which consumes `rows`.
        let pr_sidebar = self.render_pr_sidebar(&rows, &theme, cx);
        let body = match self.view_mode {
            ViewMode::List => self.render_list_body(&rows, &theme, cx),
            ViewMode::Canvas => self.render_canvas_body(rows, &theme, cx),
        };
        // A focus the body couldn't use (target filtered out / not a row)
        // is dropped, never replayed on a later frame.
        self.focus_due = None;
        // Organize moves are hand-driven (see `tile_moves`): one frame at a
        // time while any is in flight. The pass that sees the last move
        // finish leaves `tile_moves` empty and schedules nothing, so an idle
        // canvas never keeps the window redrawing.
        if !self.tile_moves.is_empty() {
            window.request_animation_frame();
        }

        let main = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .child(header)
            .when_some(legend, |el, legend| el.child(legend))
            .child(body);

        // One right-side panel, two exclusive modes (see `subagent_peek`).
        let panel = if self.subagent_peek.is_some() {
            self.render_subagent_peek(&theme, cx)
        } else {
            self.render_chat_panel(&theme, cx)
        };

        // `[PR sidebar | main | chat panel]`: both side panels are
        // `flex_none` fixed widths and `main` is `flex_1().min_w_0()`, so
        // the list/canvas is what shrinks when either (or both) is open.
        div()
            .size_full()
            .flex()
            // Escape closes a subagent peek from anywhere in the overview
            // (the peek panel's own handler covers focus inside it; this
            // covers focus elsewhere here, e.g. the search box).
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" && this.subagent_peek.is_some() {
                    this.close_subagent_peek(cx);
                    cx.stop_propagation();
                }
            }))
            .when_some(pr_sidebar, |el, sidebar| el.child(sidebar))
            .child(main)
            .when_some(panel, |el, panel| el.child(panel))
    }
}

impl Overview {
    /// The overview's one clearly-primary action: reuses the exact same
    /// new-session flow as the titlebar `+` / `mod-n` (`Shell::open_new_session`,
    /// via the `open_new_session_from_overview` wrapper — `open_new_session`
    /// itself is private to `crate::shell`). Clicking it leaves the overview
    /// for the blank new-chat composer screen; that's intentional, since the
    /// project/device pickers and the worktree chip live there, not here.
    ///
    /// As the overview becomes home base, THIS is the intended anchor point
    /// for future overview-level new-chat UX (e.g. surfacing the new-chat
    /// screen's worktree chip or pickers without leaving the overview) —
    /// per the user's stated intent. A later pass may grow this into
    /// something richer in place; it should not need a second "new chat"
    /// entrypoint added elsewhere.
    fn render_new_chat_button(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let shortcut = self
            .shell
            .upgrade()
            .map(|shell| shell.read(cx).new_session_shortcut_label());
        let tooltip: SharedString = match shortcut {
            Some(combo) => format!("Start a new chat ({combo})").into(),
            None => "Start a new chat".into(),
        };
        popover::btn_primary(theme, "+ New chat")
            .id("overview-new-chat")
            .on_click(cx.listener(|this, _, _, cx| {
                let _ = this.shell.update(cx, |shell, cx| {
                    shell.open_new_session_from_overview(cx);
                });
            }))
            .tooltip(move |_, cx| cx.new(|_| LinkSourceTooltip(tooltip.clone())).into())
            .into_any_element()
    }

    /// "Repos" toolbar control + its dropdown (web `#repo-filter-btn`/
    /// `#repo-filter-panel`, `session_canvas.html:2082-2148`): every repo
    /// seen across rows (pre-filter, so unchecking one never hides its own
    /// checkbox), a checkbox + hash-color swatch per repo, All/None bulk
    /// actions. Closes on any press outside the card (the web's
    /// document-level click listener).
    fn render_repo_filter(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let hidden = self.hidden_repos.len();
        let active = hidden > 0 || self.repo_filter.is_open();
        let trigger = div()
            .id("overview-repo-filter")
            .relative()
            .px(px(8.0))
            .py(px(3.0))
            .rounded(px(4.0))
            .cursor_pointer()
            .text_size(crate::typography::ui_rems(11.0))
            .bg(theme.element_hover.opacity(0.5))
            .when(active, |el| el.bg(theme.element_active).text_color(theme.text))
            .when(!active, |el| el.text_color(theme.text_muted))
            .hover(|el| el.text_color(theme.text))
            .child(SharedString::from(format!("{} ▾", repo_filter_label(hidden))))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.repo_filter.note_trigger_press()),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                if this.repo_filter.take_press_was_open() {
                    this.close_repo_filter(cx);
                } else {
                    this.repo_filter.open(());
                    cx.notify();
                }
            }));
        if self.repo_filter.get().is_none() {
            return trigger.into_any_element();
        }
        let closing = self.repo_filter.closing_since();

        let popup = theme.for_popup();
        let bulk = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .px(px(8.0))
                .py(px(2.0))
                .rounded(px(4.0))
                .cursor_pointer()
                .text_size(crate::typography::ui_rems(10.5))
                .text_color(popup.text_muted)
                .border_1()
                .border_color(popup.border)
                .hover(|el| el.bg(popup.element_hover).text_color(popup.text))
                .child(SharedString::from(label))
        };
        let keys = sorted_repo_keys(&self.known_repos);
        let list: Vec<gpui::AnyElement> = if keys.is_empty() {
            vec![div()
                .px(px(6.0))
                .py(px(4.0))
                .text_size(crate::typography::ui_rems(11.0))
                .text_color(popup.text_muted)
                .child(SharedString::from("No chats yet"))
                .into_any_element()]
        } else {
            keys.into_iter()
                .enumerate()
                .map(|(ix, key)| {
                    let checked = !self.hidden_repos.contains(&key);
                    let color = group_color(GroupDimension::Repo, &key);
                    let label = group_label(GroupDimension::Repo, &key);
                    div()
                        .id(("overview-repo-filter-row", ix))
                        .flex()
                        .items_center()
                        .gap(px(7.0))
                        .px(px(6.0))
                        .py(px(3.0))
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .hover(|el| el.bg(popup.element_hover))
                        .child(
                            div()
                                .flex_none()
                                .size(px(12.0))
                                .rounded(px(3.0))
                                .border_1()
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_size(crate::typography::ui_rems(9.0))
                                .when(checked, |el| {
                                    el.bg(popup.accent).border_color(popup.accent).text_color(gpui::white()).child("✓")
                                })
                                .when(!checked, |el| el.border_color(popup.border_strong)),
                        )
                        .child(div().flex_none().size(px(8.0)).rounded(px(2.0)).bg(color))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(crate::typography::ui_rems(11.0))
                                .text_color(if checked { popup.text } else { popup.text_muted })
                                .child(SharedString::from(label)),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            toggle_repo_hidden(&mut this.hidden_repos, &key);
                            this.repo_filter_changed(cx);
                        }))
                        .into_any_element()
                })
                .collect()
        };
        let card = popover::popover_card(&popup)
            .w(px(240.0))
            .max_h(px(360.0))
            .flex()
            .flex_col()
            .gap(px(4.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_repo_filter(cx)))
            .child(
                div()
                    .flex()
                    .gap(px(6.0))
                    .pb(px(4.0))
                    .child(bulk("overview-repo-filter-all", "All").on_click(cx.listener(|this, _, _, cx| {
                        this.hidden_repos.clear();
                        this.repo_filter_changed(cx);
                    })))
                    .child(bulk("overview-repo-filter-none", "None").on_click(cx.listener(|this, _, _, cx| {
                        let known = this.known_repos.clone();
                        this.hidden_repos.extend(known);
                        this.repo_filter_changed(cx);
                    }))),
            )
            .child(
                div()
                    .id("overview-repo-filter-list")
                    .flex()
                    .flex_col()
                    .gap(px(1.0))
                    .overflow_y_scroll()
                    .children(list),
            );
        trigger
            .child(popover::anchored_menu_below("overview-repo-filter-menu", card.into_any_element(), closing))
            .into_any_element()
    }

    fn close_repo_filter(&mut self, cx: &mut Context<Self>) {
        if self.repo_filter.begin_close() {
            popover::reap_popup(cx, |this: &mut Self| &mut this.repo_filter);
        }
        cx.notify();
    }

    /// The "Key" legend (web `renderLegend`, `session_canvas.html:997-1038`)
    /// — inline panel under the toolbar, adapted to what this overview
    /// actually shows (no context-% sizing, no Cursor sessions, no Open in
    /// Terminal). The web animates it open via `max-height`; this just
    /// appears (no layout-height tween primitive worth the cost here).
    fn render_legend(&self, theme: &Theme) -> gpui::AnyElement {
        let swatch_row = |color: gpui::Hsla, text: String| {
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .text_size(crate::typography::ui_rems(10.5))
                .text_color(theme.text_muted)
                .child(div().flex_none().size(px(8.0)).rounded_full().bg(color))
                .child(SharedString::from(text))
                .into_any_element()
        };
        let group = |title: &'static str, rows: Vec<gpui::AnyElement>| {
            div()
                .flex()
                .flex_col()
                .gap(px(3.0))
                .min_w(px(130.0))
                .child(
                    div()
                        .pb(px(2.0))
                        .text_size(crate::typography::ui_rems(10.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.text)
                        .child(SharedString::from(title.to_uppercase())),
                )
                .children(rows)
        };
        let statuses: [(ChatIndicator, bool); 6] = [
            (ChatIndicator::AwaitingInput, false),
            (ChatIndicator::Working, false),
            (ChatIndicator::Completed, false),
            (ChatIndicator::Errored, false),
            (ChatIndicator::Idle, false),
            (ChatIndicator::Idle, true),
        ];
        let mut status_rows: Vec<gpui::AnyElement> = statuses
            .iter()
            .map(|&(status, not_running)| {
                let (icon, label) = status_style(status, not_running);
                swatch_row(reference_status_color(status), format!("{icon} {label}"))
            })
            .collect();
        status_rows.push(
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .text_size(crate::typography::ui_rems(10.5))
                .text_color(theme.text_muted)
                .child(
                    div()
                        .flex_none()
                        .relative()
                        .w(px(14.0))
                        .h(px(8.0))
                        .rounded(px(2.0))
                        .overflow_hidden()
                        .bg(theme.surface_raised)
                        .child(unread_done_ribbon(0.5)),
                )
                .child(SharedString::from("white left-edge ribbon: done, not looked at yet"))
                .into_any_element(),
        );
        let category_rows = overview_grouping::CATEGORY_ORDER
            .iter()
            .map(|key| swatch_row(category_color(key), category_label(key)))
            .collect();
        let origin_rows = overview_grouping::ORIGIN_ORDER
            .iter()
            .filter(|key| **key != "cursor")
            .map(|key| swatch_row(origin_color(key), origin_label(key)))
            .collect();
        let notes = [
            "Tile background tint = status color (same as the status dot) — a tinted tile scans faster than the dot alone.",
            "White left-edge ribbon = the chat finished and you haven't looked yet. Expanding the tile or opening its chat clears it; the next completion re-adds the ribbon.",
            "Dimmed tile = stale: no activity for 24h and not working / waiting on you. \"Hide stale\" filters them out.",
            "\"not running\" = an imported Claude Code session with no live run in Zeron right now. Its transcript is intact; sending a message resumes it.",
            "Badge row = linked Linear ticket and/or GitHub PR; a PR badge shows its most urgent action (CI failing, merge conflict, changes requested, needs reviewer, ready to merge).",
            "Links come from the branch (inferred), a PR created or mentioned in the chat, or you: \"Link PR / ticket…\" in the expanded detail (or the sidebar chat menu) takes a GitHub PR URL or a ticket id like ENG-1234. Hover a PR/ticket for its source; a pin marks manual links, and ✕ in the detail unlinks a stored one.",
            "\"Group by\" toggles combine — the first becomes columns, each further one subdivides into rows. Turn all off to return to your own layout.",
            "Click a tile or row = expand for branch/worktree, PR (reviewers, checks, review decision), ticket, last message, tools used, skills loaded, archive.",
            "Drag a tile (ungrouped canvas) = move it; positions are remembered across restarts.",
            "\"View full chat →\" = live chat side panel: read and reply without leaving the overview (the chat becomes the selected one). \"Open full chat →\" there jumps to the chat itself.",
            "Small tiles under a card = its subagents, all of them (dot: blue running, green done). Hover for the full name; click one to peek at its transcript in a read-only side panel (a subagent can't be continued). ✕ or Escape closes it.",
            "Archive hides a chat from the default view (same as the sidebar's archive). \"Show archived\" brings archived chats back, dimmed.",
            "\"Repos\" hides whole repos everywhere (canvas, list, grouping, search). Search matches title, branch, last message, folder, ticket id and PR title, and zooms the canvas to 1-8 matches.",
            "\"My PRs\" opens a left sidebar of every open PR you authored (\u{26a1} actionable first, then by repo) beside the list or canvas. Its count lights red when something needs you — CI failing, changes requested, a merge conflict, or no reviewer ever requested (ready-to-merge alone doesn't count). While it's open, search also filters PRs by repo, title, number (123 or #123) and branch. Clicking a PR with a linked chat opens that chat in the side panel and brings its tile/row into view; otherwise it opens the PR on GitHub.",
            "Canvas: scroll to pan, pinch or Ctrl+scroll to zoom, − / + to zoom around the center, click the % to reset, ⤢ to fit every card in view. Panned or zoomed away from every card? A \"Fit view\" pill appears — click it to snap back.",
        ];
        div()
            .flex_none()
            .mx(px(16.0))
            .mb(px(10.0))
            .p(px(12.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface_raised)
            .flex()
            .flex_wrap()
            .gap(px(24.0))
            .child(group("Status", status_rows))
            .child(group("Task category", category_rows))
            .child(group("Origin (lane colors)", origin_rows))
            .child(
                div()
                    .flex_1()
                    .min_w(px(280.0))
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .child(
                        div()
                            .pb(px(2.0))
                            .text_size(crate::typography::ui_rems(10.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child(SharedString::from("READING A TILE")),
                    )
                    .children(notes.iter().map(|note| {
                        div()
                            .text_size(crate::typography::ui_rems(10.5))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(*note))
                    })),
            )
            .into_any_element()
    }

    /// Web `#empty-message`, adapted: distinguishes "no chats at all" from
    /// "everything is filtered out" and says which filters are active.
    fn render_empty_state(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        // No chats at all (as opposed to "everything's filtered out") is
        // exactly when a prominent way to start one belongs right here —
        // same button, same action as the header's.
        let truly_empty = self.unfiltered_count == 0;
        let (headline, detail) = if truly_empty {
            (
                if self.show_archived { "No chats yet." } else { "No active chats." },
                None,
            )
        } else {
            let mut active: Vec<String> = Vec::new();
            if !self.search.read(cx).text().trim().is_empty() {
                active.push("search".into());
            }
            if !self.hidden_repos.is_empty() {
                active.push(repo_filter_label(self.hidden_repos.len()).to_lowercase());
            }
            if self.hide_stale {
                active.push("hide stale".into());
            }
            let detail = (!active.is_empty()).then(|| format!("Active filters: {}.", active.join(", ")));
            ("Every chat is hidden by the current filters.", detail)
        };
        let new_chat_button = truly_empty.then(|| self.render_new_chat_button(theme, cx));
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(4.0))
            .p(px(24.0))
            .child(
                div()
                    .text_size(crate::typography::ui_rems(13.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(headline)),
            )
            .when_some(detail, |el, detail| {
                el.child(
                    div()
                        .text_size(crate::typography::ui_rems(11.0))
                        .text_color(theme.text_faint)
                        .child(SharedString::from(detail)),
                )
            })
            .when_some(new_chat_button, |el, button| {
                el.child(div().mt(px(4.0)).child(button))
            })
            .into_any_element()
    }
}

/// How a card/row is dimmed and whether it carries the unread-done ribbon:
/// `(opacity, ribbon)`.
///
/// The ribbon replaced a white box-shadow halo: gpui's `opacity` multiplies
/// the alpha of EVERYTHING the element paints — its box-shadow included —
/// and a gpui shadow is a blurred filled rect painted *under* the card, so
/// on a translucent (dimmed) card the white halo both faded AND bled through
/// the card's own background as a milky wash. That doesn't apply to the
/// ribbon (it only ever shows at full opacity — see below), but the
/// precedence it sits under is unchanged. Dimmed + unread was also the
/// common case, not an edge case: the unread set starts empty, so every
/// never-opened done chat quiet for >`STALE_AFTER` was stale and unread at
/// once. Hence:
/// - unread-done beats stale: it's the newest, most attention-worthy
///   signal, so the card renders at full opacity with the ribbon showing;
///   once acknowledged, normal stale dimming returns.
/// - archived beats unread-done: the user explicitly put it away, so it
///   dims (0.4) and shows no ribbon at all (a faded, bleeding ribbon is
///   exactly the look being avoided).
fn card_dimming(archived: bool, stale: bool, unread_done: bool) -> (Option<f32>, bool) {
    if archived {
        return (Some(0.4), false);
    }
    if unread_done {
        return (None, true);
    }
    (stale.then_some(0.55), false)
}

/// Width (px) of the unread-done left-edge ribbon, replacing the old white
/// halo (`unread_done_glow`, a two-layer box-shadow — removed: it bled
/// through a dimmed card's translucent background as a milky wash, per
/// `card_dimming`'s doc comment). A solid edge bar has no such bleed
/// concern (the ribbon only ever renders at full opacity), reads cleanly
/// against any of the status-tinted card backgrounds (blue/green/red/
/// yellow), and against the green tint every done card already carries —
/// plain white is the safest choice against all of them, so that's the
/// color used at the call sites, at full opacity.
///
/// `scale` is the canvas zoom (`1.0` for list rows/legend), floored so the
/// ribbon stays visible zoomed out — same floor `unread_done_glow` used for
/// its blur/spread.
const UNREAD_DONE_RIBBON_WIDTH: f32 = 3.0;
const UNREAD_DONE_RIBBON_MIN_SCALE: f32 = 0.5;

fn unread_done_ribbon_width(scale: f32) -> f32 {
    UNREAD_DONE_RIBBON_WIDTH * scale.max(UNREAD_DONE_RIBBON_MIN_SCALE)
}

/// The ribbon itself: an absolutely-positioned bar flush along the left
/// edge, `top`/`bottom` pinned to `0` so it spans the card's full height.
/// Relies on the caller already being `overflow_hidden()` with rounded
/// corners (every caller here is) to clip it to the corner radius, rather
/// than rounding the bar's own corners — one clip mask instead of matching
/// radii by hand at every zoom level.
fn unread_done_ribbon(scale: f32) -> gpui::Div {
    div()
        .absolute()
        .top(px(0.0))
        .bottom(px(0.0))
        .left(px(0.0))
        .w(px(unread_done_ribbon_width(scale)))
        .bg(gpui::white())
}

/// `.lane-header` / `.lane-header.nested` (`session_canvas.html:299-315`):
/// text in the group's color, underlined in the same color; top level is
/// 13px bold uppercase with a 2px rule, nested is 10px semibold mixed-case
/// with a 1px rule.
fn lane_header(text: String, color: gpui::Hsla, depth: usize, zoom: f32) -> gpui::Div {
    let top = depth == 0;
    div()
        .flex_none()
        .text_color(color)
        .border_color(color)
        .when(top, |el| {
            el.text_size(crate::typography::ui_rems(13.0 * zoom))
                .font_weight(gpui::FontWeight::BOLD)
                .pb(px(6.0 * zoom))
                .border_b_2()
        })
        .when(!top, |el| {
            el.text_size(crate::typography::ui_rems(10.0 * zoom))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .pb(px(3.0 * zoom))
                .border_b_1()
        })
        .child(SharedString::from(if top { text.to_uppercase() } else { text }))
}

impl Overview {
    fn render_list_body(
        &mut self,
        rows: &[OverviewRow],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        if rows.is_empty() {
            return self.render_empty_state(theme, cx);
        }

        // PR-row jump target: the index among the scroll container's
        // DIRECT children (group headers included), which is what
        // `ScrollHandle::scroll_to_item` counts.
        let focus_id = self.focus_due.take();
        let mut focus_child: Option<usize> = None;
        let children: Vec<gpui::AnyElement> = if self.active_groups.is_empty() {
            focus_child = focus_id
                .as_deref()
                .and_then(|id| rows.iter().position(|r| r.chat.id == id));
            rows.iter()
                .enumerate()
                .map(|(ix, row)| self.render_list_row(ix, row, theme, cx))
                .collect()
        } else {
            let mut items: Vec<ListItem<'_>> = Vec::new();
            let refs: Vec<&OverviewRow> = rows.iter().collect();
            flatten_partition(&overview_grouping::partition(refs, &self.active_groups), 0, &mut items);
            let mut out = Vec::new();
            let mut ix = 0usize;
            for item in items {
                match item {
                    // Same header treatment as the canvas lane headers
                    // (`.lane-header` / `.lane-header.nested`): colored,
                    // underlined in its own color, uppercase+bold at the top
                    // level, smaller/thinner nested.
                    ListItem::Header { label, color, depth, count } => out.push(
                        div()
                            .flex()
                            .pl(px(10.0 + depth as f32 * 14.0))
                            .pr(px(10.0))
                            .pt(px(if depth == 0 { 14.0 } else { 8.0 }))
                            .pb(px(2.0))
                            .child(lane_header(
                                format!("{label} ({count})"),
                                color,
                                depth,
                                1.0,
                            ))
                            .into_any_element(),
                    ),
                    ListItem::Row(row) => {
                        if focus_id.as_deref() == Some(row.chat.id.as_str()) {
                            focus_child = Some(out.len());
                        }
                        out.push(self.render_list_row(ix, row, theme, cx));
                        ix += 1;
                    }
                }
            }
            out
        };
        if let Some(child) = focus_child {
            self.scroll.scroll_to_item(child);
        }

        div()
            .id("overview-list")
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(1.0))
            .px(px(8.0))
            .pb(px(8.0))
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .children(children)
            .into_any_element()
    }

    /// One List-mode row. Same click split as a canvas tile (web
    /// `renderTile`): clicking the row toggles its expanded detail in place
    /// (and acknowledges an unread done); the explicit "View full chat →"
    /// control opens the interactive chat panel. Neither navigates away
    /// — "Open full chat →" in that panel does.
    fn render_list_row(
        &mut self,
        ix: usize,
        row: &OverviewRow,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let now = Utc::now();
        let chat_id = row.chat.id.clone();
        let title: SharedString = row
            .chat
            .title
            .clone()
            .unwrap_or_else(|| "New session".to_string())
            .into();
        let cwd: SharedString = row
            .chat
            .cwd
            .clone()
            .unwrap_or_else(|| "No folder".to_string())
            .into();
        let when: SharedString = row
            .chat
            .last_message_at
            .map(|at| format_time_ago(at, now))
            .unwrap_or_else(|| format_time_ago(row.chat.created_at, now))
            .into();
        let dot = reference_status_color(row.status);
        let (icon, label) = status_style(row.status, row.origin.is_some());
        let unread = is_unread_done(&self.acknowledged_done, &chat_id, row.status == ChatIndicator::Completed);
        let (dim, ribbon) = card_dimming(row.chat.archived, row.stale, unread);
        let is_expanded = self.expanded.contains(&chat_id);
        let category_badge = category_badge(row, 1.0);
        let badges = self.badges_for(&chat_id, theme, 1.0);
        let archive = (!row.chat.archived).then(|| self.archive_button(chat_id.clone(), theme, cx, 1.0));
        let open_button = self.open_chat_button(chat_id.clone(), theme, cx, 1.0);
        let subagent_tiles = self.subagent_tiles_for(&chat_id, theme, 1.0, cx);
        let detail = is_expanded.then(|| self.expanded_detail_for(row, theme, chat_id.clone(), 1.0, cx));
        let toggle_id = chat_id.clone();
        div()
            .id(("overview-row", ix))
            .debug_selector(|| "overview-list-row".into())
            .relative()
            // Never shrink inside the scrolling list column. The row is
            // `overflow_hidden` (below, for the ribbon), which zeroes its
            // automatic flex min-height — so without this, once the list
            // outgrows the viewport every row got squeezed toward 0 and the
            // squeezed part (the subagent rows at the bottom first) was
            // clipped away instead of the list scrolling.
            .flex_none()
            .flex()
            .flex_col()
            .rounded(px(6.0))
            // Clips the unread-done ribbon (an absolutely-positioned child
            // below) to the row's own rounded corner instead of squaring it
            // off; see `unread_done_ribbon`. A list row is normally
            // background-less and no longer needs an opaque plate now that
            // there's no halo to ring — the ribbon just paints on top.
            .overflow_hidden()
            .border_1()
            .border_color(gpui::transparent_black())
            // The chat open in the side panel.
            .when(self.panel_highlight.as_deref() == Some(chat_id.as_str()), |el| {
                el.border_color(theme.accent)
            })
            .when(is_expanded, |el| el.bg(theme.element_hover.opacity(0.5)))
            .when_some(dim, |el, opacity| el.opacity(opacity))
            .hover(|el| el.bg(theme.element_hover))
            .when(ribbon, |el| el.child(unread_done_ribbon(1.0)))
            .child(
                div()
                    .id(("overview-row-click", ix))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .px(px(10.0))
                    .py(px(8.0))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_expanded(&toggle_id, cx);
                        cx.notify();
                    }))
                    .child(div().flex_none().size(px(7.0)).rounded_full().bg(dot))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(div().truncate().text_color(theme.text).child(title))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.0))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .text_size(crate::typography::ui_rems(11.0))
                                            .text_color(theme.text_muted.opacity(0.7))
                                            .child(cwd),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_size(crate::typography::ui_rems(10.0))
                                            .text_color(dot)
                                            .child(SharedString::from(format!("{icon} {label}"))),
                                    )
                                    .when_some(category_badge, |el, badge| el.child(badge)),
                            )
                            .when_some(badges, |el, badges| el.child(badges)),
                    )
                    // Same bar + label as the tile face, at a fixed width so
                    // the column lines up down the list.
                    .child(div().flex_none().w(px(76.0)).child(context_meter(row.context_pct, theme, 1.0)))
                    .child(
                        div()
                            .flex_none()
                            .text_size(crate::typography::ui_rems(10.0))
                            .text_color(theme.text_muted.opacity(0.6))
                            .child(when),
                    )
                    .child(open_button)
                    .when_some(archive, |el, archive| el.child(archive)),
            )
            .when(!subagent_tiles.is_empty(), |el| {
                el.child(
                    div()
                        .debug_selector(|| "overview-list-subagents".into())
                        .flex()
                        .flex_col()
                        .gap(px(SUBAGENT_TILE_GAP))
                        .pb(px(8.0))
                        .pl(px(27.0))
                        .pr(px(10.0))
                        .children(subagent_tiles),
                )
            })
            .when_some(detail, |el, detail| el.child(div().px(px(10.0)).pb(px(8.0)).child(detail)))
            .into_any_element()
    }

    /// One tile's content — status color, title, cwd, relative time, PR/
    /// ticket badges, dimmed when stale, carrying a white left-edge ribbon
    /// while an unacknowledged done, then the parent's stack of compact
    /// subagent tiles under the face. The only place a later phase should
    /// need to touch to add more per-chat detail: append a child here, not
    /// to the drag/pan/zoom plumbing around it. `open_button` (the face's
    /// "View full chat →" control) and `subagent_tiles`
    /// (`subagent_tiles_for`) are built by the caller, which has the `cx`
    /// for their listeners.
    ///
    /// `zoom` scales every size/spacing value in here proportionally (List
    /// mode passes `1.0`, which has no zoom concept; Canvas mode passes
    /// `self.canvas_zoom`). gpui's `Transformation` (used for icon rotation
    /// elsewhere in this codebase) is an SVG-element-only primitive — not a
    /// general subtree transform — so there's no way to scale a whole
    /// rendered `div()` tree for free here; scaling each value explicitly is
    /// the real fix, not a workaround. This only touches rendered sizes, not
    /// the tile's screen position/hit-testing math (computed separately in
    /// the canvas layout functions), so click/drag behavior is unaffected.
    fn tile_body(
        &self,
        row: &OverviewRow,
        theme: &Theme,
        now: chrono::DateTime<Utc>,
        zoom: f32,
        open_button: gpui::AnyElement,
        subagent_tiles: Vec<gpui::AnyElement>,
    ) -> gpui::AnyElement {
        let title: SharedString = row
            .chat
            .title
            .clone()
            .unwrap_or_else(|| "New session".to_string())
            .into();
        let cwd: SharedString = row
            .chat
            .cwd
            .as_deref()
            .map(shorten_cwd)
            .unwrap_or_else(|| "No folder".to_string())
            .into();
        let when: SharedString = row
            .chat
            .last_message_at
            .map(|at| format_time_ago(at, now))
            .unwrap_or_else(|| format_time_ago(row.chat.created_at, now))
            .into();
        let dot = reference_status_color(row.status);
        let (icon, label) = status_style(row.status, row.origin.is_some());
        let unread = is_unread_done(
            &self.acknowledged_done,
            &row.chat.id,
            row.status == ChatIndicator::Completed,
        );
        let (dim, ribbon) = card_dimming(row.chat.archived, row.stale, unread);
        // Status color as a whole-tile background tint, not just the small
        // dot — ports `renderTile`'s `color-mix(in srgb, ${st.color} 14%,
        // surface)` (its own code comment argues this "reads at a glance
        // across a wall of tiles far better than a small status dot alone,"
        // directly relevant at Zeron's real scale). `Hsla::blend` at a low
        // source alpha is the same math as a CSS color-mix against the base.
        let tinted_bg = theme.surface_raised.blend(dot.opacity(0.14));
        // Border tinted by status too, not just the background — exact port
        // of `renderTile`'s `borderColor: color-mix(in srgb, ${st.color} 30%,
        // var(--border))`.
        let tinted_border = theme.border.blend(dot.opacity(0.30));
        let badges = self.badges_for(&row.chat.id, theme, zoom);
        let category_badge = category_badge(row, zoom);
        let is_expanded = self.expanded.contains(&row.chat.id);
        div()
            .debug_selector(|| "overview-tile-body".into())
            .size_full()
            .flex()
            .flex_col()
            .gap(px(3.0 * zoom))
            .p(px(10.0 * zoom))
            .rounded(px(8.0 * zoom))
            // Belt-and-suspenders on top of `tile_size`'s content-height
            // estimate: any residual mis-estimate (an unusually long title
            // wrap, a font-metrics difference, etc.) clips at the tile's own
            // rounded corner instead of spilling past its background into
            // whatever's rendered next, which is the visible symptom the
            // estimate exists to prevent in the first place.
            .overflow_hidden()
            .border_1()
            .border_color(tinted_border)
            // The chat open in the side panel.
            .when(self.panel_highlight.as_deref() == Some(row.chat.id.as_str()), |el| {
                el.border_color(theme.accent)
            })
            .bg(tinted_bg)
            .shadow_sm()
            .hover(|el| el.border_color(dot.opacity(0.6)))
            // Reference's exact `.card.archived { opacity: 0.4 }` /
            // `.card.stale { opacity: 0.55 }` — except an unread-done card,
            // which stays at full opacity until acknowledged (`card_dimming`).
            .when_some(dim, |el, opacity| el.opacity(opacity))
            // `.card.unread-done` — a bright left-edge ribbon, no border
            // change (the web changes no border either); see
            // `unread_done_ribbon`.
            .when(ribbon, |el| el.child(unread_done_ribbon(zoom)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0 * zoom))
                    .child(div().flex_none().size(px(7.0 * zoom)).rounded_full().bg(dot))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(theme.text)
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_size(crate::typography::ui_rems(13.0 * zoom))
                            .child(title),
                    ),
            )
            .child(
                div()
                    .truncate()
                    .text_size(crate::typography::ui_rems(10.5 * zoom))
                    .text_color(theme.text_muted.opacity(0.7))
                    .child(cwd),
            )
            // `.fill-bar` + `.pct-label`, right under the title block like
            // the web's tile face.
            .child(context_meter(row.context_pct, theme, zoom).mt(px(2.0 * zoom)))
            // `.status-row`: dot color + icon + label, then (Zeron-only)
            // relative time and the category badge.
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0 * zoom))
                    .text_size(crate::typography::ui_rems(10.0 * zoom))
                    .child(
                        div()
                            .flex_none()
                            .text_color(dot)
                            .child(SharedString::from(format!("{icon} {label}"))),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme.text_muted.opacity(0.6))
                            .child(when),
                    )
                    .child(div().flex_1())
                    .when_some(category_badge, |el, badge| el.child(badge)),
            )
            .when_some(badges, |el, badges| el.child(badges))
            .child(
                div()
                    .flex_1()
                    .flex()
                    .items_end()
                    .justify_between()
                    .gap(px(6.0 * zoom))
                    .child(open_button)
                    // Exact port of `renderTile`'s `<div class="expand-hint">
                    // click for more details</div>` — shown only when NOT
                    // already expanded, same as the reference.
                    .when(!is_expanded, |el| {
                        el.child(
                            div()
                                .text_size(crate::typography::ui_rems(9.0 * zoom))
                                .text_color(theme.text_muted.opacity(0.4))
                                .child(SharedString::from("click for more details")),
                        )
                    }),
            )
            // `.connector > .subagents`: the vertical stack of compact
            // subagent tiles under the face. Exactly
            // `subagent_stack_height` tall (fixed-height tiles, fixed gaps,
            // `mt` + this column's own `TILE_ROW_GAP` = the connector's
            // 10px), so `tile_size` reserved precisely this.
            .when(!subagent_tiles.is_empty(), |el| {
                el.child(
                    div()
                        .debug_selector(|| "overview-tile-subagents".into())
                        .flex_none()
                        .mt(px((SUBAGENT_STACK_TOP - TILE_ROW_GAP) * zoom))
                        .flex()
                        .flex_col()
                        .gap(px(SUBAGENT_TILE_GAP * zoom))
                        .children(subagent_tiles),
                )
            })
            .into_any_element()
    }

    /// Spatial tile board: absolute-positioned tiles over a pannable/
    /// zoomable canvas surface. Drag idiom follows the pane-resize-handle
    /// pattern already used in `shell.rs` (`on_mouse_down` arms a live drag,
    /// `on_mouse_move`/`on_mouse_up` on the canvas root track and finish it)
    /// rather than gpui's `on_drag`/`on_drag_move` ghost-payload machinery —
    /// simpler for continuous free-position dragging where the moving
    /// element IS the payload, not a proxy.
    ///
    /// Zoom is applied to tile position/size at layout time (not a real
    /// content-scale transform — nothing else in this codebase uses one on
    /// arbitrary content, and fighting gpui for that wasn't worth it here);
    /// visually indistinguishable from a transform for this purpose.
    fn render_canvas_body(
        &mut self,
        rows: Vec<OverviewRow>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        if rows.is_empty() {
            self.pending_fit_to_matches = false;
            self.clear_tile_motion();
            return self.render_empty_state(theme, cx);
        }
        if self.active_groups.is_empty() {
            self.render_canvas_flat(rows, theme, cx)
        } else {
            self.render_canvas_grouped(rows, theme, cx)
        }
    }

    /// Ungrouped canvas: free-drag tiles over a pannable/zoomable surface —
    /// the original single-scatter layout, unchanged.
    fn render_canvas_flat(
        &mut self,
        rows: Vec<OverviewRow>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        self.ensure_positions_loaded(cx);
        // Every currently on-canvas tile's board-space rect (post-filter,
        // flat mode's own positions) — feeds both the search auto-fit below
        // and the "Fit all"/off-screen-rescue controls in the zoom cluster.
        let all_rects: Vec<(f32, f32, f32, f32)> = rows
            .iter()
            .enumerate()
            .map(|(slot, row)| {
                let pos = self.position_for(&row.chat.id, slot);
                let (w, h) = row.tile_size();
                (pos.x, pos.y, w, h)
            })
            .collect();
        if self.take_pending_fit(rows.len(), cx) {
            self.fit_view(&all_rects);
        }
        if let Some(focus_id) = self.focus_due.take()
            && let Some((slot, row)) = rows.iter().enumerate().find(|(_, r)| r.chat.id == focus_id)
        {
            let pos = self.position_for(&focus_id, slot);
            let (w, h) = row.tile_size();
            self.focus_view((pos.x, pos.y, w, h));
        }
        let now = Utc::now();
        let zoom = self.canvas_zoom;
        let (pan_x, pan_y) = self.canvas_pan;

        // gpui has no z-index/stacking-context primitive at all (confirmed
        // against its own source — `Interactivity` exposes nothing like it;
        // paint order is purely child-array order, later paints over
        // earlier). The reference tolerates an expanded/interacted card
        // overlapping its neighbors by bumping its DOM z-index
        // (`session_canvas.html`'s `makeCard`, `card.style.zIndex = ++zTop`)
        // — the gpui equivalent of "paint on top" is "render last", so this
        // collects `(is_expanded, element)` and stable-sorts expanded tiles
        // to the end of the child list right before the previously-reported
        // "expanded content hidden behind the tile below it" bug.
        let anim_now = Instant::now();
        let reduced = cx.reduce_motion();
        let present_ids: Vec<String> = rows.iter().map(|r| r.chat.id.clone()).collect();
        // Tracks whether ANY tile intersected the viewport this frame, for
        // the off-screen rescue pill — set from the exact same
        // `tile_in_viewport` call viewport culling already makes below,
        // against each tile's CURRENT animated position, so an in-flight
        // organize move can't produce a false "all off-screen" reading (see
        // `render_rescue_pill`'s doc comment).
        let mut any_tile_visible = false;
        let mut tiles: Vec<(bool, _)> = rows
            .into_iter()
            .enumerate()
            .filter_map(|(slot, row)| {
                let chat_id = row.chat.id.clone();
                let target = self.position_for(&chat_id, slot);
                let pos = self.animated_tile_pos(&chat_id, target, anim_now, reduced);
                let screen_x = pos.x * zoom + pan_x;
                let screen_y = pos.y * zoom + pan_y;
                let (board_w, board_h) = row.tile_size();
                let (tile_w, tile_h) = (board_w * zoom, board_h * zoom);
                // Skip building this tile's element at all when off-screen —
                // see `tile_in_viewport`'s doc comment for why this has to
                // happen before `tile_body` runs, not just before paint. An
                // expanded tile's real height exceeds `tile_h`; the margin
                // comfortably covers that without tracking exact expanded
                // height here.
                let in_viewport = self.tile_in_viewport(screen_x, screen_y, tile_w, tile_h);
                if in_viewport {
                    any_tile_visible = true;
                }
                if !in_viewport {
                    return None;
                }
                let open_button = self.open_chat_button(chat_id.clone(), theme, cx, zoom);
                let subagent_tiles = self.subagent_tiles_for(&chat_id, theme, zoom, cx);
                let content = self.tile_body(&row, theme, now, zoom, open_button, subagent_tiles);
                let archive = (!row.chat.archived)
                    .then(|| self.archive_button(chat_id.clone(), theme, cx, zoom));
                let is_expanded = self.expanded.contains(&chat_id);
                let detail = is_expanded.then(|| {
                    self.expanded_detail_for(&row, theme, chat_id.clone(), zoom, cx)
                });
                let drag_id = chat_id.clone();
                let element =
                div()
                    .id(("overview-tile", slot))
                    .absolute()
                    .left(px(screen_x))
                    .top(px(screen_y))
                    .w(px(tile_w))
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            let cursor = (f32::from(event.position.x), f32::from(event.position.y));
                            let pos = this
                                .tile_positions
                                .get(&drag_id)
                                .copied()
                                .unwrap_or(TilePos { x: 0.0, y: 0.0 });
                            let tile_screen = (
                                pos.x * this.canvas_zoom + this.canvas_pan.0,
                                pos.y * this.canvas_zoom + this.canvas_pan.1,
                            );
                            let offset = (cursor.0 - tile_screen.0, cursor.1 - tile_screen.1);
                            this.dragging_tile = Some(TileDrag {
                                chat_id: drag_id.clone(),
                                offset,
                                down_cursor: cursor,
                                committed: false,
                            });
                        }),
                    )
                    .child(div().h(px(tile_h)).child(content))
                    .when_some(archive, |el, archive| {
                        el.child(div().absolute().top(px(4.0)).right(px(4.0)).child(archive))
                    })
                    .when_some(detail, |el, detail| el.child(detail));
                Some((is_expanded, element))
            })
            .collect();
        self.prune_tile_motion(&present_ids.iter().map(String::as_str).collect());
        tiles.sort_by_key(|(is_expanded, _)| *is_expanded);
        let tiles: Vec<_> = tiles.into_iter().map(|(_, element)| element).collect();
        // Rescue pill only once tiles actually exist and none of them
        // intersected the viewport this frame — never for an empty/filtered
        // board (that's the ordinary empty state, not "off-screen"). See
        // `no_tiles_visible`'s doc comment.
        let rescue_pill = no_tiles_visible(present_ids.len(), any_tile_visible)
            .then(|| self.render_rescue_pill(theme, cx, present_ids.len(), all_rects.clone()));

        div()
            .id("overview-canvas")
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .bg(theme.surface)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _, _cx| {
                    // Background press (tiles already stopped propagation on
                    // their own mouse-down) — arm a pan drag.
                    this.panning = Some((
                        (f32::from(event.position.x), f32::from(event.position.y)),
                        this.canvas_pan,
                    ));
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                let cursor = (f32::from(event.position.x), f32::from(event.position.y));
                if let Some(drag) = this.dragging_tile.as_mut() {
                    let zoom = this.canvas_zoom;
                    // Board-space cumulative movement (screen delta / zoom) —
                    // matches the reference's own `(clientX - startX) / zoom`
                    // scaling so the threshold feels the same at any zoom.
                    let moved_x = (cursor.0 - drag.down_cursor.0) / zoom;
                    let moved_y = (cursor.1 - drag.down_cursor.1) / zoom;
                    if !drag.committed
                        && (moved_x.abs() >= DRAG_THRESHOLD_PX || moved_y.abs() >= DRAG_THRESHOLD_PX)
                    {
                        drag.committed = true;
                    }
                    if drag.committed {
                        let chat_id = drag.chat_id.clone();
                        let offset = drag.offset;
                        let screen = (cursor.0 - offset.0, cursor.1 - offset.1);
                        let logical = (
                            (screen.0 - this.canvas_pan.0) / zoom,
                            (screen.1 - this.canvas_pan.1) / zoom,
                        );
                        this.tile_positions.insert(
                            chat_id,
                            TilePos {
                                x: logical.0,
                                y: logical.1,
                            },
                        );
                    }
                    cx.notify();
                } else if let Some((start_cursor, start_pan)) = this.panning {
                    this.canvas_pan = (
                        start_pan.0 + (cursor.0 - start_cursor.0),
                        start_pan.1 + (cursor.1 - start_cursor.1),
                    );
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                    // Resolved here, on release, matching the reference's own
                    // `onUp` — never on a separate racing `on_click` handler
                    // (see `dragging_tile`'s doc comment for the bug this
                    // replaced). Committed (moved past threshold) = real
                    // drag, persist the new position. Not committed = a
                    // click, toggle expand in place — never navigates.
                    if let Some(drag) = this.dragging_tile.take() {
                        if drag.committed {
                            this.save_positions(cx);
                        } else {
                            this.toggle_expanded(&drag.chat_id, cx);
                        }
                    }
                    this.panning = None;
                    cx.notify();
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                    // A release outside the canvas bounds only counts as a
                    // drag-save, never a click-to-expand (mirrors treating an
                    // off-element release as "cancel the gesture" rather than
                    // activating it).
                    if let Some(drag) = this.dragging_tile.take()
                        && drag.committed
                    {
                        this.save_positions(cx);
                    }
                    this.panning = None;
                    cx.notify();
                }),
            )
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                let delta = event.delta.pixel_delta(px(1.0));
                if event.modifiers.control {
                    // Ctrl+scroll zooms toward the cursor — matches
                    // `session_canvas.html`'s `exp(-deltaY * 0.01)` curve
                    // exactly (`~1657`).
                    let cursor = (f32::from(event.position.x), f32::from(event.position.y));
                    let factor = (-f32::from(delta.y) * 0.01).exp();
                    this.zoom_toward(this.canvas_zoom * factor, cursor);
                } else {
                    // Plain scroll pans (`~1659-1660`). Sign matches macOS
                    // natural-scrolling: content follows the finger/swipe
                    // direction, not the viewport.
                    this.canvas_pan.0 += f32::from(delta.x);
                    this.canvas_pan.1 += f32::from(delta.y);
                }
                cx.notify();
            }))
            .on_pinch(cx.listener(|this, event: &PinchEvent, _, cx| {
                // Native trackpad pinch — the primary, natural-feeling zoom
                // trigger on macOS. `event.delta` is already a usable
                // fractional zoom change (0.1 == 10%), unlike a browser's
                // Ctrl+wheel translation of the same gesture, so this reuses
                // `zoom_toward` directly rather than going through the
                // wheel-delta curve above. Ctrl+scroll stays as a secondary
                // trigger for non-trackpad input; both share the same
                // zoom_toward, so they can never disagree on the math.
                let cursor = (f32::from(event.position.x), f32::from(event.position.y));
                this.zoom_toward(this.canvas_zoom * (1.0 + event.delta), cursor);
                cx.notify();
            }))
            .children(tiles)
            .child(self.bounds_capture())
            .child(self.render_zoom_controls(theme, cx, all_rects))
            .when_some(rescue_pill, |el, pill| el.child(pill))
            .into_any_element()
    }

    /// Grouped canvas: tiles partitioned by `self.active_groups` (composable,
    /// same `overview_grouping::partition` List mode's grouped rendering
    /// uses), laid out via `layout_partition` — real free-rectangle packing
    /// per leaf group (`overview_layout::pack_rects`), not a fixed grid,
    /// nested columns/rows alternating by depth for multi-dimension
    /// composition. A final GLOBAL `overview_layout::repair_overlaps` sweep
    /// runs across every leaf's already-placed absolute position, matching
    /// the reference's two-tier repair (local packing repair, then a
    /// cross-group safety net — `overview_layout`'s own doc comment).
    /// Positions are recomputed fresh every render from group membership —
    /// deliberately NOT stored in `self.tile_positions` (that map is
    /// flat-mode's free-drag/persisted state; mixing the two would mean a
    /// tile keeps a stale absolute position from a previous grouping/drag
    /// when regions shift). Tiles are click-to-open only here, no drag — a
    /// per-tile drag would need per-region bounds clamping to mean anything,
    /// real scope this pass doesn't need; panning and zooming the whole
    /// canvas still work, same as flat mode.
    fn render_canvas_grouped(
        &mut self,
        rows: Vec<OverviewRow>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let now = Utc::now();

        let by_id: HashMap<String, &OverviewRow> =
            rows.iter().map(|r| (r.chat.id.clone(), r)).collect();

        // Packing + the global repair sweep are real work (free-rectangle
        // packing per group, then an O(n) overlap check across every tile)
        // and their OUTPUT depends only on which chat ids are present and
        // `active_groups` — never on `canvas_pan`/`canvas_zoom`, which apply
        // afterward as a pure `to_screen` transform. Without this cache, a
        // pan/zoom/drag frame (which calls `cx.notify()` on every single
        // pointer-move) reran the whole computation from scratch — the
        // confirmed primary cause of the reported sluggishness once this
        // view had 227+ real tiles to lay out.
        let key_ids: Vec<String> = rows.iter().map(|r| r.chat.id.clone()).collect();
        // Tile sizes feed packing, so they're part of the key too — belt-
        // and-suspenders on top of `ensure_context_usage`/`ensure_subagents`/
        // `ensure_link_status` dropping the cache themselves on a real
        // change. Full `(width, height)`, not width alone: height can now
        // change independent of width (subagent count changing, a
        // badge row appearing/disappearing — see `GroupedLayoutCache::key_sizes`'s
        // doc comment), so a width-only key would miss exactly that case.
        let key_sizes: Vec<(f32, f32)> = rows.iter().map(|r| r.tile_size()).collect();
        let cache_hit = self.grouped_layout_cache.as_ref().is_some_and(|c| {
            c.key_ids == key_ids && c.key_sizes == key_sizes && c.key_groups == self.active_groups
        });
        if !cache_hit {
            let refs: Vec<&OverviewRow> = rows.iter().collect();
            let tree = overview_grouping::partition(refs, &self.active_groups);
            let laid_out = layout_partition(&tree, 0);

            // Global second-tier repair: every leaf's already-locally-packed
            // absolute position, combined, re-checked for cross-group overlap
            // (a per-group pack alone can't catch two different groups' boxes
            // drifting into each other).
            let positioned: Vec<overview_layout::PositionedItem> = laid_out
                .positions
                .iter()
                .map(|(id, (x, y))| {
                    let (width, height) = by_id
                        .get(id)
                        .map(|r| r.tile_size())
                        .unwrap_or_else(|| tile_size_for_pct(None));
                    overview_layout::PositionedItem {
                        id: id.clone(),
                        x: *x,
                        y: *y,
                        width,
                        height,
                    }
                })
                .collect();
            let repaired = overview_layout::repair_overlaps(&positioned, TILE_GAP);
            self.grouped_layout_cache = Some(GroupedLayoutCache {
                key_ids,
                key_sizes,
                key_groups: self.active_groups.clone(),
                laid_out,
                repaired,
            });
        }
        // Cloned out (cheap — a plain HashMap/Vec copy, not a recompute) so
        // the borrow of `self.grouped_layout_cache` ends here: the render
        // loop below calls `self.tile_body`/`self.expanded_detail_for`/etc.,
        // which would otherwise conflict with an outstanding borrow through
        // `cache`.
        let cache = self
            .grouped_layout_cache
            .as_ref()
            .expect("just populated on a miss, present on a hit");
        let laid_out = cache.laid_out.clone();
        let repaired = cache.repaired.clone();

        // Every currently on-canvas tile's board-space rect (post-filter,
        // grouped mode's own packed positions) — feeds both the search
        // auto-fit below and the "Fit all"/off-screen-rescue controls in the
        // zoom cluster.
        let all_rects: Vec<(f32, f32, f32, f32)> = repaired
            .iter()
            .filter_map(|(id, p)| {
                let (w, h) = by_id.get(id)?.tile_size();
                Some((p.x, p.y, w, h))
            })
            .collect();
        if self.take_pending_fit(rows.len(), cx) {
            self.fit_view(&all_rects);
        }
        if let Some(focus_id) = self.focus_due.take()
            && let (Some(p), Some(row)) = (repaired.get(&focus_id), by_id.get(&focus_id))
        {
            let (w, h) = row.tile_size();
            self.focus_view((p.x, p.y, w, h));
        }
        let zoom = self.canvas_zoom;
        let (pan_x, pan_y) = self.canvas_pan;
        let to_screen = |x: f32, y: f32| (x * zoom + pan_x, y * zoom + pan_y);

        let mut elements: Vec<gpui::AnyElement> = Vec::new();
        // Visual region per group — a background tint + border so groups
        // read as distinct blocks/sections at a glance, not just a floating
        // label with nothing delineating where one group ends and the next
        // begins. Shallower (outer) groups first so nested boxes paint on
        // top of their parent's, matching normal containment expectations.
        let mut boxes: Vec<_> = laid_out.group_boxes.iter().collect();
        boxes.sort_by_key(|(_, _, _, _, depth)| *depth);
        // `.group-box` / `.group-box.nested` (`session_canvas.html:316-326`):
        // tinted in the group's own color (border 32%/26%, fill 6%/5%),
        // radius 14 top-level / 8 nested, padded `GROUP_BOX_PADDING` beyond
        // the content (`makeGroupBox`). gpui has no fractional border
        // widths, so the web's 1.5px top-level border rounds to 2px.
        for (local_x, local_y, w, h, depth) in boxes {
            let (screen_x, screen_y) =
                to_screen(*local_x - GROUP_BOX_PADDING, *local_y - GROUP_BOX_PADDING);
            let color = laid_out
                .labels
                .iter()
                .find(|(_, _, lx, ly, ldepth)| lx == local_x && ly == local_y && ldepth == depth)
                .map(|(_, color, ..)| *color)
                .unwrap_or(theme.border);
            let top = *depth == 0;
            elements.push(
                div()
                    .id(("overview-group-box", elements.len()))
                    .absolute()
                    .left(px(screen_x))
                    .top(px(screen_y))
                    .w(px((*w + GROUP_BOX_PADDING * 2.0) * zoom))
                    .h(px((*h + GROUP_BOX_PADDING * 2.0) * zoom))
                    .rounded(px(if top { 14.0 } else { 8.0 } * zoom))
                    .when(top, |el| el.border_2().border_color(color.opacity(0.32)).bg(color.opacity(0.06)))
                    .when(!top, |el| el.border_1().border_color(color.opacity(0.26)).bg(color.opacity(0.05)))
                    .into_any_element(),
            );
        }
        // Lane headers paint after every box (web: z-index 50+depth, always
        // above any box).
        for (label, color, local_x, local_y, depth) in &laid_out.labels {
            let (screen_x, screen_y) = to_screen(*local_x, *local_y);
            elements.push(
                div()
                    .id(("overview-group-label", elements.len()))
                    .absolute()
                    .left(px(screen_x))
                    .top(px(screen_y))
                    .child(lane_header(label.clone(), *color, *depth, zoom))
                    .into_any_element(),
            );
        }
        // Same paint-order fix as flat mode (see that function's comment):
        // gpui has no z-index primitive, so an expanded tile's extra height
        // is rendered last among the tiles instead, so it paints over
        // whichever neighbor its packed position happens to overlap. Real
        // height-aware repacking (feeding the expanded tile's actual taller
        // box into `pack_rects` so neighbors get pushed instead of
        // overlapped at all) would need `self.expanded` in the cache key
        // above plus feeding the expanded height (not just the collapsed
        // context-% size) through `pack_rects`/`repair_overlaps` — a real,
        // separate scope of work, not done here; this is the same tolerance the
        // reference itself uses (z-index bump, not a reflow), not a lesser
        // substitute.
        let anim_now = Instant::now();
        let reduced = cx.reduce_motion();
        let mut tiles: Vec<(bool, gpui::AnyElement)> = Vec::new();
        // See flat mode's `any_tile_visible` for why this is driven by the
        // same per-tile `tile_in_viewport` call culling already makes below
        // (current animated position, not a separate target-position check)
        // — it's what decides the off-screen rescue pill.
        let mut any_tile_visible = false;
        for (slot, (chat_id, packed)) in repaired.iter().enumerate() {
            let Some(&row) = by_id.get(chat_id) else {
                continue; // shouldn't happen — every id in `positions` came from `rows`
            };
            let pos = self.animated_tile_pos(
                chat_id,
                TilePos {
                    x: packed.x,
                    y: packed.y,
                },
                anim_now,
                reduced,
            );
            let (screen_x, screen_y) = to_screen(pos.x, pos.y);
            let (board_w, board_h) = row.tile_size();
            let (tile_w, tile_h) = (board_w * zoom, board_h * zoom);
            // Same viewport culling as flat mode — see `tile_in_viewport`'s
            // doc comment.
            let in_viewport = self.tile_in_viewport(screen_x, screen_y, tile_w, tile_h);
            if in_viewport {
                any_tile_visible = true;
            }
            if !in_viewport {
                continue;
            }
            let open_button = self.open_chat_button(chat_id.clone(), theme, cx, zoom);
            let subagent_tiles = self.subagent_tiles_for(chat_id, theme, zoom, cx);
            let content = self.tile_body(row, theme, now, zoom, open_button, subagent_tiles);
            let archive = (!row.chat.archived).then(|| self.archive_button(chat_id.clone(), theme, cx, zoom));
            let is_expanded = self.expanded.contains(chat_id);
            let detail = is_expanded.then(|| self.expanded_detail_for(row, theme, chat_id.clone(), zoom, cx));
            let toggle_id = chat_id.clone();
            tiles.push((
                is_expanded,
                div()
                    .id(("overview-grouped-tile", slot))
                    .absolute()
                    .left(px(screen_x))
                    .top(px(screen_y))
                    .w(px(tile_w))
                    .cursor_pointer()
                    // No drag here (positions are algorithm-computed, not
                    // user-placed — see this fn's own doc comment), so a
                    // plain click always toggles expand; no threshold needed.
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_expanded(&toggle_id, cx);
                        cx.notify();
                    }))
                    .child(div().h(px(tile_h)).child(content))
                    .when_some(archive, |el, archive| {
                        el.child(div().absolute().top(px(4.0)).right(px(4.0)).child(archive))
                    })
                    .when_some(detail, |el, detail| el.child(detail))
                    .into_any_element(),
            ));
        }
        self.prune_tile_motion(&repaired.keys().map(String::as_str).collect());
        tiles.sort_by_key(|(is_expanded, _)| *is_expanded);
        elements.extend(tiles.into_iter().map(|(_, element)| element));
        // Rescue pill only once tiles actually exist and none of them
        // intersected the viewport this frame — never for an empty/filtered
        // board (that's the ordinary empty state, not "off-screen"). See
        // `no_tiles_visible`'s doc comment.
        let rescue_pill = no_tiles_visible(repaired.len(), any_tile_visible)
            .then(|| self.render_rescue_pill(theme, cx, repaired.len(), all_rects.clone()));

        div()
            .id("overview-canvas-grouped")
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .bg(theme.surface)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _, _cx| {
                    this.panning = Some((
                        (f32::from(event.position.x), f32::from(event.position.y)),
                        this.canvas_pan,
                    ));
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                if let Some((start_cursor, start_pan)) = this.panning {
                    let cursor = (f32::from(event.position.x), f32::from(event.position.y));
                    this.canvas_pan = (
                        start_pan.0 + (cursor.0 - start_cursor.0),
                        start_pan.1 + (cursor.1 - start_cursor.1),
                    );
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                    this.panning = None;
                    cx.notify();
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                    this.panning = None;
                    cx.notify();
                }),
            )
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                let delta = event.delta.pixel_delta(px(1.0));
                if event.modifiers.control {
                    let cursor = (f32::from(event.position.x), f32::from(event.position.y));
                    let factor = (-f32::from(delta.y) * 0.01).exp();
                    this.zoom_toward(this.canvas_zoom * factor, cursor);
                } else {
                    this.canvas_pan.0 += f32::from(delta.x);
                    this.canvas_pan.1 += f32::from(delta.y);
                }
                cx.notify();
            }))
            .on_pinch(cx.listener(|this, event: &PinchEvent, _, cx| {
                let cursor = (f32::from(event.position.x), f32::from(event.position.y));
                this.zoom_toward(this.canvas_zoom * (1.0 + event.delta), cursor);
                cx.notify();
            }))
            .children(elements)
            .child(self.bounds_capture())
            .child(self.render_zoom_controls(theme, cx, all_rects))
            .when_some(rescue_pill, |el, pill| el.child(pill))
            .into_any_element()
    }

    /// Left "My PRs" sidebar — `Some` only while `pr_sidebar_open`. Every
    /// open PR the signed-in `gh` account authored, across every repo it can
    /// see, in `renderPrPane`'s two-part shape (`session_canvas.html`): an
    /// actionable-first rollup ("⚡ Needs your action", anything with a
    /// non-empty `action_reasons()`, severity-sorted), then every PR grouped
    /// by repo, repos alphabetical. The toolbar search filters both parts
    /// ([`pr_matches_search`]). Header/border treatment mirrors the chat
    /// panel's (mirrored to the left edge). A PR with no linked chat opens
    /// on GitHub instead of jumping, matching the reference's "no open chat
    /// for this PR" case.
    fn render_pr_sidebar(
        &mut self,
        rows: &[OverviewRow],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        if !self.pr_sidebar_open {
            return None;
        }
        self.ensure_my_prs(cx);
        let query = self.search.read(cx).text().trim().to_lowercase();

        // Clone out of `self.my_prs` up front — everything below needs
        // `&mut self` (badge/jump lookups reuse `self.link_status`), which a
        // borrow tied to `self.my_prs` would conflict with.
        let all_items: Vec<MyPrItem> = self
            .my_prs
            .iter()
            .filter(|item| pr_matches_search(&query, item))
            .cloned()
            .collect();
        let total = self.my_prs.len();

        let count_label = if query.is_empty() || total == 0 {
            format!("{total}")
        } else {
            format!("{} of {total}", all_items.len())
        };
        let header = div()
            .flex_none()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(8.0))
            .px(px(12.0))
            .py(px(10.0))
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .min_w_0()
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.text)
                            .child(SharedString::from("My PRs")),
                    )
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(10.5))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(count_label)),
                    ),
            )
            .child(
                div()
                    .id("overview-pr-sidebar-close")
                    .cursor_pointer()
                    .text_color(theme.text_muted)
                    .child(SharedString::from("×"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.set_pr_sidebar_open(false, true, cx);
                    })),
            );

        let body: gpui::AnyElement = if all_items.is_empty() {
            let message = if total == 0 {
                if self.my_prs_pending && self.my_prs_fetched_at.is_none() {
                    "Loading…".to_string()
                } else {
                    "No open PRs found (or `gh` isn't installed/authenticated).".to_string()
                }
            } else {
                format!("No PRs match \u{201c}{query}\u{201d}.")
            };
            div()
                .p(px(12.0))
                .text_size(crate::typography::ui_rems(11.5))
                .text_color(theme.text_faint)
                .child(SharedString::from(message))
                .into_any_element()
        } else {
            let mut actionable: Vec<MyPrItem> = all_items
                .iter()
                .filter(|item| {
                    item.detail
                        .as_ref()
                        .is_some_and(|d| !d.action_reasons().is_empty())
                })
                .cloned()
                .collect();
            actionable.sort_by_key(|item| {
                item.detail
                    .as_ref()
                    .and_then(|d| d.action_reasons().into_iter().next())
                    .map(pr_action_severity)
                    .unwrap_or(99)
            });

            let mut by_repo: std::collections::BTreeMap<String, Vec<MyPrItem>> =
                std::collections::BTreeMap::new();
            for item in &all_items {
                by_repo
                    .entry(item.summary.repo.clone().unwrap_or_else(|| "unknown".to_string()))
                    .or_default()
                    .push(item.clone());
            }

            // Element ids must be unique across sections (the same PR shows
            // in the rollup AND its repo group), so a running index.
            let mut next_ix = 0usize;
            let mut sections: Vec<gpui::AnyElement> = Vec::new();
            if !actionable.is_empty() {
                sections.push(self.render_pr_section(
                    format!("⚡ Needs your action ({})", actionable.len()),
                    &actionable,
                    &mut next_ix,
                    rows,
                    theme,
                    cx,
                ));
            }
            for (repo, items) in &by_repo {
                let label = format!("{} ({})", repo.rsplit('/').next().unwrap_or(repo), items.len());
                sections.push(self.render_pr_section(label, items, &mut next_ix, rows, theme, cx));
            }
            div()
                .id("overview-pr-sidebar-list")
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .px(px(6.0))
                .pb(px(8.0))
                .overflow_y_scroll()
                .track_scroll(&self.pr_sidebar_scroll)
                .children(sections)
                .into_any_element()
        };

        Some(
            div()
                .id("overview-pr-sidebar")
                .flex_none()
                .w(px(PR_SIDEBAR_W))
                .h_full()
                .flex()
                .flex_col()
                .border_r_1()
                .border_color(theme.border)
                .bg(theme.surface_raised)
                .child(header)
                .child(body)
                .into_any_element(),
        )
    }

    fn render_pr_section(
        &mut self,
        label: String,
        items: &[MyPrItem],
        next_ix: &mut usize,
        rows: &[OverviewRow],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let children: Vec<gpui::AnyElement> = items
            .iter()
            .map(|item| {
                let ix = *next_ix;
                *next_ix += 1;
                self.render_pr_item(ix, item, rows, theme, cx)
            })
            .collect();
        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .px(px(10.0))
                    .pt(px(10.0))
                    .pb(px(2.0))
                    .text_size(crate::typography::ui_rems(10.5))
                    .text_color(theme.text_muted.opacity(0.7))
                    .child(SharedString::from(label)),
            )
            .children(children)
            .into_any_element()
    }

    fn render_pr_item(
        &mut self,
        ix: usize,
        item: &MyPrItem,
        rows: &[OverviewRow],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let title: SharedString = item
            .summary
            .title
            .clone()
            .unwrap_or_else(|| format!("#{}", item.summary.number))
            .into();
        let number_label = format!("#{}", item.summary.number);
        let state_label: SharedString = if item.summary.is_draft {
            "draft".into()
        } else {
            item.detail
                .as_ref()
                .map(|d| d.state.clone())
                .unwrap_or_else(|| "open".to_string())
                .into()
        };
        let action_badge = item
            .detail
            .as_ref()
            .and_then(|d| d.action_reasons().into_iter().next())
            .map(pr_action_badge);
        let jump = self.chat_for_pr_url(rows, item.summary.url.as_deref());
        let url = item.summary.url.clone();

        let mut row = div()
            .id(("overview-pr-item", ix))
            .flex()
            .flex_col()
            .gap(px(3.0))
            .px(px(10.0))
            .py(px(8.0))
            .rounded(px(6.0))
            .cursor_pointer()
            .hover(|el| el.bg(theme.element_hover))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(div().flex_1().min_w_0().truncate().text_color(theme.text).child(title))
                    .when_some(action_badge, |el, (label, color)| {
                        el.child(
                            div()
                                .flex_none()
                                .px(px(6.0))
                                .py(px(1.0))
                                .rounded(px(3.0))
                                .bg(color.opacity(0.15))
                                .text_size(crate::typography::ui_rems(9.5))
                                .text_color(color)
                                .child(SharedString::from(label)),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .text_size(crate::typography::ui_rems(10.5))
                    .text_color(theme.text_muted.opacity(0.7))
                    .child(SharedString::from(number_label))
                    .child(SharedString::from("·"))
                    .child(SharedString::from(state_label)),
            );

        if let Some((chat_id, chat_title)) = jump {
            let pr_source = self.link_status.get(&chat_id).and_then(|e| e.status.pr_source);
            row = row.child(
                link_source_hover(
                    div()
                        .id(("overview-pr-item-chat", ix))
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .text_size(crate::typography::ui_rems(10.0))
                        .text_color(theme.accent),
                    LinkKind::Pr,
                    pr_source,
                )
                .when(pr_source == Some(ChatLinkSource::Manual), |el| el.child(manual_link_pin(theme.accent, 1.0)))
                .child(SharedString::from(format!("→ {chat_title}"))),
            );
            // Jump in place: chat panel on the right + bring its tile/row
            // into view — never navigates off the overview.
            row = row.on_click(cx.listener(move |this, _, _, cx| {
                this.jump_to_chat_from_pr(chat_id.clone(), cx);
            }));
        } else if let Some(url) = url {
            row = row.child(
                div()
                    .text_size(crate::typography::ui_rems(10.0))
                    .text_color(theme.text_muted.opacity(0.5))
                    .child(SharedString::from("No open chat for this PR — click to open on GitHub")),
            );
            row = row.on_click(move |_, _, cx| {
                cx.open_url(&url);
            });
        }

        row.into_any_element()
    }
}

/// `ACTION_SEVERITY` (`session_canvas.html`) — lower sorts first.
fn pr_action_severity(action: PrAction) -> u8 {
    match action {
        PrAction::CiFailing | PrAction::MergeConflict => 0,
        PrAction::ChangesRequested => 1,
        PrAction::NeedsReviewer => 2,
        PrAction::ReadyToMerge => 3,
    }
}

#[cfg(test)]
mod logic_tests {
    use super::*;

    fn pr(url: &str) -> ParsedLink {
        ParsedLink { kind: LinkKind::Pr, value: url.to_string() }
    }

    fn ticket(id: &str) -> ParsedLink {
        ParsedLink { kind: LinkKind::Ticket, value: id.to_string() }
    }

    #[test]
    fn link_input_parses_github_pr_urls_and_normalizes() {
        let canonical = "https://github.com/cofactr/zeron/pull/123";
        for raw in [
            "https://github.com/cofactr/zeron/pull/123",
            "  https://github.com/cofactr/zeron/pull/123  ",
            "http://github.com/cofactr/zeron/pull/123",
            "github.com/cofactr/zeron/pull/123",
            "https://www.github.com/cofactr/zeron/pull/123",
            "https://github.com/cofactr/zeron/pull/123/files",
            "https://github.com/cofactr/zeron/pull/123?foo=bar",
            "https://github.com/cofactr/zeron/pull/123#discussion_r1",
            "https://GitHub.com/cofactr/zeron/pull/123",
        ] {
            assert_eq!(parse_link_input(raw), Ok(pr(canonical)), "{raw:?}");
        }
    }

    #[test]
    fn link_input_parses_ticket_ids_and_linear_urls() {
        assert_eq!(parse_link_input("ENG-1234"), Ok(ticket("ENG-1234")));
        assert_eq!(parse_link_input(" eng-1234 "), Ok(ticket("ENG-1234")));
        assert_eq!(parse_link_input("ab-1"), Ok(ticket("AB-1")));
        assert_eq!(
            parse_link_input("https://linear.app/cofactr/issue/ENG-42/fix-login"),
            Ok(ticket("ENG-42"))
        );
    }

    #[test]
    fn link_input_rejects_garbage() {
        for raw in [
            "",
            "   ",
            "#123",
            "123",
            "hello world",
            "ENG-",
            "ENG1234",
            "E-1",              // one letter
            "ABCDEFG-1",        // seven letters
            "ENG-12a",
            "ENG-1234 extra",
            "https://github.com/cofactr/zeron",
            "https://github.com/cofactr/zeron/issues/5",
            "https://github.com/cofactr/zeron/pull/",
            "https://github.com/cofactr/zeron/pull/12x",
            "https://gitlab.com/cofactr/zeron/pull/1",
            "https://linear.app/cofactr/project/abc",
        ] {
            assert_eq!(parse_link_input(raw), Err(LINK_PARSE_ERROR), "{raw:?}");
        }
    }

    #[test]
    fn link_source_labels_cover_every_provenance() {
        use ChatLinkSource::*;
        assert_eq!(link_source_label(LinkKind::Pr, Some(Manual)), "Linked manually");
        assert_eq!(link_source_label(LinkKind::Ticket, Some(Manual)), "Linked manually");
        assert_eq!(link_source_label(LinkKind::Pr, Some(CreatedInChat)), "PR created in this chat");
        assert_eq!(link_source_label(LinkKind::Pr, Some(Mentioned)), "Mentioned in conversation");
        assert_eq!(link_source_label(LinkKind::Ticket, Some(Mentioned)), "Mentioned in conversation");
        assert_eq!(link_source_label(LinkKind::Pr, None), "Inferred from branch");
        assert_eq!(link_source_label(LinkKind::Ticket, None), "Inferred from branch");
    }

    /// `CHAT_LINK_STATUS` wire shape with the new source fields — decoded
    /// straight into `ChatLinkStatus` by `ensure_link_status`; snake_case
    /// source values, `null` = inferred, and an older engine that omits them
    /// still decodes.
    #[test]
    fn chat_link_status_decodes_source_fields() {
        let status: ChatLinkStatus = serde_json::from_value(serde_json::json!({
            "pr": null, "ticket": null, "isWorktree": false, "diffStat": null,
            "prSource": "created_in_chat", "ticketSource": "manual"
        }))
        .unwrap();
        assert_eq!(status.pr_source, Some(ChatLinkSource::CreatedInChat));
        assert_eq!(status.ticket_source, Some(ChatLinkSource::Manual));
        let legacy: ChatLinkStatus = serde_json::from_value(serde_json::json!({
            "pr": null, "ticket": null, "isWorktree": false, "diffStat": null
        }))
        .unwrap();
        assert_eq!(legacy.pr_source, None);
        assert_eq!(legacy.ticket_source, None);
    }

    #[test]
    fn set_chat_link_reply_applies_optimistically_and_merges_refetch() {
        let reply: SetChatLinkReply = serde_json::from_value(serde_json::json!({
            "linkedPrUrl": "https://github.com/a/b/pull/7", "linkedPrSource": "manual",
            "linkedTicketId": null, "linkedTicketSource": null
        }))
        .unwrap();
        let mut status = ChatLinkStatus::default();
        apply_link_reply(&mut status, LinkKind::Pr, &reply);
        let placeholder = status.pr.as_ref().unwrap();
        assert_eq!(placeholder.number, 7);
        assert!(placeholder.action_reasons().is_empty(), "placeholder must not badge");
        assert_eq!(status.pr_source, Some(ChatLinkSource::Manual));

        // Refetch lands before the engine cached the PR detail: keep ours.
        let refetched = ChatLinkStatus { pr_source: Some(ChatLinkSource::Manual), ..Default::default() };
        let merged = merge_refetched_link(Some(&status), refetched);
        assert_eq!(merged.pr.as_ref().map(|p| p.number), Some(7));

        // Ticket set, then cleared.
        let set = SetChatLinkReply {
            linked_ticket_id: Some("ENG-9".into()),
            linked_ticket_source: Some(ChatLinkSource::Manual),
            ..Default::default()
        };
        apply_link_reply(&mut status, LinkKind::Ticket, &set);
        assert_eq!(status.ticket.as_ref().map(|t| t.identifier.as_str()), Some("ENG-9"));
        apply_link_reply(&mut status, LinkKind::Ticket, &SetChatLinkReply::default());
        assert!(status.ticket.is_none());
        assert_eq!(status.ticket_source, None);

        // Cleared (no source) refetch never resurrects the old value.
        let merged = merge_refetched_link(Some(&status), ChatLinkStatus::default());
        assert!(merged.pr.is_none());
    }

    /// The panel follows the selection and closes itself when it's gone.
    #[test]
    fn panel_chat_id_follows_selection_only_while_open() {
        assert_eq!(panel_chat_id(false, Some("a")), None);
        assert_eq!(panel_chat_id(true, Some("a")), Some("a".to_string()));
        assert_eq!(panel_chat_id(true, None), None);
        assert_eq!(panel_chat_id(true, Some("")), None);
    }

    /// `ensure_subagents` decodes `SCAN_CHAT_SUBAGENTS`'s reply straight into
    /// `SubagentItem`; a key-casing drift would silently yield no rows
    /// (the decode error is swallowed). Payload shape captured verbatim from
    /// a live engine reply, plus the pinned `status` field.
    #[test]
    fn scan_chat_subagents_wire_payload_decodes() {
        let wire = serde_json::json!([
            {
                "agentId": "a21f1650e0d5284f6",
                "description": "Research slash and @ mention semantics",
                "agentType": "fork",
                "transcriptPath": "/p/8de11ea8/subagents/agent-a21f1650e0d5284f6.jsonl",
                "status": "running"
            },
            { "agentId": "b", "transcriptPath": "/p/b.jsonl", "status": "done" },
            // Pre-`status` engine / unknown value: settled default.
            { "agentId": "c", "agentType": "Explore", "transcriptPath": "/p/c.jsonl" },
            { "agentId": "d", "transcriptPath": "/p/d.jsonl", "status": "exploded" }
        ]);
        let decoded: Vec<SubagentItem> = serde_json::from_value(wire).unwrap();
        assert_eq!(decoded.len(), 4);
        assert_eq!(decoded[0].agent_id, "a21f1650e0d5284f6");
        assert_eq!(decoded[0].agent_type.as_deref(), Some("fork"));
        assert_eq!(decoded[0].status, SubagentStatus::Running);
        assert_eq!(decoded[0].display_name(), "Research slash and @ mention semantics");
        assert_eq!(decoded[1].description, None);
        assert_eq!(decoded[1].status, SubagentStatus::Done);
        // Web name fallback: description || agent_type || id.
        assert_eq!(decoded[1].display_name(), "b");
        assert_eq!(decoded[2].display_name(), "Explore");
        assert_eq!(decoded[2].status, SubagentStatus::Done);
        assert_eq!(decoded[3].status, SubagentStatus::Done);
        assert_eq!(SubagentStatus::Running.color(), reference_status_color(ChatIndicator::Working));
        assert_eq!(SubagentStatus::Done.color(), reference_status_color(ChatIndicator::Completed));
    }

    /// The pinned `READ_SUBAGENT_TRANSCRIPT` reply shape decodes, with the
    /// nullable fields (`timestamp`, `resultPreview`, `model`) and missing
    /// optional ones tolerated.
    #[test]
    fn read_subagent_transcript_wire_payload_decodes() {
        let wire = serde_json::json!({
            "turns": [
                {"role": "user", "text": "Find the bug\nin foo.rs", "timestamp": "2026-09-29T10:15:00Z", "tools": []},
                {"role": "assistant", "text": "Looking.", "timestamp": null, "tools": [
                    {"name": "Read", "inputPreview": "foo.rs", "resultPreview": "fn foo() {}"},
                    {"name": "Bash", "inputPreview": "cargo test", "resultPreview": null}
                ]},
                {"role": "assistant", "text": "", "timestamp": null, "tools": []}
            ],
            "model": "claude-opus-5-5"
        });
        let t: SubagentTranscript = serde_json::from_value(wire).unwrap();
        assert_eq!(t.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(t.turns.len(), 3);
        assert_eq!(t.turns[0].role, "user");
        assert_eq!(t.turns[0].text.as_deref(), Some("Find the bug\nin foo.rs"));
        assert_eq!(t.turns[0].timestamp.as_deref(), Some("2026-09-29T10:15:00Z"));
        assert!(!format_turn_time(t.turns[0].timestamp.as_deref()).is_empty());
        assert_eq!(t.turns[1].timestamp, None);
        assert_eq!(format_turn_time(None), "");
        assert_eq!(format_turn_time(Some("not a time")), "");
        assert_eq!(
            t.turns[1].tools,
            vec![
                TranscriptTool { name: "Read".into(), input_preview: "foo.rs".into(), result_preview: Some("fn foo() {}".into()) },
                TranscriptTool { name: "Bash".into(), input_preview: "cargo test".into(), result_preview: None },
            ]
        );
        // `model: null` and an empty object both decode.
        let t: SubagentTranscript = serde_json::from_value(serde_json::json!({"turns": [], "model": null})).unwrap();
        assert_eq!(t, SubagentTranscript::default());
        let t: SubagentTranscript = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(t.turns.is_empty());
        assert_eq!(turn_role_label("user"), "You");
        assert_eq!(turn_role_label("assistant"), "Assistant");
    }

    #[test]
    fn peek_reply_guard_drops_stale_and_closed_requests() {
        let peek = |request_id| SubagentPeek {
            chat_id: "c".into(),
            agent_id: "a".into(),
            name: "n".into(),
            agent_type: None,
            request_id,
            load: PeekLoad::Loading,
            open_tools: HashSet::new(),
        };
        // The current request lands.
        assert!(peek_reply_is_current(Some(&peek(2)), 2));
        // An earlier request superseded by a quicker click (id 1 → 2) is dropped.
        assert!(!peek_reply_is_current(Some(&peek(2)), 1));
        // Closed since: nothing to land into.
        assert!(!peek_reply_is_current(None, 2));
    }

    #[test]
    fn fenced_code_splits_out_of_plain_text() {
        assert_eq!(split_fenced_code("just text\nline 2"), vec![(false, "just text\nline 2".to_string())]);
        assert_eq!(
            split_fenced_code("before\n```rust\nfn a() {}\n```\nafter"),
            vec![
                (false, "before".to_string()),
                (true, "fn a() {}".to_string()),
                (false, "after".to_string()),
            ]
        );
        // Unterminated fence runs to the end.
        assert_eq!(split_fenced_code("```\nx\ny"), vec![(true, "x\ny".to_string())]);
        assert!(split_fenced_code("").is_empty());
    }

    #[test]
    fn repo_key_matches_reference_repo_group_key() {
        assert_eq!(repo_key(Some("/Users/m/Projects/zeron")), "zeron");
        assert_eq!(repo_key(Some("/Users/m/Projects/zeron/")), "zeron");
        assert_eq!(repo_key(Some("/Users/m/Projects/zeron///")), "zeron");
        assert_eq!(repo_key(Some("relative")), "relative");
        assert_eq!(repo_key(Some("/")), overview_grouping::NONE_KEY);
        assert_eq!(repo_key(Some("")), overview_grouping::NONE_KEY);
        assert_eq!(repo_key(None), overview_grouping::NONE_KEY);
    }

    #[test]
    fn repo_key_resolves_worktree_via_workspace_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".workspace-root"),
            "/Users/m/Projects/agent-mode-tools\n",
        )
        .unwrap();
        let cwd = dir.path().to_str().unwrap();
        assert_eq!(repo_key(Some(cwd)), "agent-mode-tools");

        // Trailing slash on the recorded root is stripped like a cwd's.
        std::fs::write(dir.path().join(".workspace-root"), "/Users/m/Projects/zeron/\n").unwrap();
        assert_eq!(repo_key(Some(cwd)), "zeron");
    }

    #[test]
    fn repo_key_workspace_root_empty_or_missing_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let basename = dir.path().file_name().unwrap().to_str().unwrap();

        // No file: unchanged behavior.
        assert_eq!(repo_key(Some(cwd)), basename);

        // Empty / whitespace-only file: fallback.
        std::fs::write(dir.path().join(".workspace-root"), "").unwrap();
        assert_eq!(repo_key(Some(cwd)), basename);
        std::fs::write(dir.path().join(".workspace-root"), "  \n").unwrap();
        assert_eq!(repo_key(Some(cwd)), basename);

        // Unreadable (a directory, not a file): fallback, no panic.
        std::fs::remove_file(dir.path().join(".workspace-root")).unwrap();
        std::fs::create_dir(dir.path().join(".workspace-root")).unwrap();
        assert_eq!(repo_key(Some(cwd)), basename);
    }

    #[test]
    fn ticket_group_key_prefers_ticket_then_pr_then_none() {
        assert_eq!(ticket_group_key(Some("ENG-1"), Some(7)), "ENG-1");
        assert_eq!(ticket_group_key(None, Some(7)), "PR #7");
        assert_eq!(ticket_group_key(None, None), overview_grouping::NONE_KEY);
    }

    #[test]
    fn repo_filter_toggle_all_none_and_label() {
        let known: BTreeSet<String> = ["ui", "zeron", overview_grouping::NONE_KEY]
            .into_iter()
            .map(String::from)
            .collect();
        let mut hidden = BTreeSet::new();
        assert_eq!(repo_filter_label(hidden.len()), "Repos");

        toggle_repo_hidden(&mut hidden, "ui");
        assert!(hidden.contains("ui"));
        assert_eq!(repo_filter_label(hidden.len()), "Repos (1 hidden)");
        toggle_repo_hidden(&mut hidden, "ui");
        assert!(hidden.is_empty());

        // "None" hides every known repo; "All" clears.
        hidden.extend(known.iter().cloned());
        assert_eq!(repo_filter_label(hidden.len()), "Repos (3 hidden)");
        hidden.clear();
        assert_eq!(repo_filter_label(hidden.len()), "Repos");
    }

    #[test]
    fn repo_list_is_alphabetical_with_none_last() {
        let known: BTreeSet<String> = [overview_grouping::NONE_KEY, "zeron", "agent-mode-tools", "ui"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            sorted_repo_keys(&known),
            vec!["agent-mode-tools", "ui", "zeron", overview_grouping::NONE_KEY]
        );
    }

    #[test]
    fn search_is_case_insensitive_substring_over_present_fields() {
        let fields = [Some("Fix Login Bug"), None, Some("/Users/m/ui"), Some("ENG-42")];
        assert!(matches_search("", &fields));
        assert!(matches_search("login", &fields));
        assert!(matches_search("eng-42", &fields));
        assert!(matches_search("users/m", &fields));
        assert!(!matches_search("zeron", &fields));
        assert!(!matches_search("x", &[None, None]));
    }

    #[test]
    fn badge_count_skips_empty_and_ready_to_merge_only() {
        let none: Vec<PrAction> = vec![];
        let ready = vec![PrAction::ReadyToMerge];
        let failing = vec![PrAction::CiFailing];
        let mixed = vec![PrAction::NeedsReviewer, PrAction::ReadyToMerge];
        let reasons: Vec<&[PrAction]> = vec![&none, &ready, &failing, &mixed];
        assert_eq!(actionable_pr_count(reasons), 2);

        let only_ready: Vec<&[PrAction]> = vec![&ready, &ready];
        assert_eq!(actionable_pr_count(only_ready), 0);
        assert_eq!(actionable_pr_count(Vec::<&[PrAction]>::new()), 0);
    }

    #[test]
    fn fit_view_frames_bbox_with_padding_and_centers() {
        // One 260x118 tile at (100, 50) in a 1000x800 viewport.
        let (zoom, pan) = fit_view_to_rects(&[(100.0, 50.0, 260.0, 118.0)], (1000.0, 800.0)).unwrap();
        // content = 260+160 x 118+160 = 420 x 278 → min(1000/420, 800/278)
        // = 2.38 → clamped to ZOOM_MAX.
        assert_eq!(zoom, ZOOM_MAX);
        let (cx, cy) = (100.0 + 130.0, 50.0 + 59.0);
        assert!((pan.0 - (500.0 - cx * zoom)).abs() < 1e-3);
        assert!((pan.1 - (400.0 - cy * zoom)).abs() < 1e-3);
        // The bbox midpoint lands on the viewport center.
        assert!((cx * zoom + pan.0 - 500.0).abs() < 1e-3);
        assert!((cy * zoom + pan.1 - 400.0).abs() < 1e-3);
    }

    #[test]
    fn fit_view_clamps_to_min_zoom_and_handles_empty() {
        assert!(fit_view_to_rects(&[], (1000.0, 800.0)).is_none());
        let far = [(0.0, 0.0, 10.0, 10.0), (20_000.0, 0.0, 10.0, 10.0)];
        let (zoom, _) = fit_view_to_rects(&far, (1000.0, 800.0)).unwrap();
        assert_eq!(zoom, ZOOM_MIN);
        // A moderate spread fits exactly (width-bound).
        let spread = [(0.0, 0.0, 100.0, 100.0), (740.0, 0.0, 100.0, 100.0)];
        let (zoom, _) = fit_view_to_rects(&spread, (1000.0, 800.0)).unwrap();
        assert!((zoom - 1000.0 / 1000.0).abs() < 1e-4);
    }

    /// "Fit all" bbox math over several tiles of DIFFERENT sizes, against
    /// the reduced viewport left over with both side panels open — same
    /// viewport shape as [`focus_view_centers_tile_in_the_narrowed_viewport`].
    #[test]
    fn fit_view_frames_multiple_variable_size_tiles_with_both_panels_open() {
        let viewport = (1440.0 - PR_SIDEBAR_W - CHAT_PANEL_W, 800.0);
        // Three tiles, deliberately different widths/heights, scattered.
        let rects = [
            (0.0, 0.0, 260.0, 118.0),
            (400.0, 50.0, 340.0, 220.0),
            (150.0, 300.0, 180.0, 90.0),
        ];
        let (zoom, pan) = fit_view_to_rects(&rects, viewport).unwrap();
        // bbox: x in [0, 740], y in [0, 390].
        let (min_x, min_y, max_x, max_y) = (0.0_f32, 0.0_f32, 740.0_f32, 390.0_f32);
        let content_w = max_x - min_x + FIT_PADDING * 2.0;
        let content_h = max_y - min_y + FIT_PADDING * 2.0;
        let expected_zoom = (viewport.0 / content_w).min(viewport.1 / content_h).clamp(ZOOM_MIN, ZOOM_MAX);
        assert!((zoom - expected_zoom).abs() < 1e-4);
        // The bbox midpoint lands on the (narrowed) viewport's own center.
        let (cx, cy) = ((min_x + max_x) / 2.0, (min_y + max_y) / 2.0);
        assert!((cx * zoom + pan.0 - viewport.0 / 2.0).abs() < 1e-3);
        assert!((cy * zoom + pan.1 - viewport.1 / 2.0).abs() < 1e-3);
    }

    #[test]
    fn no_tiles_visible_only_when_tiles_exist_and_none_are_on_screen() {
        // All off-screen: tiles present, none intersect the viewport.
        assert!(no_tiles_visible(5, false));
        // At least one on-screen: never a rescue case.
        assert!(!no_tiles_visible(5, true));
        // No tiles at all (e.g. filtered to nothing) — the ordinary empty
        // state, not a rescue case, even though "none visible" is trivially
        // true.
        assert!(!no_tiles_visible(0, false));
        assert!(!no_tiles_visible(0, true));
    }

    fn pr_item(number: u64, repo: &str, title: &str, branch: Option<&str>) -> MyPrItem {
        let detail = branch.map(|b| {
            serde_json::json!({
                "number": number, "url": null, "state": "OPEN", "isDraft": false,
                "reviewDecision": null, "reviewers": [], "checks": null, "title": title,
                "branch": b, "mergeable": "mergeable", "hasReviewerRequested": true
            })
        });
        serde_json::from_value(serde_json::json!({
            "number": number, "title": title, "url": null, "repo": repo,
            "isDraft": false, "updatedAt": null, "detail": detail
        }))
        .unwrap()
    }

    #[test]
    fn pr_search_matches_repo_title_number_and_branch() {
        let pr = pr_item(1234, "cofactr/Zeron", "Fix Login Redirect", Some("mikey/login-fix"));
        // Empty query = everything.
        assert!(pr_matches_search("", &pr));
        // Repo, either half, case-insensitive (query arrives lowercased).
        assert!(pr_matches_search("cofactr/zeron", &pr));
        assert!(pr_matches_search("zeron", &pr));
        // Title substring.
        assert!(pr_matches_search("login redirect", &pr));
        // Number with or without '#', and partial.
        assert!(pr_matches_search("1234", &pr));
        assert!(pr_matches_search("#1234", &pr));
        assert!(pr_matches_search("#12", &pr));
        assert!(pr_matches_search("23", &pr));
        // Branch (from the detail sweep).
        assert!(pr_matches_search("mikey/login-fix", &pr));
        // Misses.
        assert!(!pr_matches_search("#99", &pr));
        assert!(!pr_matches_search("other-repo", &pr));
        // No detail yet → no branch field, other fields still match.
        let pending = pr_item(7, "acme/api", "Bump deps", None);
        assert!(pr_matches_search("#7", &pending));
        assert!(!pr_matches_search("mikey/", &pending));
    }

    #[test]
    fn pr_sidebar_flag_restores_and_ignores_unknown_entries() {
        assert!(!pr_sidebar_open_from_flags(&BTreeSet::new()));
        let junk: BTreeSet<String> = ["viewMode:myPrs".to_string(), "somethingElse".to_string()].into();
        assert!(!pr_sidebar_open_from_flags(&junk));
        let on: BTreeSet<String> = [PR_SIDEBAR_FLAG.to_string(), "somethingElse".to_string()].into();
        assert!(pr_sidebar_open_from_flags(&on));
    }

    #[test]
    fn pr_sidebar_flag_round_trips_through_the_string_set_file() {
        let dir = std::env::temp_dir().join(format!("zeron-overview-flags-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Absent file → closed.
        let loaded = overview_positions::load_string_set(&dir, overview_positions::UI_FLAGS_FILE);
        assert!(!pr_sidebar_open_from_flags(&loaded));
        let on: BTreeSet<String> = [PR_SIDEBAR_FLAG.to_string()].into();
        overview_positions::save_string_set(&dir, overview_positions::UI_FLAGS_FILE, &on).unwrap();
        let loaded = overview_positions::load_string_set(&dir, overview_positions::UI_FLAGS_FILE);
        assert!(pr_sidebar_open_from_flags(&loaded));
        // Corrupt file degrades to closed rather than erroring.
        std::fs::write(dir.join(overview_positions::UI_FLAGS_FILE), "\"MyPrs\"").unwrap();
        let loaded = overview_positions::load_string_set(&dir, overview_positions::UI_FLAGS_FILE);
        assert!(!pr_sidebar_open_from_flags(&loaded));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn focus_view_centers_tile_in_the_narrowed_viewport() {
        // A 1440px window minus the PR sidebar (340) and chat panel (540).
        let viewport = (1440.0 - PR_SIDEBAR_W - CHAT_PANEL_W, 800.0);
        let rect = (1000.0, 600.0, 260.0, 160.0);
        // Zoomed out: jumps in to 100% (still fits: 560 >= 260+160).
        let (zoom, pan) = focus_view_on_rect(rect, viewport, 0.4);
        assert_eq!(zoom, 1.0);
        assert!(((1000.0 + 130.0) * zoom + pan.0 - viewport.0 / 2.0).abs() < 1e-3);
        assert!(((600.0 + 80.0) * zoom + pan.1 - viewport.1 / 2.0).abs() < 1e-3);
        // Already zoomed in and it fits: keeps the user's zoom.
        let (zoom, _) = focus_view_on_rect(rect, viewport, 1.3);
        assert!((zoom - 1.3).abs() < 1e-4);
        // A viewport too narrow for the tile at the current zoom: zooms out
        // to fit rather than centering a clipped tile.
        let (zoom, _) = focus_view_on_rect(rect, (300.0, 800.0), 1.5);
        assert!((zoom - 300.0 / 420.0).abs() < 1e-4);
    }

    #[test]
    fn acknowledged_done_resets_when_status_leaves_done() {
        let mut ack: BTreeSet<String> = ["a".to_string()].into();
        assert!(!is_unread_done(&ack, "a", true));
        assert!(is_unread_done(&ack, "b", true));
        assert!(!is_unread_done(&ack, "b", false));
        // Still done: nothing to reconcile.
        assert!(!reconcile_acknowledged(&mut ack, "a", true));
        assert!(ack.contains("a"));
        // Left done: dropped, so the next completion is unread again.
        assert!(reconcile_acknowledged(&mut ack, "a", false));
        assert!(!ack.contains("a"));
        assert!(is_unread_done(&ack, "a", true));
        // Not in the set: no change reported (no redundant save).
        assert!(!reconcile_acknowledged(&mut ack, "a", false));
    }

    #[test]
    fn status_style_maps_zeron_indicators_to_reference_vocabulary() {
        assert_eq!(status_style(ChatIndicator::AwaitingInput, false), ("●", "needs input"));
        assert_eq!(status_style(ChatIndicator::Completed, true), ("✓", "done"));
        assert_eq!(status_style(ChatIndicator::Idle, false), ("–", "idle"));
        assert_eq!(status_style(ChatIndicator::Idle, true), ("○", "not running"));
        // `not_running` only ever refines Idle.
        assert_eq!(status_style(ChatIndicator::Working, true), ("●", "working"));
    }

    #[test]
    fn cwd_helpers_match_reference_rules() {
        assert!(cwd_is_worktree(Some("/r/ws/.worktrees/eng-1")));
        assert!(cwd_is_worktree(Some("/r/ws/.agent-worktrees/x/y")));
        assert!(!cwd_is_worktree(Some("/r/ws")));
        assert!(!cwd_is_worktree(None));
        assert_eq!(shorten_cwd("/Users/m/Projects/zeron"), ".../Projects/zeron");
        assert_eq!(shorten_cwd("/Users/m"), "/Users/m");
    }

    #[test]
    fn group_labels_use_reference_meta_tables() {
        assert_eq!(group_label(GroupDimension::Category, "pr_review"), "PR review");
        assert_eq!(group_label(GroupDimension::Origin, "agent_mode"), "agent-mode.sh");
        assert_eq!(group_label(GroupDimension::Repo, overview_grouping::NONE_KEY), "No repo");
        assert_eq!(group_label(GroupDimension::Ticket, overview_grouping::NONE_KEY), "No ticket / PR");
        assert_eq!(group_label(GroupDimension::Repo, "zeron"), "zeron");
    }

    #[test]
    fn tile_width_matches_reference_tile_size() {
        // `tileSize(pct, 160, 260)`: null → min, 1.0 → max, linear + rounded.
        assert_eq!(tile_width_for_pct(None), 160.0);
        assert_eq!(tile_width_for_pct(Some(0.0)), 160.0);
        assert_eq!(tile_width_for_pct(Some(1.0)), 260.0);
        assert_eq!(tile_width_for_pct(Some(0.5)), 210.0);
        assert_eq!(tile_width_for_pct(Some(0.333)), 193.0); // 193.3 → 193
        assert_eq!(tile_width_for_pct(Some(0.337)), 194.0); // 193.7 → 194
    }

    #[test]
    fn tile_width_clamps_out_of_range_and_non_finite() {
        assert_eq!(tile_width_for_pct(Some(-0.4)), 160.0);
        assert_eq!(tile_width_for_pct(Some(3.0)), 260.0);
        assert_eq!(tile_width_for_pct(Some(f32::NAN)), 160.0);
        assert_eq!(tile_width_for_pct(Some(f32::INFINITY)), 160.0);
    }

    #[test]
    fn tile_height_is_aspect_ratio_over_content_floor() {
        // 0.62 × 260 = 161.2 → 161, above the content floor.
        assert_eq!(tile_size_for_pct(Some(1.0)), (260.0, 161.0));
        // 0.62 × 160 = 99.2 → 99, below it: the floor wins.
        assert_eq!(tile_size_for_pct(None), (160.0, TILE_CONTENT_MIN_H));
        // Height never shrinks as width grows.
        let mut last = 0.0;
        for i in 0..=100 {
            let (_, h) = tile_size_for_pct(Some(i as f32 / 100.0));
            assert!(h >= last, "height shrank at {i}%");
            last = h;
        }
        // `tile_max_size` also accounts for CONTENT height — a badge row +
        // `FLAT_STRIDE_SUBAGENT_TILES` (4) subagent tiles: 140 + 21 + (10 +
        // 4×26 + 3×6) — a deliberate bound, not the (unbounded) worst case;
        // see its own doc comment.
        assert_eq!(tile_max_size(), (260.0, 293.0));
    }

    #[test]
    fn tile_content_height_estimate_counts_every_subagent_tile() {
        // No optional content: exactly the fixed-row floor.
        assert_eq!(tile_content_height_estimate(0, false), TILE_CONTENT_MIN_H);
        assert_eq!(subagent_stack_height(0), 0.0);
        // 1 subagent: the connector's 10px + one 26px tile, no gap.
        assert_eq!(tile_content_height_estimate(1, false), 176.0);
        // 2: two tiles + one 6px gap.
        assert_eq!(tile_content_height_estimate(2, false), 140.0 + 10.0 + 52.0 + 6.0);
        // 36: uncapped — every tile charged, no "+N more" collapse.
        assert_eq!(subagent_stack_height(36), 10.0 + 36.0 * 26.0 + 35.0 * 6.0);
        assert_eq!(tile_content_height_estimate(36, false), 140.0 + 1156.0);
        // Distinct counts give distinct heights (no saturating bucket).
        assert!(tile_content_height_estimate(6, false) > tile_content_height_estimate(5, false));
        // A badge row adds a flat amount on top, independent of subagents.
        assert_eq!(tile_content_height_estimate(0, true), 161.0);
        assert_eq!(tile_content_height_estimate(36, true), 161.0 + 1156.0);
    }

    #[test]
    fn tile_content_height_estimate_is_monotonic_in_both_inputs() {
        // More subagents never shrinks the estimate — strictly grows, in
        // fact — holding badge presence fixed, in either state.
        for has_badge in [false, true] {
            let mut last = tile_content_height_estimate(0, has_badge);
            for count in 1..=40 {
                let h = tile_content_height_estimate(count, has_badge);
                assert!(h > last, "height didn't grow going to {count} subagents (badge={has_badge})");
                last = h;
            }
        }
        // Adding a badge row never shrinks the estimate, holding subagent
        // count fixed.
        for count in [0, 1, 2, 5, 36] {
            assert!(
                tile_content_height_estimate(count, true) >= tile_content_height_estimate(count, false),
                "badge row shrank the estimate at {count} subagents"
            );
        }
    }

    #[test]
    fn context_label_and_fill_match_reference_face() {
        assert_eq!(context_pct_label(None), "— context");
        assert_eq!(context_pct_label(Some(0.0)), "0% context");
        assert_eq!(context_pct_label(Some(0.426)), "43% context");
        assert_eq!(context_pct_label(Some(1.0)), "100% context");
        assert_eq!(context_fill_fraction(None), 0.0);
        assert_eq!(context_fill_fraction(Some(0.426)), 0.43);
        assert_eq!(context_fill_fraction(Some(2.0)), 1.0);
    }

    #[test]
    fn context_refetch_only_counts_as_changed_when_the_percent_moves() {
        // First fetch: null is no change, a value is.
        assert!(!context_pct_changed(None, None));
        assert!(context_pct_changed(None, Some(0.3)));
        // Same value, or sub-percent drift: no-op (no relayout).
        assert!(!context_pct_changed(Some(Some(0.3)), Some(0.3)));
        assert!(!context_pct_changed(Some(Some(0.301)), Some(0.304)));
        // Crossing a whole percent, or going to/from null: changed.
        assert!(context_pct_changed(Some(Some(0.30)), Some(0.31)));
        assert!(context_pct_changed(Some(Some(0.30)), None));
        assert!(!context_pct_changed(Some(None), None));
    }

    #[test]
    fn flat_grid_stride_fits_the_largest_tile() {
        let (sw, sh) = flat_grid_stride();
        let (mw, mh) = tile_max_size();
        assert_eq!((sw, sh), (mw + TILE_GAP, mh + TILE_GAP));
    }

    fn row(id: &str, cwd: &str, pct: Option<f32>) -> OverviewRow {
        row_ex(id, cwd, pct, 0, false)
    }

    /// Like [`row`], but also setting the two content fields that now feed
    /// `tile_size` — for tests that need tiles whose height varies
    /// independent of their context-%-driven width.
    fn row_ex(id: &str, cwd: &str, pct: Option<f32>, subagent_count: usize, has_badge_row: bool) -> OverviewRow {
        use chrono::TimeZone;
        let chat = Chat {
            id: id.into(),
            device_id: "d".into(),
            title: None,
            archived: false,
            cwd: Some(cwd.into()),
            branch: None,
            checkout_id: None,
            source_context: None,
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: Utc.timestamp_opt(0, 0).unwrap(),
            harness_session_id: None,
            harness_session_cwd: None,
            parent_chat_id: None,
            space_id: None,
            last_seen_at: None,
            room_gen: None,
            linked_pr_url: None,
            linked_pr_source: None,
            linked_ticket_id: None,
            linked_ticket_source: None,
        };
        OverviewRow {
            status: ChatIndicator::Idle,
            repo: repo_key(chat.cwd.as_deref()),
            chat,
            stale: false,
            ticket: None,
            category: None,
            origin: None,
            pr_number: None,
            pr_title: None,
            context_pct: pct,
            subagent_count,
            has_badge_row,
        }
    }

    #[test]
    fn row_tile_size_uses_the_content_height_estimate_when_it_exceeds_aspect() {
        // 0% context: aspect height is the floor (140), well under what 2
        // subagents + a badge row need (195) — `tile_size` must pick the
        // taller of the two, not the context-driven one alone.
        let tall = row_ex("c", "/r/zeron", None, 2, true);
        assert_eq!(tall.tile_size(), (160.0, 229.0));
        // 100% context, no optional content: aspect height (161) exceeds
        // the bare content floor (140) — aspect wins here instead.
        let wide = row_ex("c", "/r/zeron", Some(1.0), 0, false);
        assert_eq!(wide.tile_size(), (260.0, 161.0));
        // ...but even one subagent tile (176) outgrows it.
        let wide_sub = row_ex("c", "/r/zeron", Some(1.0), 1, false);
        assert_eq!(wide_sub.tile_size(), (260.0, 176.0));
        // A 36-subagent parent grows to fit all of them.
        let huge = row_ex("c", "/r/zeron", Some(1.0), 36, true);
        assert_eq!(huge.tile_size(), (260.0, 161.0 + 1156.0));
    }

    #[test]
    fn grouped_layout_packs_variable_tile_sizes_without_overlap() {
        let pcts = [None, Some(1.0), Some(0.2), Some(0.75), Some(0.5), None, Some(0.9)];
        let rows: Vec<OverviewRow> = pcts
            .iter()
            .enumerate()
            .map(|(i, &p)| {
                let repo = if i % 2 == 0 { "/r/zeron" } else { "/r/ui" };
                row(&format!("c{i}"), repo, p)
            })
            .collect();
        let refs: Vec<&OverviewRow> = rows.iter().collect();
        let tree = overview_grouping::partition(refs, &[GroupDimension::Repo]);
        let laid_out = layout_partition(&tree, 0);
        assert_eq!(laid_out.positions.len(), rows.len());

        let rects: Vec<(f32, f32, f32, f32)> = rows
            .iter()
            .map(|r| {
                let (x, y) = laid_out.positions[&r.chat.id];
                let (w, h) = r.tile_size();
                (x, y, w, h)
            })
            .collect();
        for i in 0..rects.len() {
            for j in (i + 1)..rects.len() {
                let (a, b) = (rects[i], rects[j]);
                let overlap = a.0 < b.0 + b.2 && a.0 + a.2 > b.0 && a.1 < b.1 + b.3 && a.1 + a.3 > b.1;
                assert!(!overlap, "tiles {i} and {j} overlap: {a:?} vs {b:?}");
            }
        }
        // Every tile sits inside its group's box: box widths account for the
        // real (variable) widths, not a uniform constant.
        for (r, &(x, y, w, h)) in rows.iter().zip(&rects) {
            let inside = laid_out.group_boxes.iter().any(|&(bx, by, bw, bh, _)| {
                x >= bx && y >= by && x + w <= bx + bw + 0.01 && y + h <= by + bh + 0.01
            });
            assert!(inside, "{} escapes every group box", r.chat.id);
        }
    }

    /// Extends the mixed-size case above with rows that share the exact
    /// same context % (so identical width) but differ in subagent
    /// count/badge presence — the case a width-only size wouldn't catch at
    /// all, and the exact scenario `tile_content_height_estimate` exists
    /// for: two same-width tiles with very different real heights must
    /// still pack, group-box, and never overlap correctly.
    #[test]
    fn grouped_layout_packs_rows_with_mixed_content_heights_without_overlap() {
        let specs: [(Option<f32>, usize, bool); 8] = [
            (Some(0.5), 0, false),
            (Some(0.5), 0, true),
            (Some(0.5), 1, false),
            (Some(0.5), 2, false),
            (Some(0.5), 2, true),
            (Some(0.5), 5, false),
            (Some(0.5), 5, true),
            (Some(0.5), 36, false),
        ];
        let rows: Vec<OverviewRow> = specs
            .iter()
            .enumerate()
            .map(|(i, &(pct, subagents, badge))| {
                let repo = if i % 2 == 0 { "/r/zeron" } else { "/r/ui" };
                row_ex(&format!("m{i}"), repo, pct, subagents, badge)
            })
            .collect();
        // Every row has the SAME width (same pct) but the sizes genuinely
        // differ in height only — the precondition this test exists to
        // exercise.
        let sizes: Vec<(f32, f32)> = rows.iter().map(|r| r.tile_size()).collect();
        assert!(sizes.iter().all(|s| s.0 == sizes[0].0), "widths should all match: {sizes:?}");
        assert!(
            sizes.iter().map(|s| s.1 as i64).collect::<BTreeSet<_>>().len() > 1,
            "heights should vary: {sizes:?}"
        );

        let refs: Vec<&OverviewRow> = rows.iter().collect();
        let tree = overview_grouping::partition(refs, &[GroupDimension::Repo]);
        let laid_out = layout_partition(&tree, 0);
        assert_eq!(laid_out.positions.len(), rows.len());

        let rects: Vec<(f32, f32, f32, f32)> = rows
            .iter()
            .map(|r| {
                let (x, y) = laid_out.positions[&r.chat.id];
                let (w, h) = r.tile_size();
                (x, y, w, h)
            })
            .collect();
        for i in 0..rects.len() {
            for j in (i + 1)..rects.len() {
                let (a, b) = (rects[i], rects[j]);
                let overlap = a.0 < b.0 + b.2 && a.0 + a.2 > b.0 && a.1 < b.1 + b.3 && a.1 + a.3 > b.1;
                assert!(!overlap, "tiles {i} and {j} overlap: {a:?} vs {b:?}");
            }
        }
        for (r, &(x, y, w, h)) in rows.iter().zip(&rects) {
            let inside = laid_out.group_boxes.iter().any(|&(bx, by, bw, bh, _)| {
                x >= bx && y >= by && x + w <= bx + bw + 0.01 && y + h <= by + bh + 0.01
            });
            assert!(inside, "{} escapes every group box", r.chat.id);
        }
    }

    #[test]
    fn unread_done_overrides_stale_dimming_but_not_archived() {
        // Fresh unread done, stale unread done: full opacity, ribbon shown.
        assert_eq!(card_dimming(false, false, true), (None, true));
        assert_eq!(card_dimming(false, true, true), (None, true));
        // Acknowledged: normal dimming returns.
        assert_eq!(card_dimming(false, true, false), (Some(0.55), false));
        assert_eq!(card_dimming(false, false, false), (None, false));
        // Archived always dims and never shows the ribbon.
        assert_eq!(card_dimming(true, true, true), (Some(0.4), false));
        assert_eq!(card_dimming(true, false, false), (Some(0.4), false));
    }

    #[test]
    fn tile_move_ease_matches_web_curve_endpoints_and_overshoots() {
        assert_eq!(tile_move_ease(0.0), 0.0);
        assert_eq!(tile_move_ease(1.0), 1.0);
        assert_eq!(tile_move_ease(-1.0), 0.0);
        assert_eq!(tile_move_ease(2.0), 1.0);
        let samples: Vec<(f32, f32)> = (1..100).map(|i| i as f32 / 100.0).map(|x| (x, tile_move_ease(x))).collect();
        let (peak_x, peak) = samples.iter().copied().fold((0.0, f32::MIN), |a, b| if b.1 > a.1 { b } else { a });
        assert!(peak > 1.05, "overshoot peak {peak} should exceed 1 (web curve peaks ~1.1)");
        assert!(peak_x > 0.5 && peak_x < 1.0, "overshoot peak at x={peak_x}");
        // Ease-out: well past halfway by the midpoint, monotonic rise before the peak.
        assert!(tile_move_ease(0.5) > 0.9);
        let rising: Vec<f32> = samples.iter().filter(|(x, _)| *x <= peak_x).map(|s| s.1).collect();
        assert!(rising.windows(2).all(|w| w[1] >= w[0]));
        // Settles back toward 1 at the end.
        assert!((tile_move_ease(0.99) - 1.0).abs() < 0.01);
    }

    #[test]
    fn lerp_pos_interpolates_and_extrapolates() {
        let a = TilePos { x: 0.0, y: 100.0 };
        let b = TilePos { x: 200.0, y: 0.0 };
        assert!(lerp_pos(a, b, 0.0).approx_eq(a));
        assert!(lerp_pos(a, b, 1.0).approx_eq(b));
        assert!(lerp_pos(a, b, 0.5).approx_eq(TilePos { x: 100.0, y: 50.0 }));
        // Overshoot carries past the target along the same line.
        assert!(lerp_pos(a, b, 1.1).approx_eq(TilePos { x: 220.0, y: -10.0 }));
    }

    #[test]
    fn tile_move_samples_then_finishes() {
        let t0 = Instant::now();
        let dur = Duration::from_millis(550);
        let m = TileMove {
            from: TilePos { x: 0.0, y: 0.0 },
            to: TilePos { x: 100.0, y: 0.0 },
            started: t0,
        };
        assert!(m.sample(t0, dur).unwrap().approx_eq(m.from));
        let mid = m.sample(t0 + Duration::from_millis(275), dur).unwrap();
        assert!(mid.x > 50.0 && mid.x < 115.0, "mid {mid:?}");
        assert!(m.sample(t0 + dur, dur).is_none());
        assert!(m.sample(t0 + Duration::from_secs(5), dur).is_none());
    }

    #[test]
    fn tile_motion_animates_only_organize_moves() {
        let t0 = Instant::now();
        let dur = Duration::from_millis(550);
        let old = TilePos { x: 0.0, y: 0.0 };
        let new = TilePos { x: 300.0, y: 120.0 };

        // First appearance: no fly-in.
        let (m, pos) = plan_tile_motion(None, None, new, false, false, t0, dur);
        assert!(m.is_none() && pos.approx_eq(new));

        // Unchanged target at rest: nothing to do.
        let (m, pos) = plan_tile_motion(Some(new), None, new, false, false, t0, dur);
        assert!(m.is_none() && pos.approx_eq(new));

        // Live drag of this tile: snap, even though the target moved.
        let (m, pos) = plan_tile_motion(Some(old), None, new, true, false, t0, dur);
        assert!(m.is_none() && pos.approx_eq(new));

        // Reduced motion: snap.
        let (m, pos) = plan_tile_motion(Some(old), None, new, false, true, t0, dur);
        assert!(m.is_none() && pos.approx_eq(new));

        // Organize move: starts at the old position, heading to the new one.
        let (m, pos) = plan_tile_motion(Some(old), None, new, false, false, t0, dur);
        let m = m.expect("organize move animates");
        assert!(pos.approx_eq(old) && m.from.approx_eq(old) && m.to.approx_eq(new));

        // Mid-flight, same target: keeps playing from the eased position.
        let t1 = t0 + Duration::from_millis(200);
        let (m2, pos) = plan_tile_motion(Some(new), Some(m), new, false, false, t1, dur);
        let m2 = m2.expect("still in flight");
        assert_eq!(m2.started, t0);
        assert!(!pos.approx_eq(old) && !pos.approx_eq(new));

        // Mid-flight retarget: continues from the displayed spot, not a jump.
        let other = TilePos { x: -50.0, y: 400.0 };
        let (m3, pos3) = plan_tile_motion(Some(new), Some(m), other, false, false, t1, dur);
        let m3 = m3.expect("retarget animates");
        assert!(pos3.approx_eq(pos) && m3.from.approx_eq(pos) && m3.to.approx_eq(other));

        // Finished: the move is dropped and the target renders exactly, so
        // nothing keeps requesting frames.
        let (m4, pos) = plan_tile_motion(Some(new), Some(m), new, false, false, t0 + dur, dur);
        assert!(m4.is_none() && pos.approx_eq(new));

        // Sub-pixel repack jitter is not a move.
        let jitter = TilePos { x: new.x + 0.2, y: new.y - 0.2 };
        let (m5, _) = plan_tile_motion(Some(new), None, jitter, false, false, t0, dur);
        assert!(m5.is_none());
    }

    /// End to end over the real UI path the canvas uses: a chat with a
    /// harness session gets a `SCAN_CHAT_SUBAGENTS` request from `rows()`,
    /// the reply lands in `subagents`, the row's `subagent_count` (tile
    /// height) follows, and Canvas mode actually lays the subagent block out
    /// INSIDE the tile's bounds (not clipped past its bottom edge by the
    /// fixed-height, `overflow_hidden` tile).
    #[gpui::test]
    fn canvas_tile_renders_scanned_subagent_rows_inside_its_bounds(cx: &mut gpui::TestAppContext) {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let _guard = runtime.enter();
        let (out, mut requests) = tokio::sync::mpsc::channel::<String>(64);
        let (replies, inbound) = tokio::sync::mpsc::channel::<String>(64);
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
        });
        let state = cx.new(|_| AppState::new());
        state.update(cx, |state, _| {
            state.set_test_engine(crate::state::EngineHandle::from_test_client(
                zeron_rpc::RpcClient::new(out, inbound),
            ));
            let mut chat = row_ex("c1", "/r", None, 0, false).chat;
            chat.harness_session_id = Some("s1".into());
            chat.last_message_at = Some(Utc::now());
            state.chats = vec![chat];
        });
        let transcript = cx.new(|cx| Transcript::new(state.clone(), cx));
        let composer = cx.new(|cx| crate::composer::Composer::new(state.clone(), cx));
        let (overview, vcx) = cx.add_window_view(|_, cx| {
            let mut overview =
                Overview::new(state.clone(), gpui::WeakEntity::new_invalid(), transcript, composer, cx);
            overview.view_mode = ViewMode::Canvas;
            overview
        });
        vcx.run_until_parked();

        // Answer the scan (the other per-row lookups just fail — irrelevant here).
        let mut scanned = false;
        while let Ok(frame) = requests.try_recv() {
            let request: serde_json::Value = serde_json::from_str(&frame).unwrap();
            let reply = if request["method"] == methods::SCAN_CHAT_SUBAGENTS {
                assert_eq!(request["params"]["chatId"], "c1");
                scanned = true;
                serde_json::json!({"id": request["id"], "ok": [
                    {"agentId": "a", "description": "Research", "agentType": "fork", "transcriptPath": "/a"},
                    {"agentId": "b", "transcriptPath": "/b"},
                    {"agentId": "c", "transcriptPath": "/c"}
                ]})
            } else {
                serde_json::json!({"id": request["id"], "err": "unavailable"})
            };
            runtime.block_on(async {
                replies.send(reply.to_string()).await.unwrap();
                while replies.capacity() < replies.max_capacity() {
                    tokio::task::yield_now().await;
                }
            });
        }
        assert!(scanned, "rows() must request SCAN_CHAT_SUBAGENTS for a chat with a harness session");
        vcx.run_until_parked();

        overview.update(vcx, |overview, cx| {
            assert_eq!(overview.subagents.get("c1").map(Vec::len), Some(3));
            assert_eq!(overview.rows(cx)[0].subagent_count, 3);
        });
        let tile = vcx.debug_bounds("overview-tile-body").expect("tile rendered");
        let block = vcx.debug_bounds("overview-tile-subagents").expect("subagent rows rendered in the tile");
        assert!(
            block.top() >= tile.top() && block.bottom() <= tile.bottom(),
            "subagent block {block:?} must sit inside tile {tile:?}"
        );
        // All three compact tiles, uncapped.
        assert!(f32::from(block.size.height) >= 3.0 * SUBAGENT_TILE_H + 2.0 * SUBAGENT_TILE_GAP - 0.5);
    }

    /// Uncapped subagent stack + the peek, end to end on the canvas: a
    /// 36-subagent parent lays out every compact tile inside its (grown)
    /// bounds; clicking one opens the read-only transcript panel WITHOUT
    /// toggling the parent's expand, issues `READ_SUBAGENT_TRANSCRIPT` for
    /// that `{chatId, agentId}`, shows a loading state until the reply
    /// lands, then renders the turns; Escape closes it back to the canvas.
    #[gpui::test]
    fn canvas_subagent_tile_click_opens_readonly_peek(cx: &mut gpui::TestAppContext) {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let _guard = runtime.enter();
        let (out, mut requests) = tokio::sync::mpsc::channel::<String>(256);
        let (replies, inbound) = tokio::sync::mpsc::channel::<String>(256);
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
        });
        let state = cx.new(|_| AppState::new());
        state.update(cx, |state, _| {
            state.set_test_engine(crate::state::EngineHandle::from_test_client(
                zeron_rpc::RpcClient::new(out, inbound),
            ));
            let mut chat = row_ex("c1", "/r", None, 0, false).chat;
            chat.harness_session_id = Some("s1".into());
            chat.last_message_at = Some(Utc::now());
            state.chats = vec![chat];
        });
        let transcript = cx.new(|cx| Transcript::new(state.clone(), cx));
        let composer = cx.new(|cx| crate::composer::Composer::new(state.clone(), cx));
        let (overview, vcx) = cx.add_window_view(|_, cx| {
            let mut overview =
                Overview::new(state.clone(), gpui::WeakEntity::new_invalid(), transcript, composer, cx);
            overview.view_mode = ViewMode::Canvas;
            overview
        });
        vcx.run_until_parked();

        let subagents: Vec<serde_json::Value> = (0..36)
            .map(|i| {
                serde_json::json!({
                    "agentId": format!("a{i}"),
                    "description": format!("Subagent task {i}"),
                    "agentType": "Explore",
                    "transcriptPath": format!("/a{i}"),
                    "status": if i == 0 { "running" } else { "done" }
                })
            })
            .collect();
        // Answers every pending request; returns the transcript requests seen.
        let answer = |requests: &mut tokio::sync::mpsc::Receiver<String>| {
            let mut transcript_requests = Vec::new();
            while let Ok(frame) = requests.try_recv() {
                let request: serde_json::Value = serde_json::from_str(&frame).unwrap();
                let reply = if request["method"] == methods::SCAN_CHAT_SUBAGENTS {
                    serde_json::json!({"id": request["id"], "ok": subagents})
                } else if request["method"] == methods::READ_SUBAGENT_TRANSCRIPT {
                    transcript_requests.push(request["params"].clone());
                    serde_json::json!({"id": request["id"], "ok": {
                        "turns": [
                            {"role": "user", "text": "Go look", "timestamp": null, "tools": []},
                            {"role": "assistant", "text": "Found it", "timestamp": null, "tools": [
                                {"name": "Grep", "inputPreview": "foo", "resultPreview": "bar.rs:1"}
                            ]}
                        ],
                        "model": "m"
                    }})
                } else {
                    serde_json::json!({"id": request["id"], "err": "unavailable"})
                };
                runtime.block_on(async {
                    replies.send(reply.to_string()).await.unwrap();
                    while replies.capacity() < replies.max_capacity() {
                        tokio::task::yield_now().await;
                    }
                });
            }
            transcript_requests
        };
        answer(&mut requests);
        vcx.run_until_parked();

        overview.update(vcx, |overview, cx| {
            assert_eq!(overview.subagents.get("c1").map(Vec::len), Some(36));
            let row = &overview.rows(cx)[0];
            assert_eq!(row.subagent_count, 36);
            assert_eq!(row.tile_size().1, tile_content_height_estimate(36, false));
        });
        vcx.run_until_parked();
        let tile = vcx.debug_bounds("overview-tile-body").expect("tile rendered");
        let block = vcx.debug_bounds("overview-tile-subagents").expect("subagent stack rendered");
        assert!(
            block.top() >= tile.top() && block.bottom() <= tile.bottom() + px(0.5),
            "36-tile stack {block:?} must fit inside the grown tile {tile:?}"
        );
        assert!(
            (f32::from(block.size.height) - (subagent_stack_height(36) - SUBAGENT_STACK_TOP)).abs() < 1.0,
            "every tile laid out at its fixed height: {block:?}"
        );

        // Click the first compact tile.
        let first = gpui::point(block.left() + px(30.0), block.top() + px(SUBAGENT_TILE_H / 2.0));
        vcx.simulate_click(first, gpui::Modifiers::none());
        vcx.run_until_parked();
        overview.update(vcx, |overview, _| {
            let peek = overview.subagent_peek.as_ref().expect("peek opened");
            assert_eq!((peek.chat_id.as_str(), peek.agent_id.as_str()), ("c1", "a0"));
            assert_eq!(peek.load, PeekLoad::Loading);
            assert!(!overview.chat_panel_open);
            assert!(!overview.expanded.contains("c1"), "subagent click must not toggle the parent");
        });
        assert!(vcx.debug_bounds("overview-subagent-peek").is_some(), "peek panel rendered");

        let transcript_requests = answer(&mut requests);
        assert_eq!(transcript_requests, vec![serde_json::json!({"chatId": "c1", "agentId": "a0"})]);
        vcx.run_until_parked();
        overview.update(vcx, |overview, _| {
            match &overview.subagent_peek.as_ref().expect("still open").load {
                PeekLoad::Loaded(t) => assert_eq!(t.turns.len(), 2),
                other => panic!("expected loaded, got {other:?}"),
            }
        });

        vcx.simulate_keystrokes("escape");
        vcx.run_until_parked();
        overview.update(vcx, |overview, _| {
            assert!(overview.subagent_peek.is_none(), "Escape closes the peek");
        });
    }

    /// List mode with more rows than fit on screen: each row keeps its full
    /// natural height (subagent rows included) and the list scrolls. The
    /// row root is `overflow_hidden` for the unread ribbon, which zeroes its
    /// flex auto-min-height, so without `flex_none` the scrolling column
    /// squeezed every row (live repro: a 64px row down to 11.5px) and clipped
    /// the subagent block away.
    #[gpui::test]
    fn list_rows_keep_subagent_rows_when_the_list_overflows(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
        });
        let state = cx.new(|_| AppState::new());
        let ids: Vec<String> = (0..60).map(|i| format!("c{i:02}")).collect();
        state.update(cx, |state, _| {
            state.chats = ids
                .iter()
                .map(|id| {
                    let mut chat = row_ex(id, "/r", None, 0, false).chat;
                    chat.last_message_at = Some(Utc::now());
                    chat
                })
                .collect();
        });
        let transcript = cx.new(|cx| Transcript::new(state.clone(), cx));
        let composer = cx.new(|cx| crate::composer::Composer::new(state.clone(), cx));
        let (_overview, vcx) = cx.add_window_view(|_, cx| {
            let mut overview =
                Overview::new(state.clone(), gpui::WeakEntity::new_invalid(), transcript, composer, cx);
            overview.view_mode = ViewMode::List;
            for id in &ids {
                overview.subagents.insert(
                    id.clone(),
                    serde_json::from_value(serde_json::json!([
                        {"agentId": "a", "description": "Research", "transcriptPath": "/a"},
                        {"agentId": "b", "transcriptPath": "/b"},
                        {"agentId": "c", "transcriptPath": "/c"}
                    ]))
                    .unwrap(),
                );
                overview.subagents_fetched_at.insert(id.clone(), Instant::now());
            }
            overview
        });
        vcx.run_until_parked();
        let row = vcx.debug_bounds("overview-list-row").expect("list row rendered");
        let block = vcx.debug_bounds("overview-list-subagents").expect("subagent rows rendered");
        assert!(
            f32::from(block.size.height) >= 3.0 * SUBAGENT_TILE_H,
            "all three subagent rows laid out: {block:?}"
        );
        assert!(
            f32::from(row.size.height) > f32::from(block.size.height),
            "row {row:?} must not be squeezed below its subagent block {block:?}"
        );
    }
}
