# Zeron unfinished-work handoff

Status: active dirty working tree; several features are implemented locally but are not committed or merged.

Prepared: 2026-10-05.

Repository: `/Users/mikey/Projects/agent-mode-tools/zeron`

Branch and source baseline:

- Branch: `import-external-sessions`
- HEAD: `51c676c5 Complete session collaboration and repository topology`
- `origin/import-external-sessions` currently points at the same commit.
- There are 25 modified tracked files, approximately 2,594 insertions and 340 deletions beyond HEAD.
- Treat the entire dirty tree as user work. Do not reset, discard, or rewrite it wholesale.
- No commit was created for the work described below.

## Start here in the next chat

1. Read this document and inspect `git status --short` plus `git diff --stat`.
2. Preserve all existing edits. Several features share `rpc.rs`, `shell.rs`, `repository_topology.rs`, transcript normalization, and tests.
3. Confirm the latest native build is actually running before diagnosing missing UI. The usual test app is `/Applications/Zeron Dev.app`; copying only to `target/debug/zeron` does not update it.
4. Prioritize the submodule diff expansion described below. Keep durable historical chat diffs as a separate design problem even though the symptoms overlap.
5. Before committing, run focused tests, the full relevant suites, and review/split the large dirty tree into coherent commits or PRs.

## Definitely unfinished

### 1. Expand committed submodule changes into real file diffs

Current behavior is still SHA-only. A parent/superproject diff represents a changed submodule as a Git mode `160000` gitlink transition:

```text
old submodule commit -> new submodule commit
```

Zeron does not currently drill into the initialized submodule, show its changed files, or let the user open those nested file diffs. Moving a chat to the main workspace does not change this Git behavior.

The worktree closeout implementation is not a solution for this. Closeout now expands *dirty nested paths* for deletion warnings, but the Changes pane still renders committed submodule movement as a gitlink hash change.

Relevant current architecture:

- [`crates/engine/src/diff_sync.rs`](../../crates/engine/src/diff_sync.rs) captures flat parent-checkout patches with `git diff`, produces `DiffFileSummary`, and reads old/new file bodies relative to one checkout root.
- [`crates/proto/src/entities.rs`](../../crates/proto/src/entities.rs) defines a flat `DiffFileSummary` without nested-repository origin or old/new nested revisions.
- `GET_CHECKOUT_FILE_DIFF_TEXT` in [`crates/engine/src/rpc.rs`](../../crates/engine/src/rpc.rs) recomputes the parent snapshot, finds a flat path, and calls `read_diff_file_text_at` with the parent root and parent revisions.
- [`crates/ui/src/changes.rs`](../../crates/ui/src/changes.rs) parses and renders a flat unified patch.

Simply concatenating a nested patch is insufficient: the UI may render it, but opening a file would still ask the engine to read that file from the parent repository and the wrong revisions.

Recommended first version:

1. Detect gitlink entries using a raw/tree diff that exposes mode `160000`, path, old object ID, and new object ID.
2. Resolve the declared submodule path safely beneath the parent checkout. Never follow an escaping symlink or trust an arbitrary wire path.
3. If the submodule is initialized and both objects exist locally, capture `oldSha..newSha` inside that repository.
4. Add backward-compatible structured origin metadata to nested file summaries, or introduce a nested diff section. At minimum the engine must retain:
   - parent-relative submodule path;
   - file path inside the submodule;
   - old nested revision;
   - new nested revision;
   - display path such as `flagship/src/lib.rs`.
5. Update file-text RPC reads to validate the nested root under the checkout and use the nested revisions with `git cat-file`/equivalent.
6. Render an expandable submodule row in Changes with aggregate file/addition/deletion counts and clickable nested files.
7. Preserve the current SHA-only row as a graceful fallback when the submodule is uninitialized, either object is missing, or inspection fails.
8. Keep checksum, truncation, patch byte caps, binary handling, rename detection, and stale-read checks correct for the combined snapshot.

Cover at least these cases:

- initialized submodule with one or multiple committed changes;
- uninitialized submodule;
- old/new object missing locally;
- added or removed submodule (zero object ID on one side);
- dirty submodule working tree in addition to a committed gitlink move;
- branch, working-tree, latest-turn, and per-commit scopes;
- nested submodule recursion with a conservative depth/repository bound;
- file preview and source opening from a nested result;
- combined payload truncation and checksum invalidation.

### 2. Durable chat-associated diffs

This is separate from submodule expansion and remains unresolved.

`CheckoutDiffSync` is checkout-scoped latest state, not a durable record of everything a chat changed. If a chat is moved to another checkout, its original worktree is removed, or the agent commits and leaves the tree clean, the chat can show an empty or unrelated diff. The latest-turn tree is in memory and is unavailable after an engine restart.

The reported example was:

- `Kit Line Ship Dates`
- chat `94db4513-e8fa-4553-9a35-0a202a684bd8`
- session `01a0fdb0-2cd2-7671-bbd2-075eb2d91032`

Decide the product contract before implementing:

- current checkout changes;
- branch changes since the chat's recorded base;
- latest turn only;
- or a durable union/range of commits and working changes attributable to the chat.

A robust direction is to persist chat/turn start and end tree or commit identities with the original checkout/repository identity, then render a historical immutable diff even after moving the chat. Do not infer authorship solely from whatever checkout the chat currently points to.

### 3. Privacy/TCC prompts for unrelated folders

The user repeatedly observed Zeron requesting access to Music, Photos, and other unrelated folders. Some automatic scanning paths now use `is_automatic_access_blocked`, including closeout shared-chat checks, but there has not been a confirmed end-to-end test proving all unwanted prompts are gone.

Treat this as unresolved until reproduced and verified with the latest app. Audit background discovery, canonicalization, repository scanning, historical chat cwd reconciliation, and file watchers. Read metadata already stored in the document before touching a historical path, and never probe broad/privacy-sensitive folders merely to classify or reconcile unrelated chats.

### 4. Git integration and release packaging

The current work is not committed, split, pushed, merged, or packaged as a release.

- The latest debug binary was manually copied into `/Applications/Zeron Dev.app` and ad-hoc re-signed.
- That makes the local app testable, but it is not a reproducible distribution artifact.
- Ad-hoc re-signing may cause macOS to request permissions again.
- The Windows package/build used earlier in the conversation predates the latest dirty changes.
- No full current Windows build or native Windows acceptance pass has been performed.

Use `split-to-prs` or otherwise separate the dirty work into reviewable units before merging. At minimum, keep worktree closeout, Repo Map refinements, and transcript/code-reference changes independently reviewable where shared files permit.

## Implemented locally but still needs user acceptance or broader verification

### Worktree closeout safety and Repo Map entry point

Implemented in the dirty tree:

- Repo Map shows **Close out worktree...** in the inspector for every non-main linked worktree, including unmatched worktrees.
- Sidebar and Overview recognize ordinary linked worktrees whose root has a `.git` file; `.workspace-root` is no longer required.
- Clean merged worktrees use the normal confirmation.
- Dirty, unmerged, detached, or unverifiable work uses the explicit two-click destructive confirmation.
- Dirty inspection failure is a hard block.
- Main/default worktrees, live processes, and shared open-chat ownership are blocked.
- Dirty initialized nested repositories show prefixed real paths in the warning.
- The engine re-inspects immediately before deletion to prevent plan/close races.

The code was built and manually installed in `/Applications/Zeron Dev.app`, but the user has not yet confirmed the final control is visible and successfully completes a real closeout.

Primary files:

- [`crates/engine/src/chat_workspace_plan.rs`](../../crates/engine/src/chat_workspace_plan.rs)
- [`crates/engine/src/rpc.rs`](../../crates/engine/src/rpc.rs)
- [`crates/ui/src/chat_closeout.rs`](../../crates/ui/src/chat_closeout.rs)
- [`crates/ui/src/shell/closeout_ui.rs`](../../crates/ui/src/shell/closeout_ui.rs)
- [`crates/ui/src/repository_topology.rs`](../../crates/ui/src/repository_topology.rs)

### Repo Map refinements

Implemented locally but not committed:

- workspace tabs instead of depending on the currently open chat;
- worktree-first graph/inspector details;
- search across repositories, worktrees, paths, branches, SHAs, chat IDs/titles, tickets, and PR URLs;
- consistent node/text scaling during zoom;
- background refresh preserves the current map rather than blanking it with “Mapping repositories and worktrees...”;
- expandable matched-chat details, agent rows, linked PRs, latest-message preview, Changes navigation, and lightweight diff peeks;
- closeout action for non-main worktrees.

Native visual acceptance is still required for dense graphs, very small zoom, search filtering, refresh behavior, and the new closeout button.

### Transcript tool output and code navigation

Implemented locally but not committed:

- Claude and Codex tool-result text is retained with bounds instead of being discarded.
- Imported Claude parent/subagent transcripts preserve bounded tool output.
- document events keep a compact summary and sidecar reference for fuller tool output;
- expanded transcript command/output rows remain horizontally inspectable rather than permanently truncated;
- inline code references such as `src/lib.rs:42:7` can become safe workspace links;
- Files can reveal the requested one-based line/column and switch Markdown preview to source when necessary.

This still needs native end-to-end testing with real Codex and Claude transcripts, large outputs, stale paths, renamed files, and a restarted app. It does not by itself create a durable historical chat diff.

### Slash-command/global skill discovery

The dirty tree changes Codex command discovery so it refreshes rather than retaining an indefinitely stale command list. Global skills were also installed earlier, but there is no final end-to-end confirmation in the latest Zeron build that `/talk-to-me` appears in a Codex session for every repository/chat. Verify in Zeron's composer; do not treat CLI behavior as proof of the in-app picker.

## Verification already completed

The following passed for the latest combined source unless noted:

- `cargo fmt --all -- --check`
- `git diff --check`
- `cargo build -p zeron --quiet`
- closeout engine unit tests: 46 passed, 1 ignored
- closeout RPC integration tests: 7 passed with the environment-sensitive live-process test skipped
- `cargo test -p zeron-ui chat_closeout --lib --quiet`: 13 passed
- `cargo test -p zeron-ui repository_topology --lib --quiet`: 22 passed

The live-process integration test uses real `ps`/`lsof` process enumeration and cannot run in the current sandbox, which reports process-enumeration restrictions. Run it on an unrestricted local environment before release.

Not yet completed:

- the full workspace test suite after all dirty changes were combined;
- a release build/package of the latest tree;
- latest Windows build and native acceptance;
- native macOS acceptance of every dirty UI feature;
- code review and security review of the entire 2,500+ line dirty diff;
- coherent commits/PRs and merge.

## Current modified files

At handoff time:

```text
crates/doc/src/parts.rs
crates/doc/src/schema.rs
crates/engine/src/chat_workspace_plan.rs
crates/engine/src/external_import.rs
crates/engine/src/rpc.rs
crates/engine/src/sessions.rs
crates/engine/src/subagent_transcript.rs
crates/engine/tests/chat_workspace_plan_rpc.rs
crates/engine/tests/external_import.rs
crates/engine/tests/external_import_links_repair.rs
crates/harness/src/claude/normalize.rs
crates/harness/src/claude/wire.rs
crates/harness/src/codex/mod.rs
crates/harness/src/codex/normalize.rs
crates/harness/tests/codex.rs
crates/harness/tests/fixtures/fake-codex.sh
crates/ui/src/chat_closeout.rs
crates/ui/src/files/mod.rs
crates/ui/src/files/preview.rs
crates/ui/src/markdown/render.rs
crates/ui/src/overview.rs
crates/ui/src/repository_topology.rs
crates/ui/src/shell.rs
crates/ui/src/shell/closeout_ui.rs
crates/ui/src/transcript.rs
```

Re-run `git status --short` because this list may change after the handoff is created.
