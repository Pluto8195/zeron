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
        Path::new(reply["worktreePath"].as_str().unwrap()).join(".git").exists()
    );

    core.shutdown().await;
}
