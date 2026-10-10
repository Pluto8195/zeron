# Zeron unfinished-work handoff

Status: the original repository/transcript implementation is committed on the fork's `main`. A second, much larger follow-up is implemented in the working tree, tested in focused suites, built, and installed locally, but is not committed and still needs native acceptance.

Prepared: 2026-10-05. Updated: 2026-10-08.

Repository: `/Users/mikey/Projects/agent-mode-tools/zeron`

Branch and source baseline:

- Branch: `import-external-sessions`
- HEAD: `f7dda4db Update unfinished-work handoff after fork merge`
- `origin/import-external-sessions` and the fork's `origin/main` point at `f7dda4db`.
- The implementation committed before this follow-up is `78a999e9 Finish repository workflows and transcript tooling`.
- The current working tree is intentionally dirty: 30 source/test files, about 6,316 insertions and 580 deletions at the time of this update. Do not discard or overwrite it.
- The dirty source was built on 2026-10-06 at 15:55 and installed/ad-hoc signed as `/Applications/Zeron Dev.app` at 15:57. The app was not automatically restarted; restart it manually when testing.

## Start here in the next chat

1. Read this document, then inspect `git status --short` and `git diff --stat`.
2. Preserve the entire dirty tree. The follow-up spans engine, protocol, document schema, transcript normalization, Repo Map, Canvas, Changes, shell, and tests.
3. Confirm `/Applications/Zeron Dev.app` was restarted after the 2026-10-06 install before diagnosing missing UI.
4. Run the manual acceptance checklist below against real repositories/chats. Most remaining uncertainty is native behavior and live data, not missing source implementation.
5. Before committing or opening an upstream PR, run the full workspace suite, review the 6,000+ line diff, and split it into coherent changes where shared files permit.

## Implemented in the current dirty tree

### 1. Worktree closeout safety and introspection

The closeout flow now exposes the state that can block or make deletion risky before the user confirms:

- Repo Map starts a closeout preflight for a selected local, non-main worktree and shows clean/dirty state, dirty paths, unmerged state, inspection failures, and other active chats.
- Dirty and unverifiable work remains behind the explicit destructive confirmation; dirty inspection failure remains a hard block.
- Unmerged commits are returned as bounded newest-first metadata, shown in the closeout dialog, and open an existing commit-diff surface rooted at that worktree.
- **View uncommitted changes** opens the worktree diff inside the closeout modal.
- Other active chats using the worktree are named and can be opened for inspection. Archived chats are visible in Repo Map but do not block closeout.
- Main/default worktrees, live processes, shared active chat ownership, and immediately re-inspected unsafe state remain protected.
- The closeout body scrolls, so long dirty-file/commit/chat lists do not push actions out of reach.
- Repo Map also shows a compact read-only preflight summary before the closeout dialog is opened.

Primary files:

- [`crates/engine/src/chat_workspace_plan.rs`](../../crates/engine/src/chat_workspace_plan.rs)
- [`crates/engine/src/rpc.rs`](../../crates/engine/src/rpc.rs)
- [`crates/ui/src/chat_closeout.rs`](../../crates/ui/src/chat_closeout.rs)
- [`crates/ui/src/shell/closeout_ui.rs`](../../crates/ui/src/shell/closeout_ui.rs)
- [`crates/ui/src/repository_topology.rs`](../../crates/ui/src/repository_topology.rs)

### 2. Repo Map chat matching and exact-worktree chat creation

The topology join was revised because real worktrees were incorrectly shown with no matched chats:

- Topology assembly uses all chats on the selected device, including archived chats and chats in another Space, rather than limiting the join to the currently selected Space.
- A chat's current `cwd` is matched against the deepest known worktree root first. Stored `checkout_id` and source-context checkout identity are fallbacks, preventing stale checkout metadata from overriding where the chat currently operates.
- Nested repositories prefer the most specific known root.
- Matching avoids broad/privacy-sensitive filesystem probing; known roots plus lexical normalization are sufficient for blocked historical paths.
- Every worktree inspector has **New chat in this worktree**, even when no chat matches. It opens a blank chat with `ReuseWorktree { path, branch }`, targeting that exact existing worktree instead of creating or selecting a different checkout.

Primary files:

- [`crates/engine/src/repository_topology.rs`](../../crates/engine/src/repository_topology.rs)
- [`crates/engine/src/rpc.rs`](../../crates/engine/src/rpc.rs)
- [`crates/ui/src/repository_topology.rs`](../../crates/ui/src/repository_topology.rs)
- [`crates/ui/src/shell.rs`](../../crates/ui/src/shell.rs)
- [`crates/ui/src/pickers.rs`](../../crates/ui/src/pickers.rs)

### 3. Committed submodule diffs

The previously SHA-only submodule gap now has a first implementation:

- Diff capture detects Git mode `160000` entries using raw, no-abbreviation Git diffs.
- Initialized, declared submodules with both revisions available locally are expanded into bounded nested patches and file summaries.
- Protocol/document sidecars carry structured submodule origin, old/new revision, file counts, stats, truncation, and nested repository path.
- Changes renders expandable submodule groups and opens nested file text using the nested repository plus nested revisions instead of reading through the parent repository.
- Branch/working-tree, commit, and turn diff capture include submodule sections.
- Expansion is bounded to depth 2 and eight repositories, validates paths without following escaping symlinks, and includes nested data in checksum/truncation behavior.
- Uninitialized/missing-object/added/removed/type-changed/renamed cases retain the safe SHA-only gitlink fallback.

Primary files:

- [`crates/engine/src/diff_sync.rs`](../../crates/engine/src/diff_sync.rs)
- [`crates/engine/src/rpc.rs`](../../crates/engine/src/rpc.rs)
- [`crates/proto/src/entities.rs`](../../crates/proto/src/entities.rs)
- [`crates/ui/src/changes.rs`](../../crates/ui/src/changes.rs)

### 4. Full tool output/diff recovery and edit introspection

The transcript-side **Couldn't load full diff — retry** path is no longer edge-only:

- Full tool output and unified edit patches are atomically cached beneath the profile's `tool-blobs` directory, so they survive app restarts.
- Fetch order is local cache, durable run-journal recovery/backfill, then edge storage.
- Journal recovery walks parent and recursively wrapped subagent events and uses the latest matching tool result.
- Failed full-output/full-diff rows remain clickable retry controls, preserve the failure for a tooltip, and stop click propagation correctly.
- Opening an edit chip prefetches only that edit's diff sidecar.
- Codex edit/apply-patch normalization now retains a bounded unified patch. Expanded edit rows render real per-file patches and line numbers, including multi-file and headerless patches, rather than only saying that a file was edited.

Primary files:

- [`crates/doc/src/parts.rs`](../../crates/doc/src/parts.rs)
- [`crates/engine/src/doc_host.rs`](../../crates/engine/src/doc_host.rs)
- [`crates/engine/src/sessions.rs`](../../crates/engine/src/sessions.rs)
- [`crates/harness/src/codex/normalize.rs`](../../crates/harness/src/codex/normalize.rs)
- [`crates/proto/src/agent.rs`](../../crates/proto/src/agent.rs)
- [`crates/ui/src/transcript.rs`](../../crates/ui/src/transcript.rs)

### 5. Codex visualization envelopes

Zeron now recognizes the Codex control envelope that previously appeared as raw text, for example `visualize` payloads pointing at `*.fragment.html`:

- Valid bounded payloads become a dedicated visualization card with title, path, and wide-mode label.
- Activating the card resolves the file through the existing safe workspace-link path.
- Surrounding Markdown remains intact, streaming keeps stable row/parser identities, and incomplete control-token prefixes are suppressed while streaming.
- Payload keys, path type, title length, and total size are strictly bounded; malformed or incomplete envelopes do not render as raw control syntax.

This is display/link support, not an embedded HTML renderer. Native acceptance should confirm that opening the artifact is the desired product behavior.

Primary file: [`crates/ui/src/transcript.rs`](../../crates/ui/src/transcript.rs).

### 6. Canvas and Repo Map layout/navigation

The Canvas keeps the existing information density while changing the visual geometry:

- Resting cards are wider and shorter (200–300 px with a 0.44 width aspect floor), showing more title without making the board vertically heavy.
- Hover temporarily reveals a second title line, branch, latest-message preview, and the full subagent stack without repacking the board.
- Expanded detail uses an explicit 480 px minimum surface. Long PR/ticket text is contained there and no longer stretches the collapsed face or its context meter.
- Repo/ticket/chat group hierarchies pack into deterministic two-dimensional clusters at every level rather than long alternating rows/columns.
- `Cmd/Ctrl+F` focuses and selects the Canvas search text.
- `Cmd/Ctrl+Shift+G` moves from Canvas to Repo Map; the Repo Map button advertises the shortcut.
- Repo Map renders more than four sibling worktrees as a compact 2–4 column grid. The 24-worktree fixture is 4×6, keeps branch/path/status/occupancy data, and stays under 1,100 px high.

Primary files:

- [`crates/ui/src/overview.rs`](../../crates/ui/src/overview.rs)
- [`crates/ui/src/overview_layout.rs`](../../crates/ui/src/overview_layout.rs)
- [`crates/ui/src/repository_topology.rs`](../../crates/ui/src/repository_topology.rs)
- [`crates/ui/src/shell.rs`](../../crates/ui/src/shell.rs)

### 7. Full-chat metadata

The open full-chat header now reuses the compact metadata strip rather than leaving Canvas-only context behind:

- linked PR and ticket chips are actionable URLs;
- model, branch, repository/workspace, category, origin, chat ID, and session ID are included when available;
- category/origin are fetched through `CHAT_CLASSIFICATION` with a short-lived cache;
- PR/ticket link status is shared with the existing multi-PR metadata path.

Primary files:

- [`crates/ui/src/chat_metadata.rs`](../../crates/ui/src/chat_metadata.rs)
- [`crates/ui/src/shell.rs`](../../crates/ui/src/shell.rs)

### 8. PR sidebar loading

The empty PR sidebar race and GUI environment mismatch have source fixes:

- The initial authored-PR RPC waits up to five seconds for the first background `gh search` success or failure rather than immediately returning the still-empty cache.
- Every GitHub CLI invocation resolves `gh` through the login-shell executable search, composes the login-shell `PATH`, disables prompts, and closes stdin for GUI-safe operation.
- The sidebar distinguishes **Loading...**, a genuinely empty/unavailable `gh` result, and a search with no matches.
- A login-shell integration test verifies executable resolution with an isolated fake `gh`.

Primary files:

- [`crates/engine/src/pr_ticket_cache.rs`](../../crates/engine/src/pr_ticket_cache.rs)
- [`crates/engine/src/rpc.rs`](../../crates/engine/src/rpc.rs)
- [`crates/harness/src/lib.rs`](../../crates/harness/src/lib.rs)
- [`crates/ui/src/overview.rs`](../../crates/ui/src/overview.rs)
- [`crates/engine/tests/pr_ticket_cache_login_shell.rs`](../../crates/engine/tests/pr_ticket_cache_login_shell.rs)

### 9. Privacy-sensitive automatic access

Additional automatic paths reject macOS privacy-managed historical locations before repository or GitHub inspection:

- change-request watches;
- PR/worktree cache probes;
- Canvas workspace-root discovery;
- closeout candidate reconciliation and topology matching.

This reduces known prompt sources but does not prove all Music/Photos/Documents prompts are gone.

## Definitely unfinished or still risky

### 1. Durable chat-associated diffs

This remains distinct from tool edit sidecars and submodule expansion.

`CheckoutDiffSync` is checkout-scoped latest state, not a durable record of everything a chat changed. If a chat moves to another checkout, its worktree is removed, or the agent commits and leaves the tree clean, the chat can still show an empty or unrelated checkout diff. The latest-turn tree is in memory and unavailable after an engine restart.

The reported example was:

- `Kit Line Ship Dates`
- chat `94db4513-e8fa-4553-9a35-0a202a684bd8`
- session `01a0fdb0-2cd2-7671-bbd2-075eb2d91032`

Decide whether the product promises current checkout changes, branch changes since the recorded base, latest turn only, or a durable union/range attributable to the chat. A robust design should persist chat/turn start and end tree or commit identities with the original checkout/repository identity.

### 2. Native acceptance of the dirty implementation

Unit coverage is strong, but the current app still needs real-data acceptance for:

- topology matching across archived/cross-Space/stale-checkout chats;
- exact-worktree new-chat creation;
- dirty/unmerged/shared-chat closeout inspection and actual safe deletion;
- nested submodule file rendering/opening and fallback cases;
- full diff retry after an app restart and when edge storage is unavailable;
- edit patches from real Codex sessions;
- visualization-card opening;
- Canvas density, hover behavior, long metadata, shortcuts, and large worktree sets;
- full-chat PR/ticket/category/origin metadata;
- PR sidebar population with the user's real `gh` authentication.

### 3. Privacy/TCC prompts

Several automatic probes are now guarded, but there has not been a confirmed end-to-end pass proving unwanted Music, Photos, Pictures, or Documents prompts are gone. Continue auditing background discovery, canonicalization, repository scanning, historical chat cwd reconciliation, and file watchers if a prompt recurs. Do not probe a historical path merely to classify it.

### 4. Packaging, full verification, and upstream integration

- The current 6,000+ line follow-up is uncommitted and unsplit.
- The local app is a debug binary manually copied into `/Applications/Zeron Dev.app` and ad-hoc signed; it is not a reproducible release artifact.
- Ad-hoc signing can cause macOS permissions to be requested again.
- No current Windows build/native acceptance has been performed.
- The full workspace test suite, full release build/package, security review, and whole-diff code review have not been completed.
- No upstream PR/merge strategy exists for the dirty follow-up.

## Verification completed

The following focused commands passed against the current dirty source on 2026-10-08:

- `cargo test -p zeron-engine repository_topology --lib --quiet`: 4 passed.
- `cargo test -p zeron-engine diff_sync --lib --quiet`: 12 passed.
- `cargo test -p zeron-engine chat_workspace_plan --lib --quiet`: 46 passed, 1 ignored.
- `cargo test -p zeron-engine pr_ticket_cache --lib --quiet`: 28 passed, 3 ignored.
- `cargo test -p zeron-ui repository_topology --lib --quiet`: 26 passed.
- `cargo test -p zeron-ui overview --lib --quiet`: 107 passed.
- `cargo test -p zeron-ui transcript --lib --quiet`: 107 passed.
- `cargo test -p zeron-ui chat_metadata --lib --quiet`: 6 passed.
- `cargo test -p zeron-ui repo_map_new_chat_targets_the_exact_existing_worktree --lib --quiet`: 1 passed.

Previously completed for the combined source before the last install:

- `cargo fmt --all -- --check`
- `git diff --check`
- `cargo build -p zeron --quiet`
- closeout RPC integration tests: 7 passed with the environment-sensitive live-process test skipped.
- `cargo test -p zeron-ui chat_closeout --lib --quiet`: 13 passed.

The live-process integration test uses real `ps`/`lsof` enumeration and cannot run in the restricted sandbox. Run it in an unrestricted local environment before release.

## Manual acceptance checklist

After manually restarting `/Applications/Zeron Dev.app`:

1. Open Repo Map and select known worktrees. Confirm archived and active chats match the correct worktree rather than all worktrees reporting no chats.
2. On an unmatched worktree, click **New chat in this worktree** and verify the new composer/session reuses the exact displayed path and branch.
3. Select clean, dirty, unmerged, shared-chat, and main worktrees. Confirm the preview is accurate; open dirty changes, each unmerged commit, and each shared chat before attempting closeout.
4. Use a disposable linked worktree to confirm safe closeout succeeds, while main/live/shared/uninspectable work remains blocked.
5. Open a repository with a committed submodule move. Confirm initialized submodules expand into nested files, files open correctly, and uninitialized/missing-object cases stay SHA-only without failing the whole diff.
6. Restart Zeron, open an older edit tool row, and load its full diff. Exercise a forced failure and click the retry row.
7. Open a real Codex multi-file edit and confirm file names, hunks, and line numbers are visible inline.
8. Open a chat containing a `visualize` envelope and confirm it shows a visualization card rather than raw control text; activate the card.
9. In Canvas, test compact cards, hover reveal, a pathological long PR name, repo→ticket grouping, `Cmd+F`, and `Cmd+Shift+G`.
10. In Repo Map, inspect a repository with many worktrees and confirm the compact grid remains readable and selectable.
11. Open a full chat with linked PRs/tickets and confirm the metadata chips and category/origin appear and open correctly.
12. Open the PR sidebar after launch and confirm authored PRs load using the same `gh` authentication as the user's shell.
13. Watch macOS privacy prompts during startup, Canvas/Repo Map navigation, historical chat opening, and closeout planning.

## Current dirty-file groups

Use `git status --short` for the authoritative list. At this update the follow-up spans:

- document/schema: `crates/doc/src/{parts,rebuild,schema}.rs`;
- engine: change requests, chat workspace planning, diff sync, document host, PR/ticket cache, repository topology, RPC, sessions, and source control;
- engine tests: closeout RPC, repository/diff integration, and login-shell PR cache coverage;
- harness/protocol: ACP/Codex normalization, executable resolution, agent events, and diff entities;
- UI: Changes, closeout, chat metadata, Overview/Canvas, pickers, Repo Map, shell/tabs, and transcript.

Do not assume a clean tree, do not reset these changes, and do not rebuild/reinstall automatically when the user only asks for status. When installing another build, install and sign it without automatically quitting or relaunching Zeron; let the user restart manually.
