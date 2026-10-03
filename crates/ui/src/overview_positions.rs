//! Local persistence for the multichat overview's free-form canvas tile
//! positions — the client-side analog of the reference `session_canvas.html`
//! tool's `positions[id]`/`savePositions()` (localStorage). Purely local,
//! per-device UI state, not synced workspace data, so it lives alongside
//! `ui-settings.json` under the engine's local data dir rather than in the
//! doc store — same convention `SettingsStore` already uses (`settings.rs`).
//!
//! Wired into `overview.rs`: loaded lazily on flat canvas mode's first
//! render (`ensure_positions_loaded`), saved on every committed tile-drag
//! release (`save_positions`).
//!
//! Also hosts the overview's other small per-device UI-state files — the
//! acknowledged-"done" set (the reference's
//! `session-canvas-acknowledged-done-v1` localStorage key) and the hidden-
//! repos filter set (the reference's `hiddenRepos`, persisted in its UI-state
//! key) — via the generic [`load_string_set`]/[`save_string_set`] pair, same
//! atomic-write/corrupt-degrades-to-empty contract as positions.

use std::collections::{BTreeSet, HashMap};
use std::io;
use std::path::{Path, PathBuf};

const FILE_NAME: &str = "overview-positions.json";

/// `chat_id -> (x, y)` in canvas logical (unzoomed) coordinates.
pub type Positions = HashMap<String, (f32, f32)>;

/// Load from `{data_dir}/overview-positions.json`. An absent or corrupt file
/// degrades to an empty map (no saved positions yet) rather than an error —
/// callers fall back to their own default layout in that case.
pub fn load_positions(data_dir: &Path) -> Positions {
    match std::fs::read_to_string(path(data_dir)) {
        Ok(text) => match serde_json::from_str::<Positions>(&text) {
            Ok(positions) => positions,
            Err(err) => {
                tracing::warn!(error = %err, "overview-positions corrupt; starting empty");
                Positions::new()
            }
        },
        Err(_) => Positions::new(),
    }
}

/// Write atomically (see [`write_json_atomic`]).
pub fn save_positions(data_dir: &Path, positions: &Positions) -> io::Result<()> {
    write_json_atomic(data_dir, &path(data_dir), positions)
}

pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join(FILE_NAME)
}

/// Chat ids whose "done" status the user has already acknowledged (expanded
/// the tile or opened the chat) — see `overview.rs`'s unread-done glow.
pub const ACKNOWLEDGED_DONE_FILE: &str = "overview-acknowledged-done.json";
/// Repo keys (cwd last path segment) the repo filter currently hides.
pub const HIDDEN_REPOS_FILE: &str = "overview-hidden-repos.json";
/// Named on/off overview UI toggles that persist (a string is present = on),
/// e.g. the My PRs sidebar. Unknown entries are ignored by readers.
pub const UI_FLAGS_FILE: &str = "overview-ui-flags.json";

/// Load a JSON string array from `{data_dir}/{file_name}`. Absent or corrupt
/// degrades to empty, same as [`load_positions`].
pub fn load_string_set(data_dir: &Path, file_name: &str) -> BTreeSet<String> {
    match std::fs::read_to_string(data_dir.join(file_name)) {
        Ok(text) => match serde_json::from_str::<BTreeSet<String>>(&text) {
            Ok(set) => set,
            Err(err) => {
                tracing::warn!(error = %err, file = file_name, "overview state file corrupt; starting empty");
                BTreeSet::new()
            }
        },
        Err(_) => BTreeSet::new(),
    }
}

/// Atomic write of a string set as a sorted JSON array.
pub fn save_string_set(data_dir: &Path, file_name: &str, set: &BTreeSet<String>) -> io::Result<()> {
    write_json_atomic(data_dir, &data_dir.join(file_name), set)
}

/// Temp file + rename so a crash mid-write never corrupts the file, matching
/// `SettingsStore::save`'s idiom.
fn write_json_atomic<T: serde::Serialize>(
    data_dir: &Path,
    dest: &Path,
    value: &T,
) -> io::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let tmp = dest.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(value)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_save_and_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut positions = Positions::new();
        positions.insert("chat-1".to_string(), (12.5, -40.0));
        positions.insert("chat-2".to_string(), (0.0, 0.0));

        save_positions(dir.path(), &positions).expect("save");
        let loaded = load_positions(dir.path());

        assert_eq!(loaded, positions);
    }

    #[test]
    fn missing_file_loads_as_empty_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let loaded = load_positions(dir.path());
        assert!(loaded.is_empty());
    }

    #[test]
    fn corrupt_file_degrades_to_empty_rather_than_panicking() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(path(dir.path()), b"{ not valid json").expect("write corrupt file");
        let loaded = load_positions(dir.path());
        assert!(loaded.is_empty());
    }

    #[test]
    fn save_creates_the_data_dir_if_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("nested").join("data");
        let positions = Positions::from([("chat-1".to_string(), (1.0, 2.0))]);

        save_positions(&nested, &positions).expect("save into missing dir");
        assert_eq!(load_positions(&nested), positions);
    }

    #[test]
    fn overwriting_replaces_the_previous_contents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = Positions::from([("chat-1".to_string(), (1.0, 1.0))]);
        save_positions(dir.path(), &first).expect("save first");

        let second = Positions::from([("chat-2".to_string(), (2.0, 2.0))]);
        save_positions(dir.path(), &second).expect("save second");

        assert_eq!(load_positions(dir.path()), second);
    }

    #[test]
    fn string_set_round_trips_and_missing_or_corrupt_is_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(load_string_set(dir.path(), HIDDEN_REPOS_FILE).is_empty());

        let set: BTreeSet<String> = ["zeron".to_string(), "ui".to_string()].into();
        save_string_set(dir.path(), HIDDEN_REPOS_FILE, &set).expect("save");
        assert_eq!(load_string_set(dir.path(), HIDDEN_REPOS_FILE), set);
        // Separate files don't bleed into each other.
        assert!(load_string_set(dir.path(), ACKNOWLEDGED_DONE_FILE).is_empty());

        std::fs::write(dir.path().join(ACKNOWLEDGED_DONE_FILE), b"[nope").expect("write");
        assert!(load_string_set(dir.path(), ACKNOWLEDGED_DONE_FILE).is_empty());
    }
}
