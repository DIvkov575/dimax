//! On-disk persistence for recognized AI-coding-CLI sessions (see
//! `protocol::SessionKind`) -- lets the daemon bring back what it was
//! running after dying *without* a clean shutdown (a crash, `SIGKILL`,
//! or genuine power loss), which every other piece of `State` (the
//! server-pane pool, split trees, etc.) has no way to survive on its
//! own once the process itself is gone.
//!
//! This is a deliberately narrow safety net, not a general session
//! manager:
//! - Only recognized agent-tool panes are captured (see
//!   `State::restorable_sessions`'s doc comment for why plain shells/
//!   editors aren't worth resurrecting on their own).
//! - Restoring re-launches the same tool in the same directory as a
//!   fresh, unbound ("orphan") server-pane -- it recreates the *shape*
//!   of what was running, not the actual prior scrollback/conversation
//!   state, which is genuinely gone once the process itself is. Most
//!   recognized tools have their own session-resume mechanism
//!   (`claude --continue`, etc.) that picking up in the same directory
//!   puts back within reach.
//! - Workspace/client-pane layout is intentionally not part of this --
//!   see `State::restore_sessions_from_disk`'s doc comment.
//!
//! The file is written periodically while the daemon runs (see
//! `daemon::mod`'s periodic-snapshot task) and deleted on a clean
//! shutdown (`spawn_cleanup_on_signal`) -- so a file still present at
//! the next fresh start (`State::new`'s caller, `daemon::mod::run`) is
//! itself the signal that the previous run ended uncleanly and this
//! run should restore from it. A hot reload's own resume path
//! (`State::from_resume`) never touches this file at all: that
//! mechanism already carries every live pane across via its real PTY
//! fd, so restoring *additionally* from this snapshot would just
//! double-spawn everything.

use std::io::Write;
use std::path::PathBuf;

/// One recognized session worth bringing back -- see
/// `State::restorable_sessions`'s doc comment for exactly what these
/// fields mean and where they come from.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RestorableSession {
    pub name: Option<String>,
    pub cmd: String,
    pub cwd: Option<String>,
}

/// `<config-dir>/dimax/sessions.json` -- same fallback convention as
/// `pinned_dirs::file_path`.
fn file_path() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(dir).join("dimax").join("sessions.json"));
    }
    let home = std::env::var("HOME").ok()?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("dimax")
            .join("sessions.json"),
    )
}

/// Load the last-saved session list. Returns an empty `Vec` -- not an
/// error -- if the file doesn't exist (the common case: either nothing
/// has been saved yet, or the previous run shut down cleanly and
/// deleted it) or fails to parse (a corrupted or hand-edited file
/// shouldn't prevent the daemon from starting; worst case, nothing
/// gets restored).
pub fn load() -> Vec<RestorableSession> {
    let Some(path) = file_path() else {
        return Vec::new();
    };
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    serde_json::from_str(&contents).unwrap_or_default()
}

/// Persist `sessions` (the full current snapshot), creating the
/// containing directory if needed. Best-effort: any I/O error is
/// silently swallowed, same rationale as `pinned_dirs::save` -- a
/// snapshot write failing must never be allowed to disrupt the daemon's
/// actual work.
pub fn save(sessions: &[RestorableSession]) {
    let Some(path) = file_path() else { return };
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(json) = serde_json::to_string_pretty(sessions) else {
        return;
    };
    if let Ok(mut file) = std::fs::File::create(&path) {
        let _ = file.write_all(json.as_bytes());
    }
}

/// Delete the persisted snapshot -- called on a clean shutdown
/// (`spawn_cleanup_on_signal`) so the *next* fresh start doesn't find a
/// stale file and mistake an intentional stop for an unclean one.
/// Best-effort: nothing further to do if the file doesn't exist or
/// can't be removed.
pub fn clear() {
    if let Some(path) = file_path() {
        let _ = std::fs::remove_file(&path);
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
        let dir = std::env::temp_dir().join(format!("dmx-sessions-test-{}", std::process::id()));
        with_fake_config_home(&dir, || {
            assert_eq!(load(), Vec::<RestorableSession>::new());
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir =
            std::env::temp_dir().join(format!("dmx-sessions-test-{}", std::process::id() + 1));
        with_fake_config_home(&dir, || {
            let sessions = vec![
                RestorableSession {
                    name: Some("agent".to_string()),
                    cmd: "claude".to_string(),
                    cwd: Some("/home/dev/api".to_string()),
                },
                RestorableSession {
                    name: None,
                    cmd: "codex".to_string(),
                    cwd: None,
                },
            ];
            save(&sessions);
            assert_eq!(load(), sessions);
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_returns_empty_for_a_corrupted_file_rather_than_panicking() {
        let dir =
            std::env::temp_dir().join(format!("dmx-sessions-test-{}", std::process::id() + 2));
        with_fake_config_home(&dir, || {
            let path = dir.join("dimax").join("sessions.json");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "not valid json").unwrap();
            assert_eq!(load(), Vec::<RestorableSession>::new());
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clear_removes_a_saved_file_and_is_a_no_op_without_one() {
        let dir =
            std::env::temp_dir().join(format!("dmx-sessions-test-{}", std::process::id() + 3));
        with_fake_config_home(&dir, || {
            save(&[RestorableSession {
                name: None,
                cmd: "claude".to_string(),
                cwd: None,
            }]);
            assert!(!load().is_empty());
            clear();
            assert!(load().is_empty());
            // Calling again with no file present must not panic.
            clear();
        });
        let _ = std::fs::remove_dir_all(&dir);
    }
}
