//! On-disk persistence for the attach menu's pinned-directory-group
//! list -- the one piece of dimax state that survives a daemon
//! restart. Every other piece of `State` (the server-pane pool, split
//! trees, etc.) is intentionally ephemeral, tied to the daemon
//! process's own lifetime; pinning is different because it's a pure
//! per-user *preference* about how to browse directories, not
//! something tied to any currently-running process, so losing it on
//! every restart would be a real (if small) usability regression.
//!
//! Deliberately the daemon's own concern, not something client
//! (TUI/CLI) code loads independently: `State` is the single owner of
//! `pinned_dirs`' in-memory copy, and every mutation of it re-saves
//! immediately (see `save`'s call sites in `state.rs`) rather than
//! batching -- pin/unpin is a rare, deliberate action, not something
//! happening often enough that write-amortization would matter.

use std::io::Write;
use std::path::PathBuf;

/// `<config-dir>/dimax/pinned_dirs.json`, where `<config-dir>` is
/// `$XDG_CONFIG_HOME` if set, else `~/.config` -- same fallback
/// convention `protocol::socket_path` already uses for
/// `$XDG_RUNTIME_DIR`. Returns `None` only if neither `$XDG_CONFIG_HOME`
/// nor `$HOME` is set, which [`load`]/[`save`] both treat as "no
/// persistence available this run" rather than an error.
fn file_path() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(dir).join("dimax").join("pinned_dirs.json"));
    }
    let home = std::env::var("HOME").ok()?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("dimax")
            .join("pinned_dirs.json"),
    )
}

/// Load the saved pin order, oldest-pinned first (see `state.rs`'s
/// `pinned_dirs` field doc for why order matters). Returns an empty
/// `Vec` -- not an error -- if the file doesn't exist yet (the common
/// case: nothing has ever been pinned) or fails to parse (a corrupted
/// or hand-edited file shouldn't prevent the daemon from starting;
/// worst case, previously pinned dirs just don't come back pinned).
pub fn load() -> Vec<String> {
    let Some(path) = file_path() else {
        return Vec::new();
    };
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    serde_json::from_str(&contents).unwrap_or_default()
}

/// Persist `dirs` (the full current pin order) to disk, creating the
/// containing directory if needed. Best-effort: any I/O error is
/// silently swallowed rather than propagated -- same rationale as
/// the keybinding config loader (see `tui/keys.rs`): whether
/// a preference could be saved to disk must never be allowed to fail a
/// request the daemon is otherwise perfectly able to serve from memory.
pub fn save(dirs: &[String]) {
    let Some(path) = file_path() else { return };
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(json) = serde_json::to_string_pretty(dirs) else {
        return;
    };
    if let Ok(mut file) = std::fs::File::create(&path) {
        let _ = file.write_all(json.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_fake_config_home<T>(dir: &std::path::Path, f: impl FnOnce() -> T) -> T {
        // See `crate::ENV_FAKE_HOME_LOCK`'s doc comment: shared across
        // every module's tests that fake this same process-global env
        // var, not just this module's own.
        let _guard = crate::ENV_FAKE_HOME_LOCK.blocking_lock();
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", dir);
        }
        let result = f();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
        result
    }

    #[test]
    fn load_returns_empty_when_no_file_exists() {
        let dir = std::env::temp_dir().join(format!("dmx-pinned-test-{}", std::process::id()));
        with_fake_config_home(&dir, || {
            assert_eq!(load(), Vec::<String>::new());
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_then_load_round_trips_the_pin_order() {
        let dir = std::env::temp_dir().join(format!("dmx-pinned-test-{}", std::process::id() + 1));
        with_fake_config_home(&dir, || {
            save(&["/home/dev/api".to_string(), "/home/dev/web".to_string()]);
            assert_eq!(
                load(),
                vec!["/home/dev/api".to_string(), "/home/dev/web".to_string()]
            );
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_returns_empty_for_a_corrupted_file_rather_than_panicking() {
        let dir = std::env::temp_dir().join(format!("dmx-pinned-test-{}", std::process::id() + 2));
        with_fake_config_home(&dir, || {
            let path = dir.join("dimax").join("pinned_dirs.json");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "not valid json").unwrap();
            assert_eq!(load(), Vec::<String>::new());
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_overwrites_a_previous_save() {
        let dir = std::env::temp_dir().join(format!("dmx-pinned-test-{}", std::process::id() + 3));
        with_fake_config_home(&dir, || {
            save(&["/a".to_string()]);
            save(&["/b".to_string(), "/c".to_string()]);
            assert_eq!(load(), vec!["/b".to_string(), "/c".to_string()]);
        });
        let _ = std::fs::remove_dir_all(&dir);
    }
}
