//! Liveness check for external-session import (ticket 001, piece 4):
//! before importing a Claude Code session Zeron didn't launch itself, warn
//! (never hard-block — Mikey's call) if the source session looks like it
//! might still be actively running elsewhere. Importing a session that's
//! concurrently being written to by another `claude` process risks two
//! processes racing on the same on-disk transcript.
//!
//! Two independent signals, either is enough to warn:
//! - the transcript `.jsonl` was modified recently (still being appended to)
//! - a `claude` process is currently running on this machine that either
//!   mentions the target session id in its command line, or whose real
//!   working directory (not just its args) matches the target cwd
//!
//! Best-effort by design: a signal this can't determine (permission denied,
//! `ps`/`lsof` unavailable, clock skew) reads as "not concerning" rather
//! than an error — a false negative here just means no warning shown, which
//! is no worse than not having this check at all. A false *positive* (an
//! unnecessary warning) is the safe direction to fail toward instead.
//!
//! No process-enumeration crate (e.g. `sysinfo`) is a direct workspace
//! dependency today — it's only pulled in transitively by an unrelated crate
//! (`zed-scap`), so using it here would mean adding a new direct dependency
//! (`Cargo.toml` + `Cargo.lock` churn, a bigger/shared-blast-radius change
//! than this module warrants). Shelling out to `ps`/`lsof` needs nothing
//! beyond `std` and keeps this entirely self-contained.
//!
//! Why cwd needs `lsof`, not just `ps` args: a plain `claude` invocation
//! doesn't put its cwd in argv — cwd is a process-level OS attribute set by
//! whatever shell/tmux pane launched it, not a command-line flag. Matching
//! cwd by searching `ps`'s args text for the cwd string would almost never
//! actually fire. `lsof -a -d cwd -p <pid>` reads the real cwd off the
//! process's file descriptor table instead.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

/// A source session's on-disk transcript was modified more recently than
/// this is treated as "possibly still being written to."
const RECENT_MTIME_THRESHOLD: Duration = Duration::from_secs(5 * 60);

/// The result of checking whether an external session looks like it might
/// still be live. Never a hard block — the caller decides whether/how to
/// surface a warning and always allows proceeding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LivenessCheck {
    /// The transcript file's mtime is within [`RECENT_MTIME_THRESHOLD`].
    pub recently_modified: bool,
    /// A running `claude` process's command line mentions this session id,
    /// or its real cwd (via `lsof`) matches the target cwd.
    pub live_process_match: bool,
}

impl LivenessCheck {
    pub fn is_concerning(&self) -> bool {
        self.recently_modified || self.live_process_match
    }
}

/// Check whether `session_id` (running under `cwd`, with its transcript at
/// `transcript_path`) looks like it might still be live. Blocking (fs +
/// process enumeration); run off the async path.
pub fn check_liveness(transcript_path: &Path, session_id: &str, cwd: &str) -> LivenessCheck {
    LivenessCheck {
        recently_modified: mtime_is_recent(transcript_path, SystemTime::now()),
        live_process_match: claude_process_matches("claude", session_id, cwd),
    }
}

/// Process-only half of [`check_liveness`]: is a running `claude` process
/// tied to `session_id` (in its args) or to `cwd` (its real working
/// directory)? Skips the transcript-mtime signal, which exists to warn before
/// importing a session — it would read a chat whose turn just finished as
/// "live" for [`RECENT_MTIME_THRESHOLD`], wrongly blocking a close-out.
/// Blocking (`ps`/`lsof`).
pub fn live_process_matches(session_id: &str, cwd: &str) -> bool {
    claude_process_matches("claude", session_id, cwd)
}

fn mtime_is_recent(path: &Path, now: SystemTime) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false; // can't stat it — not concerning, just undetermined
    };
    let Ok(modified) = metadata.modified() else {
        return false; // platform doesn't support mtime — undetermined
    };
    is_recent(modified, now, RECENT_MTIME_THRESHOLD)
}

/// Pure comparison, split out from the fs call so it's directly unit
/// testable without touching disk. A `modified` time *after* `now` (clock
/// skew, or a filesystem with coarse/odd mtime semantics) is treated as
/// recent — the conservative direction for a best-effort warning.
fn is_recent(modified: SystemTime, now: SystemTime, threshold: Duration) -> bool {
    match now.duration_since(modified) {
        Ok(age) => age < threshold,
        Err(_) => true,
    }
}

/// One `ps -eo pid=,args=` row: a process id and its full command line.
struct PsRow {
    pid: u32,
    args: String,
}

fn list_processes() -> Vec<PsRow> {
    let Ok(output) = Command::new("ps").args(["-eo", "pid=,args="]).output() else {
        return Vec::new(); // `ps` unavailable (e.g. non-Unix) — undetermined
    };
    if !output.status.success() {
        return Vec::new();
    }
    let Ok(text) = String::from_utf8(output.stdout) else {
        return Vec::new();
    };
    parse_ps_rows(&text)
}

fn parse_ps_rows(ps_output: &str) -> Vec<PsRow> {
    ps_output
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim_start();
            let (pid_str, rest) = trimmed.split_once(char::is_whitespace)?;
            let pid = pid_str.parse().ok()?;
            Some(PsRow {
                pid,
                args: rest.trim_start().to_string(),
            })
        })
        .collect()
}

/// True when a process named `binary_name` (matched on argv[0]'s basename)
/// either has `session_id` in its command line, or has a real cwd (via
/// `lsof`) equal to `cwd`.
fn claude_process_matches(binary_name: &str, session_id: &str, cwd: &str) -> bool {
    processes_matching(&list_processes(), binary_name, session_id, cwd, real_cwd_of)
}

/// Split out from [`claude_process_matches`] so the matching logic is
/// testable against a synthetic process list and a fake cwd lookup, without
/// depending on real `ps`/`lsof` output.
fn processes_matching(
    rows: &[PsRow],
    binary_name: &str,
    session_id: &str,
    cwd: &str,
    lookup_cwd: impl Fn(u32) -> Option<String>,
) -> bool {
    rows.iter().any(|row| {
        let Some(first_token) = row.args.split_whitespace().next() else {
            return false;
        };
        let base = first_token.rsplit('/').next().unwrap_or(first_token);
        if base != binary_name {
            return false;
        }
        if !session_id.is_empty() && row.args.contains(session_id) {
            return true;
        }
        !cwd.is_empty() && lookup_cwd(row.pid).as_deref() == Some(cwd)
    })
}

/// The real working directory of `pid`, via `lsof -a -d cwd -p <pid>`.
/// `None` on any failure (process exited, permission denied, `lsof` absent,
/// unexpected output shape) — undetermined, not an error.
fn real_cwd_of(pid: u32) -> Option<String> {
    let output = Command::new("lsof")
        .args(["-a", "-d", "cwd", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    // Header line, then one row: "<COMMAND> <PID> <USER> <FD> <TYPE> <DEVICE>
    // <SIZE/OFF> <NODE> <NAME>" — NAME (the cwd path) is the last
    // whitespace-separated field, but paths can contain spaces, so take
    // everything from the 9th field onward rather than just the last token.
    let row = text.lines().nth(1)?;
    let mut fields = row.split_whitespace();
    for _ in 0..8 {
        fields.next()?;
    }
    let rest: Vec<&str> = fields.collect();
    if rest.is_empty() {
        return None;
    }
    Some(rest.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_mtime_is_not_recent() {
        let now = SystemTime::now();
        let old = now - Duration::from_secs(3600);
        assert!(!is_recent(old, now, RECENT_MTIME_THRESHOLD));
    }

    #[test]
    fn just_past_threshold_is_not_recent() {
        let now = SystemTime::now();
        let just_over = now - (RECENT_MTIME_THRESHOLD + Duration::from_secs(1));
        assert!(!is_recent(just_over, now, RECENT_MTIME_THRESHOLD));
    }

    #[test]
    fn fresh_mtime_is_recent() {
        let now = SystemTime::now();
        let fresh = now - Duration::from_secs(10);
        assert!(is_recent(fresh, now, RECENT_MTIME_THRESHOLD));
    }

    #[test]
    fn future_mtime_reads_as_recent_conservatively() {
        let now = SystemTime::now();
        let future = now + Duration::from_secs(60);
        assert!(is_recent(future, now, RECENT_MTIME_THRESHOLD));
    }

    #[test]
    fn a_freshly_created_file_is_recently_modified() {
        let dir = std::env::temp_dir().join(format!("zeron-liveness-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("fresh.jsonl");
        std::fs::write(&path, b"{}").expect("write fresh file");

        assert!(mtime_is_recent(&path, SystemTime::now()));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_file_is_not_recently_modified() {
        let missing = Path::new("/nonexistent/zeron-liveness-test-missing.jsonl");
        assert!(!mtime_is_recent(missing, SystemTime::now()));
    }

    #[test]
    fn parses_pid_and_args_rows() {
        let text = "  123 /opt/homebrew/bin/claude --resume=abc-123\n 4567   node script.js --flag value\n";
        let rows = parse_ps_rows(text);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].pid, 123);
        assert_eq!(rows[0].args, "/opt/homebrew/bin/claude --resume=abc-123");
        assert_eq!(rows[1].pid, 4567);
        assert_eq!(rows[1].args, "node script.js --flag value");
    }

    #[test]
    fn matches_on_session_id_in_args() {
        let rows = vec![PsRow {
            pid: 1,
            args: "/opt/homebrew/bin/claude --resume=abc-123".into(),
        }];
        assert!(processes_matching(&rows, "claude", "abc-123", "", |_| None));
        assert!(!processes_matching(
            &rows,
            "claude",
            "not-present",
            "",
            |_| None
        ));
    }

    #[test]
    fn does_not_match_wrong_binary_name() {
        // "claude" appears in the args, but the binary itself is "node" —
        // must not match on substring alone, only on argv[0]'s basename.
        let rows = vec![PsRow {
            pid: 1,
            args: "node /usr/local/bin/claude-wrapper.js --session claude-thing".into(),
        }];
        assert!(!processes_matching(
            &rows,
            "claude",
            "claude-thing",
            "",
            |_| None
        ));
    }

    #[test]
    fn matches_on_real_cwd_when_args_have_no_session_id() {
        let rows = vec![PsRow {
            pid: 42,
            args: "/opt/homebrew/bin/claude".into(),
        }];
        let lookup = |pid: u32| (pid == 42).then(|| "/work/project".to_string());
        assert!(processes_matching(
            &rows,
            "claude",
            "no-match-id",
            "/work/project",
            lookup
        ));
        assert!(!processes_matching(
            &rows,
            "claude",
            "no-match-id",
            "/other/dir",
            lookup
        ));
    }

    #[test]
    fn undetermined_cwd_lookup_does_not_match() {
        let rows = vec![PsRow {
            pid: 42,
            args: "/opt/homebrew/bin/claude".into(),
        }];
        assert!(!processes_matching(
            &rows,
            "claude",
            "no-id",
            "/work/project",
            |_| None
        ));
    }

    #[test]
    fn real_ps_invocation_does_not_panic_and_returns_rows_or_empty() {
        // Smoke test against the real `ps` on this machine: don't assert on
        // specific content (that's covered by the pure-function tests
        // above), just confirm the real invocation path is wired correctly
        // and doesn't error out.
        let rows = list_processes();
        assert!(
            !rows.is_empty(),
            "expected `ps` to report at least this test process"
        );
    }

    #[test]
    fn real_lsof_invocation_finds_this_process_own_cwd() {
        // End-to-end against the real `lsof` on this machine: the current
        // test process's own real cwd is the crate's own directory (cargo/
        // rustc's cwd when running tests), so just confirm we get *some*
        // absolute path back rather than asserting an exact value that
        // depends on how this test binary was invoked.
        let Some(cwd) = real_cwd_of(std::process::id()) else {
            return; // no `lsof` on this machine/platform — skip
        };
        assert!(
            cwd.starts_with('/'),
            "expected an absolute path, got {cwd:?}"
        );
    }

    #[test]
    fn finds_a_real_spawned_process_end_to_end_by_session_id_in_args() {
        // No real `claude` binary needed: spawn a genuine, harmless `sleep`
        // process using this test's own pid as a distinctive fingerprint —
        // passed as `sleep`'s duration argument (an earlier version of this
        // test tried embedding a marker via a `sh -c "sleep 2 # marker"`
        // shell comment; `/bin/sh` exec-optimizes a single simple command
        // and strips the comment during parsing, so the marker never
        // reached the actual process's argv at all — the fingerprint has to
        // be something the exec'd binary itself actually receives).
        let fingerprint = std::process::id().to_string();
        let mut child = match Command::new("sleep").arg(&fingerprint).spawn() {
            Ok(child) => child,
            Err(_) => return, // no `sleep` on this machine/platform — skip
        };

        std::thread::sleep(Duration::from_millis(200));
        let rows = list_processes();
        let found = processes_matching(&rows, "sleep", &fingerprint, "", |_| None);

        let _ = child.kill();
        let _ = child.wait();

        assert!(
            found,
            "expected to find the real spawned `sleep {fingerprint}` process via ps"
        );
    }
}
