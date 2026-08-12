//! On-disk snapshot of workspace layout -- which panes existed, where,
//! and what directory each was running in -- so a *deliberate* daemon
//! restart (e.g. to pick up a rebuilt binary, the whole reason this
//! module exists) doesn't have to mean losing every open pane.
//!
//! This is explicitly NOT process migration. The actual PTY-backed
//! child process behind each pane (a shell, an editor mid-edit,
//! whatever) dies with the old daemon regardless of anything this
//! module does: once the daemon process exits, every PTY master fd it
//! held closes, and the kernel delivers `SIGHUP` to each slave's
//! foreground process group -- the same mechanism that kills a shell
//! when you close the terminal window it's running in. There is no
//! user-space way to snapshot a live process's memory/fds and resume it
//! elsewhere without OS-level checkpoint/restore (e.g. Linux's CRIU,
//! unavailable on macOS and not a dependency this crate takes on).
//!
//! What *does* survive, and what [`SavedSession`] actually captures, is
//! the layout: which workspace had which split structure, which leaf
//! held which pane(s), each pane's custom name and last-known working
//! directory. Restoring replays that shape with fresh default shells
//! spawned into the same directories -- the same experience as a
//! terminal app that remembers your window arrangement, not the
//! stronger guarantee suspend/resume would be. Shell history within
//! the dead session, an unfinished foreground command, interactively-set
//! env vars -- none of that comes back, because none of it ever left
//! the process that's now gone.
//!
//! Saved on clean daemon shutdown (`daemon::mod`'s `spawn_cleanup_on_signal`),
//! consumed *and deleted* the next time a daemon starts via
//! `run_and_restore_session` -- so an ordinary subsequent start (no
//! save in between) never re-replays stale layout. Deliberately not
//! wired into plain `State::new()`/`run()`: every existing test wants a
//! deterministically empty starting `State` regardless of what a real
//! session file this file's own tests exercise looks like on the
//! machine running them.

use crate::protocol::SplitDir;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedSession {
    pub workspaces: Vec<SavedWorkspace>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedWorkspace {
    pub number: u8,
    pub name: Option<String>,
    pub tree: Option<SavedTree>,
}

/// Mirrors `protocol::SplitTree`'s shape, deliberately with no id
/// fields at all -- a `ClientPaneId`/`ServerPaneId`/`SplitId` from the
/// dead daemon means nothing to the next one, so there's nothing here
/// for a reader to wonder whether they need to remap; restoring always
/// mints fresh ones.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SavedTree {
    Leaf {
        name: Option<String>,
        tabs: Vec<SavedServerPane>,
        active_tab: usize,
    },
    Split {
        dir: SplitDir,
        ratio: f32,
        a: Box<SavedTree>,
        b: Box<SavedTree>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedServerPane {
    pub name: Option<String>,
    /// Last-known working directory, if it could be determined --
    /// `None` respawns at the daemon's own cwd, same as an ordinary
    /// `ServerSpawn` with no `cwd` given.
    pub cwd: Option<String>,
}

/// `<config-dir>/dimax/session.json` -- same directory and
/// `$XDG_CONFIG_HOME`/`$HOME` fallback convention as
/// `daemon::pinned_dirs::file_path`. `None` only if neither is set,
/// which every caller here treats as "no persistence available this
/// run" rather than an error, matching `pinned_dirs`.
fn file_path() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(dir).join("dimax").join("session.json"));
    }
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config").join("dimax").join("session.json"))
}

/// Persist `session` to disk, creating the containing directory if
/// needed. Best-effort, same rationale as `pinned_dirs::save`: whether
/// this could be written to disk must never be allowed to block or fail
/// a shutdown that's already in progress.
pub fn save(session: &SavedSession) {
    let Some(path) = file_path() else { return };
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(json) = serde_json::to_string_pretty(session) else {
        return;
    };
    if let Ok(mut file) = std::fs::File::create(&path) {
        let _ = file.write_all(json.as_bytes());
    }
}

/// Load and delete the saved session, if any -- restoring is a one-time
/// replay, not a standing "always start like this" config, so the file
/// is consumed rather than left for a subsequent ordinary start (with
/// no save in between) to replay again. Returns `None` -- not an error
/// -- if there's no file (the common case: the previous shutdown wasn't
/// a clean one, or this is the first run ever) or it fails to parse (a
/// hand-edited or corrupted file shouldn't prevent the daemon from
/// starting; worst case, the layout just doesn't come back).
pub fn take() -> Option<SavedSession> {
    let path = file_path()?;
    let contents = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    serde_json::from_str(&contents).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_fake_config_home<T>(dir: &std::path::Path, f: impl FnOnce() -> T) -> T {
        // Shared across every module in this crate that mutates
        // `$XDG_CONFIG_HOME` -- see `super::super::XDG_CONFIG_HOME_TEST_LOCK`'s
        // doc comment for why a per-module lock isn't enough:
        // `pinned_dirs` resolves its own on-disk path from the same env
        // var, and a lock private to just this module can't prevent a
        // race against that module's own tests.
        let _guard = super::super::XDG_CONFIG_HOME_TEST_LOCK.lock().unwrap();
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

    fn sample() -> SavedSession {
        SavedSession {
            workspaces: vec![SavedWorkspace {
                number: 1,
                name: None,
                tree: Some(SavedTree::Split {
                    dir: SplitDir::Vertical,
                    ratio: 0.5,
                    a: Box::new(SavedTree::Leaf {
                        name: Some("editor".to_string()),
                        tabs: vec![SavedServerPane {
                            name: Some("editor".to_string()),
                            cwd: Some("/home/dev/project".to_string()),
                        }],
                        active_tab: 0,
                    }),
                    b: Box::new(SavedTree::Leaf {
                        name: None,
                        tabs: vec![SavedServerPane {
                            name: None,
                            cwd: None,
                        }],
                        active_tab: 0,
                    }),
                }),
            }],
        }
    }

    #[test]
    fn take_returns_none_when_no_file_exists() {
        let dir = std::env::temp_dir().join(format!("dmx-session-test-{}", std::process::id()));
        with_fake_config_home(&dir, || {
            assert_eq!(take(), None);
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_then_take_round_trips_the_session() {
        let dir = std::env::temp_dir().join(format!("dmx-session-test-{}", std::process::id() + 1));
        with_fake_config_home(&dir, || {
            save(&sample());
            assert_eq!(take(), Some(sample()));
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn take_deletes_the_file_so_a_second_call_gets_nothing() {
        let dir = std::env::temp_dir().join(format!("dmx-session-test-{}", std::process::id() + 2));
        with_fake_config_home(&dir, || {
            save(&sample());
            assert!(take().is_some());
            assert_eq!(take(), None, "a session file must be consumed, not replayed forever");
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn take_returns_none_for_a_corrupted_file_rather_than_panicking() {
        let dir = std::env::temp_dir().join(format!("dmx-session-test-{}", std::process::id() + 3));
        with_fake_config_home(&dir, || {
            let path = dir.join("dimax").join("session.json");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "not valid json").unwrap();
            assert_eq!(take(), None);
        });
        let _ = std::fs::remove_dir_all(&dir);
    }
}
