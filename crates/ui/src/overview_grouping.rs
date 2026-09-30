//! Composable multi-dimension grouping for the multichat overview canvas —
//! a port of `session_canvas.html`'s `GROUP_DIMENSIONS`/`activeGroups`/
//! `partitionAndPlace` (verified by reading that file directly with
//! `grep -a`/a non-grep tool; plain `grep` silently treats the whole file as
//! binary and skips real matches because of a stray null byte in it — a
//! previous pass got a false "this function doesn't exist" negative exactly
//! this way).
//!
//! The reference's four dimensions toggle independently and *compose*: the
//! order you activate them in is the nesting order (a `Set`'s insertion
//! order in JS — an ordered `Vec` here is the direct equivalent and doesn't
//! need any of `Set`'s other semantics). `partitionAndPlace`
//! (session_canvas.html:1499-1541) recurses over the active dimensions:
//! depth 0 lays its groups out as columns, every deeper dimension nests as
//! rows inside its parent group, and once dimensions run out the remaining
//! rows are leaves to be pixel-packed (`packRects` — a sibling module's job,
//! not this one's; see [`Partition::Leaves`]).
//!
//! This module is the pure partitioning step only: given rows and an
//! ordered list of active dimensions, produce the nested group tree. It
//! knows nothing about pixel layout, colors, or display labels (the
//! reference's `getMeta`) — those are UI-layer concerns a caller adds on
//! top of the raw group keys this module returns, via [`GroupKeyed`].

use std::collections::HashMap;

/// One of the four composable grouping dimensions
/// (`session_canvas.html:1411-1420`'s `GROUP_DIMENSIONS`). Nesting order is
/// caller-determined via the order dimensions appear in the `dims` slice
/// passed to [`partition`] — there is no fixed priority between dimensions,
/// unlike the fixed *value* order within `Category`/`Origin` themselves
/// (see [`CATEGORY_ORDER`]/[`ORIGIN_ORDER`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GroupDimension {
    /// Task/work classification: implementing, pr_review, debug, research,
    /// planning, quick_question, other.
    ///
    /// Wired: computed at external-import time by `external_import.rs`'s
    /// `classify_heuristic` (a port of `session_canvas_server.py`'s pure
    /// tool-count/skills/turn-count heuristic — no LLM pass), persisted per
    /// chat, and fetched lazily by `overview.rs`'s `ensure_classification`
    /// via `CHAT_CLASSIFICATION`. A chat that never came through import (or
    /// whose fetch hasn't landed yet) has no category and keys as
    /// [`NONE_KEY`], which the fixed-order bucketing absorbs into `other`.
    Category,
    /// Launch origin: agent_mode, cursor, sdk_driven, claude_desktop,
    /// bare_cli, unknown.
    ///
    /// Wired the same way as [`GroupDimension::Category`]: computed at
    /// import time by `external_import.rs`'s `classify_entrypoint` (the
    /// transcript's `entrypoint` field → `sdk_driven`/`claude_desktop`/
    /// `bare_cli`, overridden to `agent_mode` when the transcript carries a
    /// `peon_hook.py` `stop_hook_summary` line) and fetched via `CHAT_CLASSIFICATION` in
    /// `ensure_classification`. Missing values key as [`NONE_KEY`] and land
    /// in the `unknown` bucket.
    Origin,
    /// The transcript's cwd, last path segment.
    ///
    /// Real and already in use: `session_canvas_server.py`'s own
    /// `repoGroupKey` (`session_canvas.html:909-913`) uses exactly this
    /// proxy rather than a resolved repo name, specifically to avoid a
    /// git/gh round trip per session just for a label. Already the basis of
    /// `overview.rs`'s existing single-dimension "By repo" grouping.
    Repo,
    /// A linked ticket identifier, or `"PR #<number>"` when there's a
    /// linked PR but no ticket (`ticketGroupKey`,
    /// `session_canvas.html:896-898`).
    ///
    /// Already the basis of `overview.rs`'s existing "By ticket" grouping,
    /// via `TicketStatus.identifier`.
    Ticket,
}

impl GroupDimension {
    /// Stable persistence key (`overview-ui-flags.json`'s `groupBy:` entry).
    /// Never renamed: a persisted value must keep restoring.
    pub fn persist_key(self) -> &'static str {
        match self {
            GroupDimension::Category => "category",
            GroupDimension::Origin => "origin",
            GroupDimension::Repo => "repo",
            GroupDimension::Ticket => "ticket",
        }
    }

    /// Inverse of [`GroupDimension::persist_key`]; `None` for an unknown key
    /// (a dimension from a newer/older build, or junk).
    pub fn from_persist_key(key: &str) -> Option<Self> {
        match key {
            "category" => Some(GroupDimension::Category),
            "origin" => Some(GroupDimension::Origin),
            "repo" => Some(GroupDimension::Repo),
            "ticket" => Some(GroupDimension::Ticket),
            _ => None,
        }
    }
}

/// Sentinel key for "this row has no value on this dimension" — matches the
/// reference's own literal `"__none__"` string (`repoGroupKey`/
/// `ticketGroupKey`, `session_canvas.html:898,912`) rather than a Rust
/// `Option`, so a dynamic-order dimension's "no value" bucket sorts and
/// buckets exactly like every other key with no special-casing needed here.
pub const NONE_KEY: &str = "__none__";

/// Fixed display/sort order for `Category`'s values — mirrors
/// `session_canvas.html:861`'s `CATEGORY_ORDER`, which the reference notes
/// matches `session_canvas_server.py`'s `TASK_CATEGORIES` list, plus Zeron's
/// own `debug` (bugs / firefights / production errors). `other` MUST stay
/// last: it is the catch-all bucket for unrecognized keys (see `group_rows`).
pub const CATEGORY_ORDER: &[&str] = &[
    "implementing",
    "pr_review",
    "debug",
    "research",
    "planning",
    "quick_question",
    "other",
];

/// Fixed display/sort order for `Origin`'s values — mirrors
/// `session_canvas.html:873`'s `ORIGIN_ORDER`.
pub const ORIGIN_ORDER: &[&str] = &[
    "agent_mode",
    "cursor",
    "sdk_driven",
    "claude_desktop",
    "bare_cli",
    "unknown",
];

/// A row's raw group key per dimension — the only thing this module needs
/// from a caller's row type, so it stays generic over whatever `overview.rs`
/// (or any future caller) actually uses without depending on that crate's
/// types directly. Mirrors `GROUP_DIMENSIONS[dim].getKey`
/// (`session_canvas.html:1412-1419`).
pub trait GroupKeyed {
    /// The raw key for `dim`. Use [`NONE_KEY`] for "no value" (matching the
    /// reference's own convention), not an empty string or a sentinel of
    /// your own — [`compute_order`] and the fixed-order fallback both key
    /// off this exact constant.
    fn group_key(&self, dim: GroupDimension) -> String;
}

/// The fixed value-order for a dimension, if it has one — `Category` and
/// `Origin` do (a small, known enum of values); `Repo`/`Ticket` don't (an
/// unbounded, data-dependent set — see [`compute_order`]'s dynamic branch).
/// Mirrors the reference's `order: CATEGORY_ORDER`/`order: null` fields on
/// `GROUP_DIMENSIONS` (`session_canvas.html:1412-1419`).
pub fn fixed_order(dim: GroupDimension) -> Option<&'static [&'static str]> {
    match dim {
        GroupDimension::Category => Some(CATEGORY_ORDER),
        GroupDimension::Origin => Some(ORIGIN_ORDER),
        GroupDimension::Repo | GroupDimension::Ticket => None,
    }
}

/// The ordered set of group keys `rows` should be bucketed into for `dim` —
/// mirrors `computeGroupOrder` (`session_canvas.html:1429-1437`). A fixed
/// dimension returns its fixed order outright, regardless of what's
/// actually present in `rows` (so an empty or sparse bucket still shows up
/// in the *right place*, just filtered out downstream if empty — see
/// [`group_rows`]). A dynamic dimension computes its order fresh from
/// whatever keys `rows` actually has, alphabetically, with [`NONE_KEY`]
/// always sorted last (matching the reference's explicit
/// `if (a === "__none__") return 1;` / `if (b === "__none__") return -1;`).
pub fn compute_order<T: GroupKeyed>(rows: &[T], dim: GroupDimension) -> Vec<String> {
    if let Some(fixed) = fixed_order(dim) {
        return fixed.iter().map(|s| s.to_string()).collect();
    }
    let mut seen: HashMap<String, ()> = HashMap::new();
    let mut keys = Vec::new();
    for row in rows {
        let key = row.group_key(dim);
        if seen.insert(key.clone(), ()).is_none() {
            keys.push(key);
        }
    }
    keys.sort_by(|a, b| match (a.as_str() == NONE_KEY, b.as_str() == NONE_KEY) {
        (true, true) => std::cmp::Ordering::Equal,
        (true, false) => std::cmp::Ordering::Greater,
        (false, true) => std::cmp::Ordering::Less,
        (false, false) => a.cmp(b),
    });
    keys
}

/// Buckets `rows` by `dim`, dropping empty buckets, preserving `dim`'s
/// order for the ones that survive — mirrors `groupSessions`
/// (`session_canvas.html:1439-1448`). A row whose raw key isn't present in
/// a *fixed*-order dimension's value list (an unrecognized category, say)
/// falls into that dimension's LAST bucket as a catch-all — matching the
/// reference's `order[order.length - 1]` fallback precisely, which is
/// deliberately different from falling into a `NONE_KEY` bucket. A dynamic
/// dimension's order is derived from the rows themselves, so this fallback
/// path is unreachable for `Repo`/`Ticket` (every raw key is already in the
/// order by construction).
fn group_rows<T: GroupKeyed>(rows: Vec<T>, dim: GroupDimension) -> Vec<(String, Vec<T>)> {
    let order = compute_order(&rows, dim);
    if order.is_empty() {
        return Vec::new();
    }
    let index_of: HashMap<&str, usize> =
        order.iter().enumerate().map(|(i, k)| (k.as_str(), i)).collect();
    let fallback_index = order.len() - 1;
    let mut buckets: Vec<Vec<T>> = (0..order.len()).map(|_| Vec::new()).collect();
    for row in rows {
        let raw = row.group_key(dim);
        let idx = *index_of.get(raw.as_str()).unwrap_or(&fallback_index);
        buckets[idx].push(row);
    }
    order
        .into_iter()
        .zip(buckets)
        .filter(|(_, items)| !items.is_empty())
        .collect()
}

/// One group at some depth in a [`Partition`] tree — a raw key (map it to a
/// display label/color yourself, e.g. via [`fixed_order`]'s values for
/// `Category`/`Origin`, or a hash-based color for `Repo`/`Ticket` matching
/// the reference's `hashColor`, `session_canvas.html:884-889`, if you want
/// exact visual parity there too) plus its recursively-partitioned contents.
#[derive(Debug, Clone, PartialEq)]
pub struct PartitionGroup<T> {
    pub key: String,
    pub items: Partition<T>,
}

/// The result of recursively partitioning rows by an ordered list of
/// dimensions. Mirrors `partitionAndPlace`'s recursion structure
/// (`session_canvas.html:1499-1541`) minus the pixel-packing (a sibling
/// module's job — see the doc comment on [`Leaves`](Partition::Leaves))
/// and minus label/color metadata (a UI-layer concern — see
/// [`PartitionGroup`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Partition<T> {
    /// No active dimensions left (or none were active to begin with): the
    /// rows to place directly. In the reference this is exactly the point
    /// where `measureItem`+`packRects` take over (`session_canvas.html:
    /// 1508-1516`) — real 2D rectangle-packing within this group's bounds,
    /// not something this module does. A caller wanting flat/ungrouped
    /// layout should call [`partition`] with an empty `dims` slice and get
    /// back exactly one `Leaves` containing every row, in input order.
    Leaves(Vec<T>),
    /// One active dimension at this depth: its rows split into ordered
    /// groups (empty ones dropped), each recursively partitioned by
    /// whatever dimensions remain. The reference lays depth-0 groups out
    /// side by side (columns) and every deeper depth's groups top to
    /// bottom (rows) inside their parent — that's a rendering/layout
    /// decision for whatever consumes this tree, not encoded here (the
    /// depth is implicit in tree nesting, so a caller doing the columns/
    /// rows layout can track it by recursion depth while walking this).
    Groups {
        dimension: GroupDimension,
        groups: Vec<PartitionGroup<T>>,
    },
}

/// Recursively partitions `rows` by `dims`, in the given order — the
/// nesting order IS the order dimensions appear in this slice, matching the
/// reference's click-order-via-insertion-order `activeGroups` semantics
/// exactly (session_canvas.html:1421-1428's comment explains why a `Set`
/// was chosen there for this reason; an ordered `Vec` here is the direct,
/// simpler equivalent — Rust doesn't need insertion-ordered-set semantics
/// smuggled through a `HashSet`/`BTreeSet` to get the same effect).
///
/// An empty `dims` (no active dimensions) returns `Partition::Leaves(rows)`
/// unchanged — the flat/ungrouped case, and also what today's
/// single-dimension "Flat" option in `overview.rs` already means.
pub fn partition<T: GroupKeyed>(rows: Vec<T>, dims: &[GroupDimension]) -> Partition<T> {
    match dims.split_first() {
        None => Partition::Leaves(rows),
        Some((&dimension, rest)) => {
            let groups = group_rows(rows, dimension)
                .into_iter()
                .map(|(key, items)| PartitionGroup {
                    key,
                    items: partition(items, rest),
                })
                .collect();
            Partition::Groups { dimension, groups }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Row {
        id: &'static str,
        category: &'static str,
        repo: &'static str,
        ticket: &'static str,
    }

    impl GroupKeyed for Row {
        fn group_key(&self, dim: GroupDimension) -> String {
            match dim {
                GroupDimension::Category => self.category.to_string(),
                GroupDimension::Origin => NONE_KEY.to_string(), // unused in these tests
                GroupDimension::Repo => self.repo.to_string(),
                GroupDimension::Ticket => self.ticket.to_string(),
            }
        }
    }

    fn sample_rows() -> Vec<Row> {
        vec![
            Row { id: "a", category: "implementing", repo: "zeron", ticket: "ENG-1" },
            Row { id: "b", category: "implementing", repo: "zeron", ticket: NONE_KEY },
            Row { id: "c", category: "research", repo: "workspace", ticket: "ENG-1" },
            Row { id: "d", category: "research", repo: "workspace", ticket: NONE_KEY },
            Row { id: "e", category: "quirky_unrecognized", repo: NONE_KEY, ticket: "ENG-2" },
        ]
    }

    fn ids_in_order(partition: &Partition<Row>) -> Vec<&'static str> {
        match partition {
            Partition::Leaves(rows) => rows.iter().map(|r| r.id).collect(),
            Partition::Groups { groups, .. } => {
                groups.iter().flat_map(|g| ids_in_order(&g.items)).collect()
            }
        }
    }

    #[test]
    fn zero_dimensions_is_flat_leaves_in_input_order() {
        let rows = sample_rows();
        let expected: Vec<&str> = rows.iter().map(|r| r.id).collect();
        let result = partition(rows, &[]);
        assert_eq!(result, Partition::Leaves(sample_rows()));
        assert_eq!(ids_in_order(&result), expected);
    }

    #[test]
    fn single_dimension_groups_by_repo_matching_todays_single_select_behavior() {
        let result = partition(sample_rows(), &[GroupDimension::Repo]);
        let Partition::Groups { dimension, groups } = &result else {
            panic!("expected Groups");
        };
        assert_eq!(*dimension, GroupDimension::Repo);
        // Dynamic order: alphabetical, NONE_KEY last.
        let keys: Vec<&str> = groups.iter().map(|g| g.key.as_str()).collect();
        assert_eq!(keys, vec!["workspace", "zeron", NONE_KEY]);
        let workspace = &groups[0];
        assert_eq!(ids_in_order(&workspace.items), vec!["c", "d"]);
        let zeron = &groups[1];
        assert_eq!(ids_in_order(&zeron.items), vec!["a", "b"]);
        let none = &groups[2];
        assert_eq!(ids_in_order(&none.items), vec!["e"]);
    }

    #[test]
    fn fixed_order_dimension_uses_full_fixed_list_and_catches_unrecognized_values_in_last_bucket() {
        let result = partition(sample_rows(), &[GroupDimension::Category]);
        let Partition::Groups { groups, .. } = &result else {
            panic!("expected Groups");
        };
        // Only non-empty fixed-order buckets survive, in CATEGORY_ORDER's order.
        let keys: Vec<&str> = groups.iter().map(|g| g.key.as_str()).collect();
        // "implementing" and "research" are real buckets; the unrecognized
        // "quirky_unrecognized" value falls into the LAST fixed bucket,
        // "other" — not a NONE_KEY bucket, matching the reference exactly.
        assert_eq!(keys, vec!["implementing", "research", "other"]);
        let other = groups.iter().find(|g| g.key == "other").unwrap();
        assert_eq!(ids_in_order(&other.items), vec!["e"]);
    }

    #[test]
    fn debug_category_sits_before_other_and_the_catch_all_stays_last() {
        let pos = |k: &str| CATEGORY_ORDER.iter().position(|c| *c == k).unwrap();
        assert!(pos("pr_review") < pos("debug") && pos("debug") < pos("research"));
        assert_eq!(CATEGORY_ORDER.last(), Some(&"other"));
        let rows = vec![
            Row { id: "a", category: "research", repo: "zeron", ticket: NONE_KEY },
            Row { id: "b", category: "debug", repo: "zeron", ticket: NONE_KEY },
            Row { id: "c", category: "not_a_category_yet", repo: "zeron", ticket: NONE_KEY },
        ];
        let Partition::Groups { groups, .. } = partition(rows, &[GroupDimension::Category]) else {
            panic!("expected Groups");
        };
        let keys: Vec<&str> = groups.iter().map(|g| g.key.as_str()).collect();
        assert_eq!(keys, vec!["debug", "research", "other"]);
    }

    #[test]
    fn two_dimensions_compose_and_nesting_order_follows_activation_order() {
        // Repo outer, Ticket inner.
        let repo_then_ticket = partition(
            sample_rows(),
            &[GroupDimension::Repo, GroupDimension::Ticket],
        );
        let Partition::Groups { dimension, groups } = &repo_then_ticket else {
            panic!("expected Groups");
        };
        assert_eq!(*dimension, GroupDimension::Repo);
        let zeron_group = groups.iter().find(|g| g.key == "zeron").unwrap();
        let Partition::Groups { dimension: inner_dim, groups: inner_groups } = &zeron_group.items
        else {
            panic!("expected nested Groups");
        };
        assert_eq!(*inner_dim, GroupDimension::Ticket);
        let inner_keys: Vec<&str> = inner_groups.iter().map(|g| g.key.as_str()).collect();
        assert_eq!(inner_keys, vec!["ENG-1", NONE_KEY]);

        // Ticket outer, Repo inner — same rows, opposite activation order,
        // must produce a genuinely different top-level split. This is the
        // actual "composability" behavior, verified explicitly rather than
        // assumed correct from the recursion alone.
        let ticket_then_repo = partition(
            sample_rows(),
            &[GroupDimension::Ticket, GroupDimension::Repo],
        );
        let Partition::Groups { dimension: outer_dim2, groups: outer_groups2 } = &ticket_then_repo
        else {
            panic!("expected Groups");
        };
        assert_eq!(*outer_dim2, GroupDimension::Ticket);
        let eng1_group = outer_groups2.iter().find(|g| g.key == "ENG-1").unwrap();
        let Partition::Groups { dimension: inner_dim2, groups: inner_groups2 } = &eng1_group.items
        else {
            panic!("expected nested Groups");
        };
        assert_eq!(*inner_dim2, GroupDimension::Repo);
        let inner_keys2: Vec<&str> = inner_groups2.iter().map(|g| g.key.as_str()).collect();
        assert_eq!(inner_keys2, vec!["workspace", "zeron"]);

        // Same rows end up in the tree either way, just nested differently —
        // the two orderings are not the same tree shape.
        assert_ne!(repo_then_ticket, ticket_then_repo);
        let mut ids_a = ids_in_order(&repo_then_ticket);
        let mut ids_b = ids_in_order(&ticket_then_repo);
        ids_a.sort_unstable();
        ids_b.sort_unstable();
        assert_eq!(ids_a, ids_b, "same underlying rows either way, only the nesting differs");
    }

    #[test]
    fn empty_rows_produce_no_groups_for_a_dynamic_dimension() {
        let result = partition(Vec::<Row>::new(), &[GroupDimension::Repo]);
        assert_eq!(result, Partition::Groups { dimension: GroupDimension::Repo, groups: vec![] });
    }

    #[test]
    fn empty_rows_still_enumerate_every_fixed_bucket_but_all_empty_so_all_filtered_out() {
        // Fixed-order dims return their full order regardless of what's in
        // `rows` (see `compute_order`'s doc comment) — but every bucket is
        // empty with no rows at all, so group_rows's empty-bucket filter
        // still drops all of them. This exercises that path explicitly
        // rather than assuming it degrades gracefully.
        let result = partition(Vec::<Row>::new(), &[GroupDimension::Category]);
        assert_eq!(result, Partition::Groups { dimension: GroupDimension::Category, groups: vec![] });
    }
}
