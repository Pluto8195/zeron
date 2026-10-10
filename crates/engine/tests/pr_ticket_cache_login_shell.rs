//! Regression test for GUI launches whose direct PATH cannot see `gh`.
//!
//! Keep this as a single-test binary: it mutates process environment and
//! warms the process-global login-shell PATH snapshot.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use zeron_engine::pr_ticket_cache::PrTicketCache;

fn write_executable(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[tokio::test]
async fn pr_search_uses_login_shell_path_and_disables_prompts() {
    let dir = tempfile::tempdir().unwrap();
    let hostile_checkout = dir.path().join("hostile-checkout");
    std::fs::create_dir(&hostile_checkout).unwrap();
    let hostile_marker = dir.path().join("hostile-gh-ran");
    write_executable(
        &hostile_checkout.join("gh"),
        &format!(
            "#!/bin/sh\nprintf hostile > '{}'\nexit 2\n",
            hostile_marker.display()
        ),
    );
    let hostile_git_marker = dir.path().join("hostile-git-ran");
    write_executable(
        &hostile_checkout.join("git"),
        &format!(
            "#!/bin/sh\nprintf hostile > '{}'\nexit 2\n",
            hostile_git_marker.display()
        ),
    );

    let shell_bin = dir.path().join("shell-bin");
    std::fs::create_dir(&shell_bin).unwrap();
    let trusted_git_marker = dir.path().join("trusted-git-ran");
    write_executable(
        &shell_bin.join("git"),
        &format!(
            "#!/bin/sh\nprintf trusted > '{}'\nprintf '%s\\n' 'git version test'\n",
            trusted_git_marker.display()
        ),
    );
    write_executable(
        &shell_bin.join("gh"),
        r##"#!/bin/sh
if [ "$GH_PROMPT_DISABLED" != "1" ]; then
  echo "interactive auth was not disabled" >&2
  exit 2
fi
git --version >/dev/null || exit 4
case "$1 $2" in
  "search prs")
    printf '%s\n' '[{"number":42,"title":"Login shell PR","url":"https://github.com/acme/widgets/pull/42","repository":{"name":"widgets","nameWithOwner":"acme/widgets"},"isDraft":false,"updatedAt":"2026-10-06T12:00:00Z"}]'
    ;;
  "pr view")
    printf '%s\n' '{"number":42,"title":"Login shell PR","url":"https://github.com/acme/widgets/pull/42","state":"OPEN","isDraft":false,"reviewDecision":null,"statusCheckRollup":[],"reviews":[],"headRefName":"feature/login-shell","mergeable":"MERGEABLE","reviewRequests":[],"author":{"login":"octocat"}}'
    ;;
  *) exit 3 ;;
esac
"##,
    );

    let fake_shell = dir.path().join("fake-shell");
    write_executable(
        &fake_shell,
        &format!(
            "#!/bin/sh\nPATH=\"{}:/usr/bin:/bin\"; export PATH\n\
             while [ \"$#\" -gt 0 ]; do\n\
               if [ \"$1\" = \"-c\" ]; then shift; exec /bin/sh -c \"$1\"; fi\n\
               shift\n\
             done\nexit 1\n",
            shell_bin.display()
        ),
    );

    // Simulate a GUI/service launch with a hostile relative PATH. Once a gh
    // command sets `hostile_checkout` as cwd, resolving the bare name through
    // `.` would execute the checkout's file. The login shell exposes the only
    // trusted absolute candidate. This binary has one test, so environment
    // mutation cannot race another test.
    unsafe {
        std::env::set_var("SHELL", &fake_shell);
        std::env::set_var("HOME", dir.path());
        std::env::set_var("PATH", ".");
        std::env::remove_var("ZERON_NO_LOGIN_SHELL");
    }

    let cache = PrTicketCache::new();
    cache.status_for(
        hostile_checkout.to_str(),
        Some("feature/hostile"),
        None,
        None,
    );
    let sweep = cache.spawn_sweep_loop();
    assert!(
        cache
            .wait_for_my_open_prs_ready(Duration::from_secs(5))
            .await,
        "PR search should finish through login-shell-only gh"
    );
    let items = cache.my_open_prs();
    sweep.abort();

    assert!(!hostile_marker.exists(), "checkout-local gh must never run");
    assert!(
        !hostile_git_marker.exists(),
        "checkout-local git must never run from the child PATH"
    );
    assert!(
        trusted_git_marker.exists(),
        "trusted absolute-PATH git should run"
    );
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].summary.number, 42);
    assert_eq!(items[0].summary.repo.as_deref(), Some("acme/widgets"));
    assert_eq!(items[0].summary.title.as_deref(), Some("Login shell PR"));
}
