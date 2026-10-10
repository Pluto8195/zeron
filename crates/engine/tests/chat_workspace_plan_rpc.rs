//! `PLAN_CHAT_WORKSPACE`/`CREATE_CHAT_WORKTREE` over the RPC dispatch
//! (memory transport): wire shapes, the heuristic fallback end-to-end (no
//! `TYPESAFE_API_KEY` on the test machine), and — the part
//! `crates/engine/src/chat_workspace_plan.rs`'s own unit tests can't reach,
//! since they call `create_chat_worktree` directly rather than through
//! `EngineRpc` — that a real `CreateChatWorktree` call stamps the chat row's
//! `cwd` so its next dispatch runs in the new worktree automatically.

use std::path::Path;

use zeron_engine::{EngineCore, HarnessRegistry};
use zeron_proto::HarnessId;

const CHAT: &str = "chat-smart-worktree";

fn git(cwd: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn init_repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "t@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    std::fs::write(dir.join("README.md"), "hello\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn plan_chat_workspace_falls_back_to_heuristic_without_a_typesafe_key() {
    // Ensure a stray key from the outer shell can't turn this into a live
    // network call — the point of this test is the heuristic path.
    unsafe { std::env::remove_var("TYPESAFE_API_KEY") };

    let tmp = tempfile::tempdir().unwrap();
    let core = EngineCore::assemble(
        &tmp.path().join("data"),
        std::sync::Arc::new(HarnessRegistry::new()),
        HarnessId::Mock,
        None,
    )
    .expect("engine core assembles");
    let client = zeron_rpc::memory_client(core.rpc_service());

    let reply = client
        .call(
            zeron_rpc::methods::PLAN_CHAT_WORKSPACE,
            serde_json::json!({"message": "please implement retry logic", "cwd": "/tmp"}),
        )
        .await
        .expect("PlanChatWorkspace call");
    assert_eq!(reply["needsWorktree"], serde_json::json!(true));
    assert_eq!(reply["source"], serde_json::json!("heuristic"));
    assert_eq!(reply["probability"], serde_json::Value::Null);

    let reply = client
        .call(
            zeron_rpc::methods::PLAN_CHAT_WORKSPACE,
            serde_json::json!({"message": "why is this endpoint slow?", "cwd": "/tmp"}),
        )
        .await
        .expect("PlanChatWorkspace call");
    assert_eq!(reply["needsWorktree"], serde_json::json!(false));
    assert_eq!(reply["source"], serde_json::json!("heuristic"));

    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn create_chat_worktree_stamps_the_chat_rows_cwd() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp_path = tmp.path().canonicalize().unwrap();
    let repo_dir = tmp_path.join("repo");
    init_repo(&repo_dir);

    let core = EngineCore::assemble(
        &tmp_path.join("data"),
        std::sync::Arc::new(HarnessRegistry::new()),
        HarnessId::Mock,
        None,
    )
    .expect("engine core assembles");
    let client = zeron_rpc::memory_client(core.rpc_service());

    // The chat row must already exist for the cwd stamp to land — same
    // precondition the composer's ordinary `createChat` flow satisfies
    // before a chat's first send.
    client
        .call(
            zeron_rpc::methods::MUTATE,
            serde_json::json!({
                "op": "createChat",
                "chatId": CHAT,
                "deviceId": core.device_id,
            }),
        )
        .await
        .expect("createChat");

    let reply = client
        .call(
            zeron_rpc::methods::CREATE_CHAT_WORKTREE,
            serde_json::json!({
                "chatId": CHAT,
                "repoPath": repo_dir.to_string_lossy(),
                // Contains a ticket id ("eng-42") — the slug prefers it over
                // sanitizing the full free-text name (see
                // `chat_workspace_plan::compute_slug`).
                "name": "eng-42 do a thing",
            }),
        )
        .await
        .expect("CreateChatWorktree call");

    let worktree_path = reply["worktreePath"].as_str().unwrap().to_string();
    assert_eq!(reply["branch"], serde_json::json!("eng-42"));
    let expected = repo_dir.join(".worktrees").join("workspace").join("eng-42");
    assert_eq!(worktree_path, expected.to_string_lossy());
    assert!(expected.join(".git").exists());
    assert!(expected.join(".workspace-root").exists());

    // The RPC's own effect on the chat row: cwd now points at the worktree,
    // so the next Run this chat dispatches starts there automatically.
    let chat = core
        .workspace
        .chat(CHAT)
        .expect("read chat row")
        .expect("chat row exists");
    assert_eq!(chat.cwd.as_deref(), Some(worktree_path.as_str()));

    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn create_chat_worktree_without_an_existing_chat_row_still_creates_the_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp_path = tmp.path().canonicalize().unwrap();
    let repo_dir = tmp_path.join("repo");
    init_repo(&repo_dir);

    let core = EngineCore::assemble(
        &tmp_path.join("data"),
        std::sync::Arc::new(HarnessRegistry::new()),
        HarnessId::Mock,
        None,
    )
    .expect("engine core assembles");
    let client = zeron_rpc::memory_client(core.rpc_service());

    // No createChat call first — the RPC must not fail just because the
    // stamp is a no-op (`Ok(false)`); the worktree itself is still real.
    let reply = client
        .call(
            zeron_rpc::methods::CREATE_CHAT_WORKTREE,
            serde_json::json!({
                "chatId": "chat-with-no-row-yet",
                "repoPath": repo_dir.to_string_lossy(),
                "name": "no-row-yet",
            }),
        )
        .await
        .expect("CreateChatWorktree call succeeds even with no chat row yet");
    assert!(
        Path::new(reply["worktreePath"].as_str().unwrap())
            .join(".git")
            .exists()
    );

    core.shutdown().await;
}

// ── PLAN_CHAT_CLOSEOUT / CLOSE_CHAT_WORKTREE ─────────────────────────────────

async fn assemble(tmp_path: &Path) -> (EngineCore, zeron_rpc::RpcClient) {
    let core = EngineCore::assemble(
        &tmp_path.join("data"),
        std::sync::Arc::new(HarnessRegistry::new()),
        HarnessId::Mock,
        None,
    )
    .expect("engine core assembles");
    let client = zeron_rpc::memory_client(core.rpc_service());
    (core, client)
}

/// Repo + chat row + a real chat worktree created through the RPC, exactly as
/// the composer does it.
async fn repo_chat_and_worktree(
    tmp_path: &Path,
    core: &EngineCore,
    client: &zeron_rpc::RpcClient,
    name: &str,
) -> (std::path::PathBuf, String) {
    let repo_dir = tmp_path.join("repo");
    init_repo(&repo_dir);
    client
        .call(
            zeron_rpc::methods::MUTATE,
            serde_json::json!({"op": "createChat", "chatId": CHAT, "deviceId": core.device_id}),
        )
        .await
        .expect("createChat");
    let reply = client
        .call(
            zeron_rpc::methods::CREATE_CHAT_WORKTREE,
            serde_json::json!({"chatId": CHAT, "repoPath": repo_dir.to_string_lossy(), "name": name}),
        )
        .await
        .expect("CreateChatWorktree");
    (
        repo_dir,
        reply["worktreePath"].as_str().unwrap().to_string(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn plan_chat_closeout_reply_has_exactly_the_pinned_wire_fields() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp_path = tmp.path().canonicalize().unwrap();
    let (core, client) = assemble(&tmp_path).await;
    let (_repo, wt) = repo_chat_and_worktree(&tmp_path, &core, &client, "eng-55 closeout").await;
    std::fs::write(Path::new(&wt).join("scratch.txt"), "s\n").unwrap();

    let reply = client
        .call(
            zeron_rpc::methods::PLAN_CHAT_CLOSEOUT,
            serde_json::json!({"chatId": CHAT, "cwd": wt}),
        )
        .await
        .expect("PlanChatCloseout call");
    let mut keys: Vec<&str> = reply
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "branch",
            "chatLive",
            "defaultBranch",
            "dirty",
            "dirtyFiles",
            "dirtyInspectionError",
            "isWorktree",
            "mergeInspectionError",
            "sharedChats",
            "unmergedCommitDetails",
            "unmergedCommits",
            "worktreePath"
        ]
    );
    assert_eq!(reply["isWorktree"], serde_json::json!(true));
    assert_eq!(reply["worktreePath"], serde_json::json!(wt));
    assert_eq!(reply["branch"], serde_json::json!("eng-55"));
    assert_eq!(reply["chatLive"], serde_json::json!(false));
    assert_eq!(reply["dirty"], serde_json::json!(true));
    assert_eq!(reply["dirtyFiles"], serde_json::json!(["scratch.txt"]));
    assert_eq!(reply["dirtyInspectionError"], serde_json::Value::Null);
    assert_eq!(reply["unmergedCommits"], serde_json::json!(0));
    assert_eq!(reply["unmergedCommitDetails"], serde_json::json!([]));
    assert_eq!(reply["mergeInspectionError"], serde_json::Value::Null);
    assert_eq!(reply["defaultBranch"], serde_json::json!("main"));
    assert_eq!(reply["sharedChats"], serde_json::json!([]));

    // The RPC is chat-bound: it must not inspect an arbitrary path supplied
    // for a chat whose stored cwd points at a different worktree.
    let error = client
        .call(
            zeron_rpc::methods::PLAN_CHAT_CLOSEOUT,
            serde_json::json!({"chatId": CHAT, "cwd": tmp_path.to_string_lossy()}),
        )
        .await
        .expect_err("mismatched close-out cwd must be rejected");
    assert!(error.to_string().contains("does not match"), "{error}");

    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn close_chat_worktree_refuses_dirty_then_force_removes_and_archives() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp_path = tmp.path().canonicalize().unwrap();
    let (core, client) = assemble(&tmp_path).await;
    let (repo, wt) = repo_chat_and_worktree(&tmp_path, &core, &client, "dirty-rpc").await;
    std::fs::write(Path::new(&wt).join("scratch.txt"), "s\n").unwrap();

    let err = client
        .call(
            zeron_rpc::methods::CLOSE_CHAT_WORKTREE,
            serde_json::json!({"chatId": CHAT, "cwd": wt, "force": false}),
        )
        .await
        .expect_err("dirty worktree refused without force");
    assert!(err.to_string().contains("1 uncommitted file"), "{err}");
    assert!(Path::new(&wt).exists());
    assert!(!core.workspace.chat(CHAT).unwrap().unwrap().archived);

    let reply = client
        .call(
            zeron_rpc::methods::CLOSE_CHAT_WORKTREE,
            serde_json::json!({"chatId": CHAT, "cwd": wt, "force": true}),
        )
        .await
        .expect("forced close");
    assert_eq!(
        reply,
        serde_json::json!({"removed": true, "branchDeleted": true, "archived": true})
    );
    assert!(!Path::new(&wt).exists());
    assert!(core.workspace.chat(CHAT).unwrap().unwrap().archived);
    let branches = std::process::Command::new("git")
        .args(["branch", "--list", "dirty-rpc"])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&branches.stdout).trim().is_empty());

    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn close_chat_worktree_clean_merged_happy_path_archives_the_chat() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp_path = tmp.path().canonicalize().unwrap();
    let (core, client) = assemble(&tmp_path).await;
    let (_repo, wt) = repo_chat_and_worktree(&tmp_path, &core, &client, "clean-rpc").await;

    let reply = client
        .call(
            zeron_rpc::methods::CLOSE_CHAT_WORKTREE,
            serde_json::json!({"chatId": CHAT, "cwd": wt, "force": false}),
        )
        .await
        .expect("clean close");
    assert_eq!(
        reply,
        serde_json::json!({"removed": true, "branchDeleted": true, "archived": true})
    );
    assert!(!Path::new(&wt).exists());

    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn repo_map_can_close_an_unmatched_registered_worktree_without_a_marker() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp_path = tmp.path().canonicalize().unwrap();
    let repo = tmp_path.join("repo");
    init_repo(&repo);
    let wt = tmp_path.join("repo-map-worktree");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "repo-map-worktree",
            wt.to_str().unwrap(),
        ],
    );
    assert!(!wt.join(".workspace-root").exists());

    let (core, client) = assemble(&tmp_path).await;
    let plan = client
        .call(
            zeron_rpc::methods::PLAN_CHAT_CLOSEOUT,
            serde_json::json!({"cwd": wt.to_string_lossy()}),
        )
        .await
        .expect("unmatched worktree plan");
    assert_eq!(plan["isWorktree"], serde_json::json!(true));
    assert_eq!(plan["dirty"], serde_json::json!(false));

    // An unmatched Repo Map action must not delete a checkout owned by any
    // open local chat, even when that chat is idle.
    client
        .call(
            zeron_rpc::methods::MUTATE,
            serde_json::json!({
                "op": "createChat",
                "chatId": CHAT,
                "deviceId": core.device_id,
                "cwd": wt.to_string_lossy(),
            }),
        )
        .await
        .expect("create owner chat");
    client
        .call(
            zeron_rpc::methods::MUTATE,
            serde_json::json!({"op": "renameChat", "chatId": CHAT, "title": "Owner chat"}),
        )
        .await
        .expect("rename owner chat");
    let plan = client
        .call(
            zeron_rpc::methods::PLAN_CHAT_CLOSEOUT,
            serde_json::json!({"cwd": wt.to_string_lossy()}),
        )
        .await
        .expect("planning exposes open chat ownership");
    assert_eq!(
        plan["sharedChats"],
        serde_json::json!([{"id": CHAT, "title": "Owner chat"}])
    );

    let error = client
        .call(
            zeron_rpc::methods::CLOSE_CHAT_WORKTREE,
            serde_json::json!({"cwd": wt.to_string_lossy(), "force": true}),
        )
        .await
        .expect_err("open chat ownership blocks close-out");
    assert!(error.to_string().contains("used by 1 open chat"), "{error}");

    client
        .call(
            zeron_rpc::methods::MUTATE,
            serde_json::json!({"op": "setChatArchived", "chatId": CHAT, "archived": true}),
        )
        .await
        .expect("archive owner chat");
    let plan = client
        .call(
            zeron_rpc::methods::PLAN_CHAT_CLOSEOUT,
            serde_json::json!({"cwd": wt.to_string_lossy()}),
        )
        .await
        .expect("archived chat no longer blocks planning");
    assert_eq!(plan["sharedChats"], serde_json::json!([]));
    let reply = client
        .call(
            zeron_rpc::methods::CLOSE_CHAT_WORKTREE,
            serde_json::json!({"cwd": wt.to_string_lossy(), "force": false}),
        )
        .await
        .expect("close unmatched worktree");
    assert_eq!(
        reply,
        serde_json::json!({"removed": true, "branchDeleted": true, "archived": false})
    );
    assert!(!wt.exists());

    core.shutdown().await;
}

/// Live-chat refusal through the real liveness mechanism: a process whose
/// argv[0] basename is `claude` and whose cwd is the worktree is what the
/// engine's process scan (same signal as the auto-adopt sweep) treats as a
/// live session — no harness run needed.
#[tokio::test(flavor = "multi_thread")]
async fn close_chat_worktree_refuses_while_a_claude_process_sits_in_the_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp_path = tmp.path().canonicalize().unwrap();
    let (core, client) = assemble(&tmp_path).await;
    let (_repo, wt) = repo_chat_and_worktree(&tmp_path, &core, &client, "live-rpc").await;

    let bin_dir = tmp_path.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let fake_claude = bin_dir.join("claude");
    std::os::unix::fs::symlink("/bin/sleep", &fake_claude).unwrap();
    let mut child = std::process::Command::new(&fake_claude)
        .arg("60")
        .current_dir(&wt)
        .spawn()
        .expect("spawn fake claude");
    // Let ps/lsof observe it.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let plan = client
        .call(
            zeron_rpc::methods::PLAN_CHAT_CLOSEOUT,
            serde_json::json!({"chatId": CHAT, "cwd": wt}),
        )
        .await
        .expect("plan");
    let err = client
        .call(
            zeron_rpc::methods::CLOSE_CHAT_WORKTREE,
            serde_json::json!({"chatId": CHAT, "cwd": wt, "force": true}),
        )
        .await;

    let _ = child.kill();
    let _ = child.wait();

    assert_eq!(plan["chatLive"], serde_json::json!(true));
    let err = err.expect_err("live chat refused even with force");
    assert!(err.to_string().contains("live"), "{err}");
    assert!(Path::new(&wt).exists());
    assert!(!core.workspace.chat(CHAT).unwrap().unwrap().archived);

    core.shutdown().await;
}
