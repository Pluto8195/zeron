# Repository-scoped collaboration: implementation handoff

Status: proposed design; no feature implementation performed.
Prepared: 2026-10-03.
Source baseline: `da6ad0dc2b25506a60b71778a18b8ca358f76b33` plus the existing dirty working tree. Several engine, RPC, workspace, repository-topology, and UI files have uncommitted changes. Treat those changes as user work; inspect the actual implementation before applying this plan.

## Outcome

A client signs into the Windows Zeron app with their own account, accepts an invitation to one ecommerce repository hosted on the owner's Mac, starts chats in separate worktrees, and opens the corresponding running site. Agents, builds, tests, and dev servers run on the Mac's compute. The client receives only the shared repository's collaboration data and has no general control over the owner's Zeron instance.

The owner keeps personal chats, other repositories, and personal worktree contents private while using the same desktop app. Sharing must be an explicit action on a repository. No shared login, manual second desktop session, or general SSH access is part of the client workflow.

Here, **sync means sharing chat/state/preview access to remote execution**. It does not mean continuously mirroring the entire source tree to Windows. Git remains the source-control mechanism; Windows renders the app and site.

## Recommended first version

- One owner, one designated Mac host, and explicitly invited collaborators per share.
- Separate authenticated accounts, including guests whose active organization differs from the owner's. A matching organization alone never grants access.
- A new shared collaboration area for one repository, with chats and worktrees created there. All members can see that area's chats and worktrees. Personal chats stay personal, even if associated with the same repository.
- The owner can invite/remove members, configure the runtime/provider, approve restricted actions, and close the share. Collaborators can create worktrees/chats, send and steer messages, inspect diffs/files, attach references, and open previews.
- Git commits and local development within the shared runtime are supported. Remote push/merge/deployment credentials and destructive cleanup remain owner-controlled in the first version.
- Reuse the existing transport, transcript, worktree, and preview components where their trust assumptions hold.
- Implement a narrow authenticated command path for guests. The designated host publishes canonical shared registry and transcript updates.
- Support one explicitly validated agent harness in an isolated execution runtime first. Add further harnesses only after the same isolation tests pass.

Defer viewer roles, private conversations between subsets of share members, arbitrary terminal access, importing historical chats, multi-host failover, and iOS collaboration UI. Older clients retain personal functionality and cannot join shared resources they do not support.

### Visibility contract and Git limits

Sharing does not automatically publish existing chats or attach every existing worktree. For v1, create new shared chats and worktrees. A future explicit import must review the entire conversation, attachments, and related subagent/parent links before publishing it.

Worktree visibility and Git history are different boundaries. Linked worktrees share refs, objects, and Git metadata. A hidden worktree does **not** make its committed branch/history confidential. Members receive the history/refs present in the collaboration repository. If some committed code must remain private, it must not be imported or fetched into that repository; the correct boundary is a separate sanitized repository.

Use a separate collaboration clone/object database for execution, with no shared object alternates or hardlinks into the owner's personal Git store. Shared worktrees link only to that collaboration clone. Do not expose the owner's personal `.git` common directory, linked-worktree paths, local config, hooks, or credentials to the guest runtime. This still represents the same logical ecommerce repository, with an explicit owner-controlled integration step between personal and shared work.

## Verified current architecture

The findings below describe code inspected for this handoff, not a tested Windows-to-Mac deployment.

| Area | Current behavior | Implementation consequence |
| --- | --- | --- |
| Identity | [`edge/src/auth.ts`](../../edge/src/auth.ts) verifies user and optional organization/session claims. | Add explicit invitations and share membership; keep JWT verification. |
| Registry | [`edge/src/index.ts`](../../edge/src/index.ts) routes to `reg1/{orgId}/{userId}`. [`registry-room.ts`](../../edge/src/registry-room.ts) accepts generic row mutations and client HLC/device values. | Preserve personal registries. Publish a separate, bounded shared index; membership cannot be an ordinary client-editable row. |
| Chats | [`chat-room.ts`](../../edge/src/chat-room.ts) owner-claims rooms and relays opaque CRDT updates/checkpoints. | Guests cannot be given unrestricted document-write access. Reuse the log/read protocol with host-only publication. |
| Device RPC | [`device-room.ts`](../../edge/src/device-room.ts) requires the same owner user; [`EngineCore::start_host_relay`](../../crates/engine/src/lib.rs) exposes the full `EngineRpc`. | Add a separate share-authorized path. Do not relax the owner gate on the existing device endpoint. |
| Caller identity | [`crates/rpc/src/device_room.rs`](../../crates/rpc/src/device_room.rs) creates virtual RPC connections carrying a connection ID; verified human identity is not part of host dispatch. | Bind a verified principal and share to the connection/intent; never trust caller-supplied actor fields. |
| Local IPC/MCP | [`crates/rpc/src/server.rs`](../../crates/rpc/src/server.rs) rejects browser Origins but does not authenticate a local caller; [`crates/mcp/src/tools.rs`](../../crates/mcp/src/tools.rs) exposes chat operations through the engine. | Guest processes must not reach the personal IPC listener or reuse `zeron mcp` as a privileged route. Origin/device/chat labels are not authorization. |
| Commands | [`doc_host.rs`](../../crates/engine/src/doc_host.rs) queues commands with device attribution; `RelayCommand` accepts a command entry. | Existing attribution is not proof of authorization. Shared intents need authenticated actors and durable deduplication. |
| Repos/files | [`rpc.rs`](../../crates/engine/src/rpc.rs) forwards repo, folder, worktree, terminal, upload, account, and action methods. [`repos.rs`](../../crates/engine/src/repos.rs) uses host paths and device-specific checkout IDs. | Resolve opaque shared IDs into approved host resources. Deny the generic guest RPC surface. |
| Storage | [`profile.rs`](../../crates/engine/src/profile.rs) separates personal profile stores, but repos/worktrees/accounts remain device resources. | Add resource-level collaboration scope and separate caches; do not replace immutable engine profile scope. |
| Previews | [`preview-route.ts`](../../edge/src/preview-route.ts) is user-scoped; [`crates/preview`](../../crates/preview) tunnels HTTP/WebSockets over WebRTC. | Reuse transport, add share/peer/service authorization and expiring access. |
| Attachments | [`uploads.rs`](../../crates/engine/src/uploads.rs) owns native local uploads. Legacy edge `PUT /attachments/*` acknowledges and discards. `/blob/*` uses user-prefixed R2 keys. | Scope active native attachment delivery first. Do not design around the retired mirror or assume all tool sidecars are enabled. |
| Windows | [Windows development](../reference/windows-development.md) documents portable builds; [`browser/mod.rs`](../../crates/ui/src/browser/mod.rs) opens URLs in the default browser on Windows. | Reuse desktop UI and external preview opening. Actual cross-device acceptance remains required. |

Some older architecture and chat2 design comments are historical. For example, preview WebRTC exists despite earlier architecture prose about its removal. Use implementation and focused current docs as the source of truth.

## Trust and data model

Keep `WorkspaceScope`/`EngineProfile` as the immutable account/storage boundary established at engine startup. Introduce a separate resource-level scope, with proposed names such as:

```text
CollaborationScope = Personal | SharedRepo(shareId)

RepoShare
  id, ownerUserId, ownerOrgId, hostDeviceId
  repositoryId, displayName, aclRevision, state
  protocolVersion, executionPolicyId

ShareMember
  shareId, userId, role, membershipGeneration, state

ShareInvitation
  tokenHash, shareId, intendedRecipient, expiresAt, redeemedBy

SharedWorktree
  id, shareId, repositoryId, branch, createdBy, state

SharedChat
  id, shareId, worktreeId, createdBy, hostDeviceId

ShareIntent
  id, shareId, actorUserId, membershipGeneration
  kind, resourceId, payload, payloadHash, acceptedAt, state
```

These are proposed contracts, not existing types. Absolute paths, runtime identifiers, Git common-directory identities, provider secrets, and private account/device metadata live in host-only mappings. Current `checkoutId` remains useful internally; it is not a portable authorization key.

ACL data belongs to an authoritative edge store, accessed through typed operations. Introduce a `ShareRoom` Durable Object to own one share's membership, invitations, designated publisher, index publication, and durable intent inbox. A per-user share directory provides discovery without exposing anybody else's personal registry. Implement reliable directory updates with idempotent reconciliation; a stale directory entry must never authorize access.

Invitations are expiring, single-use, hashed server-side, and bound to the intended authenticated recipient. Redemption is atomic and binds the verified user ID. The current JWT verifier does not return verified email: if invitations target email, resolve verified email/user ownership server-side through the identity provider instead of trusting a client-supplied email. The owner's organization defines ownership/tenancy; the guest's verified user ID defines membership. Do not require the guest to switch into the owner's organization or silently grant all organization members access.

## Publication and transport design

Use additive routes, for example `/shares/:shareId/...`. Exact route names can follow repository conventions, but every resource request must verify membership plus ownership of that resource by the share.

Suggested room identities:

```text
share1/<shareId>                 membership, index, intent inbox
shared-chat1/<shareId>/<chatId>  canonical transcript log
shared-preview1/<shareId>       scoped service catalog and pairing
```

Keep the existing personal `reg1`, `chat2`, `d2`, and preview room behavior intact. Extract reusable room/log routines where helpful; do not copy the entire sync system or migrate everybody into shared rooms.

### Canonical host publication

The currently fenced designated host is the sole publisher of shared chat CRDT updates, checkpoints, tails, and diffs. The shared index is also a host-authored projection with a constrained schema. Guest changes are typed intents; the host applies valid changes and publishes the result. Owner interactions with shared resources use the same path, including from another owner device. Being the human owner alone does not bypass publisher fencing.

This restriction is necessary because an opaque CRDT update could change execution settings, fabricate another actor, mutate command outcomes, or replace history. Checking that a guest belongs to the room cannot authorize the contents of those bytes.

Add an explicit reader mode to the shared chat sync client/sink: it must never reseed a room, upload a checkpoint, replay personal pending CRDT writes, or use the personal reset/recovery path as a fallback. A room reset requires the designated host to republish; guest caches are not authoritative.

Enforce reader restrictions on both WebSocket frames and HTTP fallback: a collaborator may read authorized rows/checkpoints/tails/diffs but may not POST rows/checkpoints, reset a room, or PUT host sidecars.

Bind publisher access to the owner and designated host registration/session. Do not accept a claimed `deviceId` or client HLC as authentication. Use a broker-held host credential/session that guest execution processes cannot read. Define reconnect/replacement fencing so two host instances cannot publish or dispatch as the current executor simultaneously.

### Guest requests

Implement a `SharedRepoRpc`/share service with a default-deny operation table. It can reuse internal engine helpers after resolving scope. It must not wrap unrestricted `EngineRpc` and assume hidden UI controls are sufficient.

| Operation | Collaborator behavior in v1 |
| --- | --- |
| List chats/worktrees, read transcript/diff/files | Only the share's registered resources. |
| Create chat/worktree | Host chooses paths and verifies the base ref; publishes the new mapping. |
| Send/queue/steer/stop | Authenticated, idempotent intent for a shared chat; execution policy remains host-owned. |
| Answer questions | Allowed for the addressed shared run. Tool escalation outside runtime policy is denied; restricted approval is owner-only. |
| Upload/read attachments | Share-bound upload IDs and file references; no arbitrary host path. |
| Open preview | Explicitly advertised service associated with an approved shared worktree. |
| Commit | Inside the shared execution repository under its policy. |
| Push/merge/deploy, remove worktree/share, change membership/provider | Owner-only initially. |
| Browse host home/drives, add/clone arbitrary repo, global accounts/settings, raw terminal | Denied through guest connections. |

Prefer scope-bound connections or a service factory that creates dispatch with a verified context. The edge stamps `{actorUserId, shareId, membershipGeneration, role}` from authenticated state. Strip/overwrite spoofable forwarded identity headers. The Mac checks this context and the resource mapping again before execution. Shared RPC cannot choose a different host or tunnel into a personal device link.

Provide typed reads and a durable inbox for mutating actions. Reuse the existing relay framing for bounded interactive reads if useful, but connect it only to the restricted dispatcher. Large attachment streams need the same scoped context and limits.

### Intent lifecycle, attribution, and recovery

1. Client creates a random intent ID and a local pending UI entry.
2. Edge verifies membership/operation, stamps actor identity, and persists an inbox record before acknowledging acceptance.
3. Host claims work under its current executor fence and rechecks membership revision, resource ownership, and execution policy.
4. Host records the intent in a durable dispatch ledger before applying it. It writes canonical chat/queue state carrying the verified human actor separately from device attribution.
5. Host publishes outcome and shared registry/transcript updates. The client reconciles its pending entry by intent ID.

Deduplicate on `(shareId, intentId)` and require the originally stored actor, membership generation, and payload hash to match on reuse. Re-invitation does not make an old intent executable again; a new action needs a new ID and current generation. Reconnects must not create a second chat/worktree or resend an accepted prompt. Durably acknowledge worktree/chat allocation with recoverable intermediate states so a crash cannot leave an invisible checkout or duplicate it on retry.

Do not promise exactly-once arbitrary shell side effects. If a crash occurs after process launch but before recording its outcome, reconcile using the existing run journal/process identity. Mark unresolved outcomes as interrupted/unknown; never blindly replay a non-idempotent action.

Offline clients may retain drafts. Edge-accepted work may wait while the Mac is offline, but membership is rechecked at dispatch. Revoked or stale-generation pending intents are cancelled/rejected, not replayed automatically after rejoining. Limit accepted queue length, payload size, and active runs per share/member to keep client sessions from consuming the whole host.

## Repository and execution confinement

### IDs, paths, and Git

The owner registers exactly one repository and its permitted execution checkout. Sharing an arbitrary parent folder or home directory is invalid. The broker maps shared worktree IDs to explicit roots and verifies their repository identity at use time, including after restarts or filesystem moves.

Reuse the containment and write-conflict logic in `WorkspaceFiles`, but supply only authorized roots. Validate canonical paths, symlinks, replacement races, and ancestor traversal at the filesystem operation, including file creation. Reject Windows-style traversal payloads even when execution runs on macOS. Never authorize a path just because it begins with a textual repository prefix.

Git common-directory equality is necessary when validating linked worktrees, but insufficient for visibility: only explicitly registered shared worktrees are exposed. Do not expose other linked working directories through repository topology, refs metadata, diffs, error messages, imports, or preview discovery. Reject nested independent repositories/submodules by default until an explicit sharing policy exists for them.

Repository commands that can write refs or execute hooks belong inside the shared execution boundary. Disable inheritance of personal global Git configuration and credential helpers. Existing multi-worktree helpers should operate against the approved collaboration clone.

For v1, the owner selects the base refs and exports a Git bundle into the isolated store; include only those refs and their reachable history. Do not expose a host filesystem remote or perform an unrestricted mirror of the personal repository. Reachable ancestors are still shared: sensitive ancestry requires a sanitized source repository. New shared branches return as an explicit bundle/patch artifact for owner review and validated import into a staging ref, without executing supplied hooks. No personal remote URL with embedded credentials or host credential helper is copied. Owner-configured remote access can follow later with per-repository credentials; merely hiding refs does not remove already imported objects.

### Runtime is a required part of remote coding

Changing `cwd`, filtering files in the UI, or enabling a harness approval setting does not confine code execution. The existing Codex harness maps auto-approve to full access, and other harnesses have different permission behavior. Do not expose those settings as a way to enlarge a shared chat's outer execution boundary.

Add a `SharedExecutionBackend` abstraction used by shared agent sessions, build/test actions, and their child processes. For this web-commerce use case, the recommended first implementation is an app-managed container worker in a VM-backed container runtime on the Mac. This uses the Mac's compute while giving the process a separate filesystem/network boundary. Runtime provisioning is a prerequisite to validate; this repository does not currently establish that such a backend exists. Shared coding must fail closed if this backend is unavailable: there is no fallback to a native host harness, personal RPC service, or host terminal.

Prove one harness end to end before adding the full sharing UI. The worker needs:

- Only its approved collaboration checkout/worktree and runtime-owned caches; no personal home, Zeron data root, personal Git common directory, Docker/control socket, SSH agent, or arbitrary host filesystem mounts.
- A synthetic runtime HOME, allowlisted environment, and runtime-owned provider configuration. Use explicitly configured, scoped and revocable credentials intended for this workload, short-lived where supported; do not copy the owner's existing provider/session directories. Credentials supplied to a coding process must be treated as accessible to that process.
- No access to the owner's engine IPC, host admin services, or other projects over loopback, host gateways, or LAN. Use runtime-enforced network policy/proxying that blocks private/link-local/metadata destinations and rechecks resolved addresses/redirects, including IPv6. Permit configured provider/research/package public egress. This boundary protects the host; it does not promise to prevent members from exporting source intentionally shared with them.
- Process-tree cancellation and runtime teardown that also stops builds, detached children, and dev servers. Resource/concurrency limits belong to this backend.
- No host computer-use, browser profile, plugin credentials, global MCP tools, or arbitrary connected services inherited into shared runs. Explicitly supported research tools can run within the approved environment.

The trusted broker manages identities, invitations, credentials for edge publication, and shared worktree registration outside the worker. The agent cannot obtain a generic broker RPC or mutate membership/publisher credentials. Any worker-to-broker channel must be authenticated and restricted to its runtime/share; a loopback address or `ZERON_CHAT_ID`/`ZERON_DEVICE_ID` environment value is insufficient. A scoped MCP integration, if added, uses that same restricted channel. Provider permissions operate inside this outer boundary and cannot escape it.

To retain a simple user experience, Zeron should provision/manage this runtime from one setup flow rather than requiring the client to administer a second macOS account. A native macOS execution backend can be added later if it provides equivalent tested confinement. If the first runtime proof fails, report that as the blocker; do not ship unrestricted execution under a claim of repo isolation.

## Previews and attachments

### Preview authorization

The current preview connector accepts a service ID against the host catalog, and peers share a connector. Filtering the displayed catalog alone is insufficient: a guest could send another known service ID in an `OPEN`/`WS_OPEN` frame.

Bind each peer/multiplexer to `{shareId, principal, lease}` and authorize every service open against the explicit shared-worktree mapping. Key peer connections by share and peer identity, not just device ID; two shares on the same host must not reuse a broader connection. Reject spoofed host advertisements/signals and cross-share service IDs, even if the service happens to exist locally.

Retain bounded streaming, Host/Origin handling, redirects, and WebSocket/HMR behavior. Scope catalog entries before publication: current preview wire data includes cwd/project/device metadata, so publishing the whole catalog would itself leak information.

The isolated runtime changes discovery: macOS `lsof` cwd discovery cannot identify processes inside a Linux VM/container. Add runtime-owned service registration or discovery inside the worker, with the broker validating its association to the shared worktree. Broker-managed forwarding must resolve only an approved runtime service, not an arbitrary caller-provided TCP target. Reuse the current service-ID transport after this resolution.

The viewing Windows engine retains the local preview proxy and stable URL. The client clicks **Open preview**, and their default browser opens it. Start with the existing direct WebRTC transport; test LAN and a remote network separately. Current code uses STUN/host candidates and has no TURN/byte-relay fallback. Show a useful connection failure if ICE cannot connect; fallback transport is a separate enhancement.

### Attachments, generated files, and sidecars

Scope upload staging, chunk reads, pending references, generated-image imports, and deletion by share and chat/worktree. Resolve opaque IDs on the host. Guest requests must never read a personal upload root through a raw absolute path or the legacy compatibility fallback.

Only publish share-owned attachment references in shared transcripts. Preserve names/media types needed by the UI but avoid personal absolute paths. If tool output blobs are enabled, add share-authorized blob routes/storage keys: only the designated host may PUT canonical tool blobs; members may GET only after the share/chat/part relationship is checked. Do not authorize them solely by knowing a `chatId`/part ID. The retired edge attachment endpoint is not the new attachment transport.

## Revocation, retention, and disconnects

Persist removal/closure and increment the membership/ACL generation before accepting further traffic. Every new protected HTTP request checks current authorization. Push invalidation to shared sockets, host dispatchers, and preview coordinators; cancel the removed member's queued actions and active guest-originated runs, and close their file/preview streams.

For host and P2P access, use short-lived broker-verifiable leases bound to share, member, generation, and peer. Proposed v1 bound: a maximum 30-second lease, renewed approximately every 10 seconds. The Mac must stop affected execution/streams when that lease expires if the authority cannot be reached. Use monotonic local deadlines conservatively; reconnect never revives stale grants. New effects require a current authorization check; long-running work uses the stated bounded lease. The final implementation must test and document its measured bound rather than claiming instantaneous revocation under partition.

Membership revocation cannot retract bytes already delivered or files a collaborator copied. Remove revoked shares from the normal UI, drop pending writes and in-memory handles, and clean app-managed caches on detection. Do not claim forensic erasure or remote deletion on a disconnected client. Revocation should not destroy the team's existing shared history or owner-owned worktrees.

Lease expiry affects guest-originated shared runs/streams and shared access; it must not stop the owner's independent personal/offline work. An owner resuming interrupted shared work does so as a new authorized action, not by silently adopting a revoked guest's queued intent.

## Client UI and local storage

Add **Share repository…** to a Git-backed Space/repository. The owner sees the selected repository, host, runtime readiness, configured agent, invited people, and stop-sharing control. Invitation acceptance adds a **Shared with me** item for the client; they never browse the Mac's home directory to connect.

Within that item, show shared chats, shared worktrees, host online/offline state, and **New chat**, **New worktree**, and **Open preview**. Display that execution runs on the Mac. Pending actions distinguish draft, accepted/queued, running, completed, interrupted, and access removed. Do not expose backend IDs or permission tables in the normal workflow.

Keep personal and shared registries separate internally, then merge authorized projections for display. Keys for chat handles, diffs, queues, file watchers, uploads, link caches, saved tabs, and preview catalogs must include scope. Store shared snapshots/outboxes beneath the signed-in profile and `shareId`. Private sidebar pin/order/read-state preferences remain personal even when they reference shared chats.

Shared selection must survive navigation without being silently converted into a personal `targetDeviceId` call. Authoritative scope must be carried through the composer, question/approval responses, worktree picker, file pane, project actions, overview/topology view, and attachment flows. Deny unknown shared capabilities and show an upgrade message; never fall back to the unrestricted personal protocol.

## Implementation map

Proposed new modules are suggestions; existing entry points are listed to make the work assignable.

| Work package | Existing seams | Suggested additions/change |
| --- | --- | --- |
| Shared contracts | `crates/proto/src/{entities,workspace,agent}.rs`, `crates/rpc/src/lib.rs` | `proto::collaboration`; scope/resource handles, typed intents, actor, capability/version constants. |
| Share authority | `edge/src/{auth,index,env}.ts`, `edge/wrangler.jsonc` | ShareRoom, invitation/directory APIs, membership generation, publisher registration/fence, intent inbox. |
| Read replicas | `edge/src/{chat-room,chat-log,registry-room}.ts`, `crates/sync/src/{registry,chat_client}.rs` | Shared room authorization and reader-only clients; reuse log/checkpoint primitives. |
| Host broker | `crates/rpc/src/device_room.rs`, `crates/engine/src/{lib,rpc,doc_host,workspace_host,chat2_host}.rs` | Scoped service factory, trusted request context, intent consumer, canonical publisher, revocation controller. |
| Resource handling | `crates/engine/src/{repos,workspace_files,chat_workspace_plan,repository_topology,uploads,project_actions}.rs` | Host-private shared-resource registry, scoped helpers, crash reconciliation, no personal import fallback. |
| Execution | `crates/engine/src/{sessions,terminals,run_journal}.rs`, `crates/harness`, `proto::RunRequest` | Isolated worker lifecycle, credential/env policy, harness capability gate, cancellation. |
| Preview | `edge/src/{preview-route,preview-room}.ts`, `crates/preview/src/{service,peer,mux,catalog,signaling}.rs` | Scoped connectors/catalogs, runtime discovery, per-peer leases and revocation. |
| Desktop | `crates/ui/src/{state,shell,composer,pickers,repository_topology}.rs`, `shell/spaces.rs`, `browser/{mod,view}.rs` | Invites/shared selection, scope-aware caches/actions, external Windows preview. |

### Build sequence and agent assignments

1. **Coordinator: freeze contracts and fixtures.** Define the scope/actor/resource schemas, permission table, protocol capability, lease behavior, and request/outcome states. Establish a fixture with owner A, invited guest B, unrelated user C, two repositories, a personal chat, a private worktree, and a shared chat. Gate: all later work targets the same contracts.
2. **Execution agent: prove isolated remote development.** Run one actual supported harness, a build/test, and a dev preview inside the proposed Mac-hosted runtime. Prove denial of personal files, host IPC/network services, and provider-home inheritance. Gate: the runtime can do useful ecommerce work without host-wide access. Run this in parallel with edge work after contracts are fixed.
3. **Edge agent: authority and shared read paths.** Implement invitations, member discovery, ACL revisions, publisher fencing, typed durable inbox, share-only index, and host-only shared chat writes. Add DO migration/bindings to production and workerd test configurations. Gate: A/B/C tests pass on every HTTP/WS path, including rejected opaque writes and stale leases.
4. **Engine agent: broker and resource integration.** Bind trusted connection context, consume intents idempotently, manage shared worktrees and uploads, connect the runtime, and publish shared state. Gate: B can request useful work while all personal RPC and resource targets fail closed.
5. **Preview agent: scoped preview path.** Add runtime discovery, scope-bound catalogs/connectors, leases, and revocation. Gate: shared HMR works and a guessed private service ID fails on both HTTP and WebSocket opens.
6. **UI agent: complete Windows/Mac workflow.** Implement invitation/share surfaces and scope-aware chat/worktree/file/preview selection using the established contracts. Gate: a Windows user can complete the workflow without knowing a host path or port.
7. **Integration/review agent: privacy, lifecycle, compatibility.** Run the full matrix below, repair shared-path gaps, verify personal behavior, and document actual Windows/Mac evidence. Release only after the execution and authorization gates pass.

Use isolated implementation worktrees and explicit file ownership for these packages. `rpc.rs`, `lib.rs`, `workspace_host.rs`, and `shell.rs` are coordination hotspots; do not have multiple agents edit them independently. Integrate the edge and execution proofs before polishing the full UI. This handoff authorizes no deployment or implementation by itself.

## Acceptance tests

Test with genuinely distinct authenticated user identities, not two devices logged into A. Include a guest in a different organization to exercise the intended invitation model. Dev-mode tokens can model actors for local tests; final acceptance also uses the real sign-in flow.

| Scenario | Required result |
| --- | --- |
| Windows B accepts A's invite | Exactly the invited share appears; no owner's personal registry/device rows, other repo names/paths, personal chats, or private working files are downloaded. B retains access to B's own personal profile. |
| No invite / expired / wrong-recipient / reused invite | C and the wrong recipient cannot join or claim ownership. Concurrent redemption is atomic. |
| Known private IDs | Requests for A's private chat/checkpoint/tail/diff/blob/upload/worktree/preview remain unauthorized, even when B knows the ID. |
| Forged actor/device/scope/host | The broker uses authenticated context; payloads cannot change actor, membership, publisher, share, cwd, or executor. |
| Personal forwarding bypass | B cannot supply `targetDeviceId` to acquire A's unrestricted `EngineRpc`, invoke generic `RelayCommand`, or use an owner-only device connection. |
| Opaque write attempts | B cannot push CRDT rows/checkpoints, registry mutations, reset/reseed operations, or host sidecars via HTTP or WebSocket. |
| File/Git escape | Traversal, symlink replacement, nested repos, arbitrary worktree path, raw personal `.git` path, and Windows path syntax cannot reach an unauthorized root. |
| Agent escape | A guest prompt, project script, hook, or child process cannot read personal canary files/provider homes, contact personal host IPC or other repo services, use SSH agents, or reach runtime control sockets. Test direct RPC and `zeron mcp` with omitted/spoofed origin environment values. |
| Two independent chats | Agents/builds run in their allocated shared worktrees on the Mac runtime; files, server ports, and build state stay correctly associated. |
| Attribution and retry | Messages show the authenticated actor; duplicate intent delivery creates one logical action; mismatched payload reuse fails. |
| Crash/reconnect | Worktree creation and queue acceptance reconcile without duplication; unknown external effects are not replayed automatically. |
| Authorization removal | New access is rejected; existing streams/runs stop within the specified lease bound, including with signaling/authority partitioned. |
| Offline backlog | Revoked queued commands never execute when the host wakes. A stale client cannot reseed deleted/revoked state. |
| Preview | Windows default browser loads the shared server, redirects/cookies work, HMR works, another service ID fails, network failure is bounded and visible. |
| Attachments | B's reference upload reaches the correct shared chat; private/generated files and legacy roots are inaccessible by guessed paths/IDs. |
| Runtime/provider policy | B cannot change to full host access, select an unapproved credential/tool, or approve a request outside the runtime policy. |
| Mixed versions and scopes | Personal sync still works; old clients cannot join shared resources; moving between personal/shared tabs cannot reuse broader cached handles. |

Exercise known owner-resource IDs against **all** private route families: legacy `/session`, `/tail`, `/stats`, `/diff`, `/snapshot`, `/append`; `/workspace` including `append/reset-log`; `/registry` including `push/reset`; `/chat2` including HTTP rows/checkpoints/sidecars/reset; `/device` including `ws/status/sidecar/nudge`; and `/blob`. These requests must never reveal or mutate A's resources using B's identity. Some existing routes resolve to B's own user namespace rather than returning an error; preserve that behavior while proving that no A data crosses the boundary.

Extend these existing suites where the boundary fits; add dedicated shared-collaboration tests for new behavior:

- Edge: `edge/test/workerd/{chat-checkpoint,chat-log,preview}.workerd.test.ts`, registry tests, and new share-authority/intent tests. Test HTTP fallback as well as WS.
- Engine/RPC: `crates/engine/tests/{device_routing,workspace_sync,workspace_files,local_profiles,message_queue,queued_attachments,previews,chat_workspace_plan_rpc}.rs` and `crates/rpc/tests/device_room.rs`.
- Sync: `crates/sync/tests/{registry_client,registry_edge}.rs`; reader mode, reconnect, reset, and stale generation cases.
- Preview: `crates/preview/tests/{coordinator,transport,proxy,leak}.rs`; verify access at stream open, not merely catalog filtering.
- Native acceptance: the actual Windows app and Mac runtime; the current Windows build/synthetic harness tests do not prove this cross-user workflow.

Suggested validation commands from the `zeron/` root, after implementation:

```sh
cargo fmt --all -- --check
npm --prefix edge run typecheck
npm --prefix edge test
cargo test --locked -p zeron-rpc -p zeron-sync -p zeron-preview
cargo test --locked -p zeron-engine --lib
cargo test --locked -p zeron-engine --test device_routing --test workspace_sync --test workspace_files --test local_profiles --test message_queue --test queued_attachments --test previews --test chat_workspace_plan_rpc
cargo test --locked -p zeron-ui --lib
cargo check --locked -p zeron
```

Also run newly added shared/runtime tests explicitly and the Windows workflow/native acceptance. Some live integration tests require a local Worker or explicit environment configuration; report skips/prerequisites accurately. Do not treat an ignored coordinator test as a pass.

## Rollout and completion criteria

Deploy additive edge classes/routes first behind a feature gate, preserving the Worker name and existing room/storage identities. Append migrations rather than editing already deployed migrations. Mirror new bindings/classes in workerd configurations. Upgrade the Mac host and Windows client, negotiate `repo-sharing-v1` (proposed capability), and enable a test share only after the runtime gate passes.

No automatic migration of personal chats or existing worktrees is required for v1. Store shared data under distinct namespaces so rollback can disable invitations/execution, close active leases, and preserve shared data for recovery without corrupting personal stores. Never fall back from a failed shared connection to a personal connection.

Done means two separate users can collaborate from Windows and Mac, a running ecommerce preview follows the correct worktree, private resources remain inaccessible through both UI and direct protocol attempts, and revocation/recovery behave as specified. Record tested OS/app versions, selected runtime/harness, known network limitations, and measured revocation behavior in the implementation PR.

## Evidence and limitations of this handoff

Three delegated read-only audits covered sync/auth, host execution/resources, and client/preview integration. This document combines their findings with direct source inspection. No runtime, Windows, edge deployment, or security acceptance test was executed as part of preparing the handoff. The document is an implementation specification, not a claim that repository sharing or execution isolation already works.
