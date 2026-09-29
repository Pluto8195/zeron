//! One initialize response feeds both picker catalogs for two minutes.
use crate::HarnessError;
use serde_json::Value;
use std::{collections::HashMap, future::Future, time::Duration};
use tokio::time::Instant;
use zeron_proto::SlashCommand;

#[derive(Default)]
pub(super) struct InitializeCache {
    state: tokio::sync::Mutex<State>,
}
#[derive(Default)]
struct State {
    context: Option<[u8; 32]>,
    completed_at: Option<Instant>,
    response: Option<Result<Value, String>>,
}
impl InitializeCache {
    pub(super) async fn get<F, Fut>(
        &self,
        context: impl Fn() -> Result<[u8; 32], HarnessError>,
        probe: F,
    ) -> Result<Value, HarnessError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Value, HarnessError>>,
    {
        let requested_at = Instant::now();
        let mut state = self.state.lock().await;
        let key = context()?;
        if state.context != Some(key) {
            *state = State {
                context: Some(key),
                ..Default::default()
            };
        }
        if let (Some(at), Some(response)) = (state.completed_at, &state.response)
            && (at >= requested_at || (response.is_ok() && at.elapsed() < Duration::from_secs(120)))
        {
            return response.clone().map_err(HarnessError::Protocol);
        }
        let response = probe().await.map_err(|error| error.to_string());
        if context()? != key {
            *state = State::default();
            return Err(HarnessError::Protocol(
                "Claude credentials changed during initialize; retry".into(),
            ));
        }
        state.completed_at = Some(Instant::now());
        state.response = Some(response.clone());
        response.map_err(HarnessError::Protocol)
    }
}

/// Slash commands depend on the CLI's project cwd (`.claude/commands`,
/// project-scoped skills), unlike models — so this is deliberately a
/// SEPARATE cache from [`InitializeCache`], keyed by (credential context,
/// cwd) rather than context alone. Sharing `InitializeCache`'s single slot
/// would thrash it every time a cwd-independent models probe and a
/// per-chat-cwd commands probe interleaved. One short-lived `claude`
/// handshake per (context, cwd) is cheap and only fires on a picker
/// open — never per keystroke (see `ClaudeHarness::commands`) — and a
/// modest TTL keeps a long-running engine from respawning it for a chat
/// whose popup was just opened, while still picking up a command/skill
/// added to the project within the same session.
#[derive(Default)]
pub(super) struct CommandsCache {
    state: tokio::sync::Mutex<HashMap<([u8; 32], String), Entry>>,
}

struct Entry {
    at: Instant,
    commands: Vec<SlashCommand>,
}

const COMMANDS_CACHE_TTL: Duration = Duration::from_secs(300);

impl CommandsCache {
    pub(super) async fn get_or_probe<F, Fut>(
        &self,
        context: [u8; 32],
        cwd: &str,
        probe: F,
    ) -> Result<Vec<SlashCommand>, HarnessError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Vec<SlashCommand>, HarnessError>>,
    {
        let key = (context, cwd.to_owned());
        let mut state = self.state.lock().await;
        // Opportunistic eviction: a long-running engine accumulates entries
        // for every chat cwd/worktree ever probed, and nothing else prunes
        // this map.
        let now = Instant::now();
        state.retain(|_, entry| now.saturating_duration_since(entry.at) < COMMANDS_CACHE_TTL);
        if let Some(entry) = state.get(&key) {
            return Ok(entry.commands.clone());
        }
        // Held across the probe, like `InitializeCache`: discovery only
        // fires on a picker open, so serializing concurrent opens behind one
        // short-lived CLI handshake is cheap and keeps this simple.
        let commands = probe().await?;
        state.insert(
            key,
            Entry {
                at: Instant::now(),
                commands: commands.clone(),
            },
        );
        Ok(commands)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    #[tokio::test(start_paused = true)]
    async fn initialize_coalesces_expires_and_invalidates_with_credentials() {
        let cache = InitializeCache::default();
        let calls = AtomicUsize::new(0);
        let probe = || async {
            calls.fetch_add(1, Relaxed);
            tokio::time::sleep(Duration::from_millis(1)).await;
            Ok(serde_json::json!({"models":[],"commands":[]}))
        };
        let results =
            futures::future::join_all((0..10).map(|_| cache.get(|| Ok([1; 32]), probe))).await;
        assert!(results.iter().all(Result::is_ok));
        assert_eq!(calls.load(Relaxed), 1);
        tokio::time::advance(Duration::from_secs(119)).await;
        cache.get(|| Ok([1; 32]), probe).await.unwrap();
        assert_eq!(calls.load(Relaxed), 1);
        tokio::time::advance(Duration::from_secs(2)).await;
        cache.get(|| Ok([1; 32]), probe).await.unwrap();
        assert_eq!(calls.load(Relaxed), 2);
        cache.get(|| Ok([2; 32]), probe).await.unwrap();
        assert_eq!(calls.load(Relaxed), 3);
    }
    #[tokio::test]
    async fn failed_initialize_retries_and_raced_login_is_not_cached() {
        let cache = InitializeCache::default();
        assert!(
            cache
                .get(
                    || Ok([1; 32]),
                    || async { Err(HarnessError::Protocol("offline".into())) }
                )
                .await
                .is_err()
        );
        assert!(
            cache
                .get(|| Ok([1; 32]), || async { Ok(Value::Null) })
                .await
                .is_ok()
        );
        let key = AtomicUsize::new(2);
        assert!(
            cache
                .get(
                    || Ok([key.load(Relaxed) as u8; 32]),
                    || async {
                        key.store(3, Relaxed);
                        Ok(Value::Null)
                    }
                )
                .await
                .is_err()
        );
    }

    fn command(name: &str) -> SlashCommand {
        SlashCommand {
            name: name.into(),
            description: String::new(),
            input_hint: None,
        }
    }

    /// Pins the cache design behind the project-skills fix: commands are
    /// keyed by (credential context, cwd) — NOT context alone like
    /// `InitializeCache` — so two chats in different repos/worktrees each
    /// get their own probe and their own answer, a stale entry re-probes
    /// after the TTL, and a credential change also invalidates.
    #[tokio::test(start_paused = true)]
    async fn commands_cache_keys_by_context_and_cwd_and_expires() {
        let cache = CommandsCache::default();
        let calls = AtomicUsize::new(0);
        let probe_named = |name: &'static str| {
            let calls = &calls;
            move || async move {
                calls.fetch_add(1, Relaxed);
                Ok(vec![command(name)])
            }
        };

        let a1 = cache
            .get_or_probe([1; 32], "/repo/a", probe_named("a"))
            .await
            .unwrap();
        assert_eq!(calls.load(Relaxed), 1);
        assert_eq!(a1, vec![command("a")]);

        // Same (context, cwd): cache hit, no second probe.
        let a2 = cache
            .get_or_probe([1; 32], "/repo/a", probe_named("a"))
            .await
            .unwrap();
        assert_eq!(calls.load(Relaxed), 1, "cache hit for same (context, cwd)");
        assert_eq!(a2, a1);

        // Different cwd, same context: a chat in a different repo/worktree
        // gets its own probe and its own commands.
        let b = cache
            .get_or_probe([1; 32], "/repo/b", probe_named("b"))
            .await
            .unwrap();
        assert_eq!(calls.load(Relaxed), 2, "distinct cwd re-probes");
        assert_ne!(b, a1);

        // The first cwd's entry is untouched by probing the second.
        let a3 = cache
            .get_or_probe([1; 32], "/repo/a", probe_named("a"))
            .await
            .unwrap();
        assert_eq!(calls.load(Relaxed), 2, "first cwd's entry still cached");
        assert_eq!(a3, a1);

        // Different credential context, same cwd: also a separate entry.
        cache
            .get_or_probe([2; 32], "/repo/a", probe_named("a"))
            .await
            .unwrap();
        assert_eq!(calls.load(Relaxed), 3, "distinct context re-probes");

        // TTL: an expired entry re-probes.
        tokio::time::advance(Duration::from_secs(301)).await;
        cache
            .get_or_probe([1; 32], "/repo/a", probe_named("a"))
            .await
            .unwrap();
        assert_eq!(calls.load(Relaxed), 4, "expired entry re-probes");
    }
}
