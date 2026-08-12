//! Wire protocol shared by the daemon, the TUI frontend, and the CLI.
//!
//! Requests flow client -> daemon. The daemon replies with exactly one
//! `Response` per `Request`, but a connection that has sent `Subscribe`
//! may also receive `Event` frames pushed asynchronously at any time
//! (layout changes, terminal output) — `ServerMessage` disambiguates the
//! two on the wire so a client can tell which kind of frame just arrived.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

pub type ServerPaneId = Uuid;
pub type WorkspaceId = Uuid;
pub type ClientPaneId = Uuid;
pub type SplitId = Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Size {
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitDir {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientPane {
    pub id: ClientPaneId,
    pub name: Option<String>,
    /// Server-panes this leaf can cycle between. Empty = unbound
    /// placeholder (design doc "Error handling").
    pub tabs: Vec<ServerPaneId>,
    /// Index into `tabs` of the one currently displayed. Out of range
    /// whenever `tabs` is empty, so read it through [`ClientPane::active_bound`]
    /// rather than indexing directly.
    pub active_tab: usize,
    /// A short, sequential, human-readable label (`"aa"`, `"ab"`, ...)
    /// assigned once at `client_spawn` time -- the same base-62 scheme
    /// `ServerPaneInfo::short_id` uses, just from an independent counter
    /// (a client-pane and a server-pane are different kinds of thing, so
    /// their sequences don't need to be related). Ephemeral like the
    /// server-pane version: resets to `"aa"` on daemon restart. Purely a
    /// nicer fallback title than a raw UUID prefix when the pane has no
    /// custom `name` -- never used for addressing.
    pub short_id: String,
}

impl ClientPane {
    pub fn active_bound(&self) -> Option<ServerPaneId> {
        self.tabs.get(self.active_tab).copied()
    }
}

/// A binary split tree of client-panes within one workspace.
/// See docs/superpowers/specs/2026-07-30-dimax-design.md "Data model reference".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SplitTree {
    Leaf(ClientPane),
    Split {
        /// Stable identity for this divider, independent of `a`/`b`'s
        /// contents — what `Request::ResizeSplit` addresses to change
        /// *this* divider's ratio without needing to name either child
        /// pane (which may itself be a nested split with no single pane
        /// id of its own).
        id: SplitId,
        dir: SplitDir,
        /// Fraction of space given to `a`; `b` gets the remainder.
        ratio: f32,
        a: Box<SplitTree>,
        b: Box<SplitTree>,
    },
}

impl SplitTree {
    /// Find a leaf by client-pane id.
    pub fn find(&self, pane: ClientPaneId) -> Option<&ClientPane> {
        match self {
            SplitTree::Leaf(p) if p.id == pane => Some(p),
            SplitTree::Leaf(_) => None,
            SplitTree::Split { a, b, .. } => a.find(pane).or_else(|| b.find(pane)),
        }
    }

    /// Find a leaf by client-pane id, mutably.
    pub fn find_mut(&mut self, pane: ClientPaneId) -> Option<&mut ClientPane> {
        match self {
            SplitTree::Leaf(p) if p.id == pane => Some(p),
            SplitTree::Leaf(_) => None,
            SplitTree::Split { a, b, .. } => {
                if let Some(found) = a.find_mut(pane) {
                    return Some(found);
                }
                b.find_mut(pane)
            }
        }
    }

    /// Split the leaf identified by `target` into two leaves: the
    /// existing pane and `new_pane`. Returns an error if `target` does
    /// not name a leaf in this tree.
    pub fn split_leaf(
        &mut self,
        target: ClientPaneId,
        dir: SplitDir,
        new_pane: ClientPane,
    ) -> anyhow::Result<()> {
        match self {
            SplitTree::Leaf(p) if p.id == target => {
                let existing = p.clone();
                *self = SplitTree::Split {
                    id: SplitId::new_v4(),
                    dir,
                    ratio: 0.5,
                    a: Box::new(SplitTree::Leaf(existing)),
                    b: Box::new(SplitTree::Leaf(new_pane)),
                };
                Ok(())
            }
            SplitTree::Leaf(_) => anyhow::bail!("client-pane {target} not found"),
            SplitTree::Split { a, b, .. } => {
                if a.find(target).is_some() {
                    a.split_leaf(target, dir, new_pane)
                } else if b.find(target).is_some() {
                    b.split_leaf(target, dir, new_pane)
                } else {
                    anyhow::bail!("client-pane {target} not found")
                }
            }
        }
    }

    /// Remove the leaf identified by `target`. Returns `Ok(None)` if the
    /// whole tree became empty (the removed leaf was the only node),
    /// `Ok(Some(new_tree))` otherwise, or an error if not found.
    pub fn remove_leaf(self, target: ClientPaneId) -> anyhow::Result<Option<SplitTree>> {
        match self {
            SplitTree::Leaf(p) if p.id == target => Ok(None),
            SplitTree::Leaf(_) => anyhow::bail!("client-pane {target} not found"),
            SplitTree::Split {
                id,
                dir,
                ratio,
                a,
                b,
            } => {
                let a_has = a.find(target).is_some();
                let b_has = b.find(target).is_some();
                if a_has {
                    match a.remove_leaf(target)? {
                        Some(new_a) => Ok(Some(SplitTree::Split {
                            id,
                            dir,
                            ratio,
                            a: Box::new(new_a),
                            b,
                        })),
                        None => Ok(Some(*b)),
                    }
                } else if b_has {
                    match b.remove_leaf(target)? {
                        Some(new_b) => Ok(Some(SplitTree::Split {
                            id,
                            dir,
                            ratio,
                            a,
                            b: Box::new(new_b),
                        })),
                        None => Ok(Some(*a)),
                    }
                } else {
                    anyhow::bail!("client-pane {target} not found")
                }
            }
        }
    }

    /// All leaves in this tree, in left-to-right order.
    pub fn leaves(&self) -> Vec<&ClientPane> {
        match self {
            SplitTree::Leaf(p) => vec![p],
            SplitTree::Split { a, b, .. } => {
                let mut v = a.leaves();
                v.extend(b.leaves());
                v
            }
        }
    }

    /// Set the ratio of the split identified by `target`, clamped to a
    /// sane range so a drag can never collapse a pane to zero width/height
    /// (design intent for mouse-drag resizing: dragging can push a pane
    /// small, not make it vanish). Returns an error if `target` does not
    /// name a split in this tree.
    pub fn resize_split(&mut self, target: SplitId, new_ratio: f32) -> anyhow::Result<()> {
        match self {
            SplitTree::Leaf(_) => anyhow::bail!("split {target} not found"),
            SplitTree::Split {
                id, ratio, a, b, ..
            } if *id == target => {
                *ratio = new_ratio.clamp(0.05, 0.95);
                Ok(())
            }
            SplitTree::Split { a, b, .. } => {
                if a.resize_split(target, new_ratio).is_ok() {
                    Ok(())
                } else {
                    b.resize_split(target, new_ratio)
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub id: WorkspaceId,
    pub number: u8,
    pub name: Option<String>,
    /// `None` = workspace exists but has no panes yet.
    pub tree: Option<SplitTree>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerPaneStatus {
    Running,
    /// Process exited; last screen contents are retained for display.
    Dead,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerPaneInfo {
    pub id: ServerPaneId,
    pub name: Option<String>,
    pub size: Size,
    pub status: ServerPaneStatus,
    /// A live OS-level snapshot of the PTY's foreground process, queried
    /// fresh at the moment this `ServerPaneInfo` was built (design doc
    /// "Attach menu identification columns" — not tracked/cached
    /// continuously, since the attach menu already re-fetches on every
    /// open). `None` if the process couldn't be queried (e.g. a `Dead`
    /// pane has no foreground process to look up) or on a platform where
    /// this isn't supported.
    pub foreground: Option<ForegroundProcessInfo>,
    /// The workspace this server-pane was spawned from, if any. Set once
    /// at `ServerSpawn` time from the request's own `workspace` field and
    /// never changed afterward — binding/adding this pane as a tab into a
    /// *different* workspace's client-pane does not transfer ownership.
    /// `None` for a pane spawned with no workspace context (e.g. `dimax
    /// server spawn` from the CLI) — an "orphan" pane, always shown in
    /// every workspace's attach menu regardless of the
    /// same-workspace-only filter (see `tui::filter_servers_for_menu`).
    pub owner_workspace: Option<WorkspaceId>,
    /// A short, human-friendly display label assigned once at spawn time
    /// from a per-daemon sequential counter (see `daemon::state::encode_short_id`):
    /// `"aa"`, `"ab"`, ..., `"az"`, `"ba"`, ..., `"zz"`, `"Aa"`, ...,
    /// `"AA"`, ..., `"ZZ"`, `"aaa"`, ... -- replaces the truncated-UUID
    /// fallback previously shown wherever a pane has no user-assigned
    /// `name`. Display-only: this is never accepted as an address by
    /// `resolve_server_pane` or anywhere else, only the full id or a
    /// real `name` are. Ephemeral like everything except `pinned_dirs`
    /// -- resets to `"aa"` on every daemon restart, so it is not stable
    /// across restarts the way a real name is.
    pub short_id: String,
}

/// See [`ServerPaneInfo::foreground`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForegroundProcessInfo {
    /// The foreground process's own name (e.g. `vim`, `bash`) — what's
    /// actually running right now, not necessarily what the pane was
    /// spawned with.
    pub process_name: String,
    /// The foreground process's current working directory, if it could
    /// be determined.
    pub cwd: Option<String>,
}

/// One screen cell. Kept intentionally simple for v1 — enough styling to
/// render legibly, not a full terminal-attribute model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cell {
    /// The grapheme occupying this cell (usually one `char`, but wide/
    /// combining glyphs may take more than one `char`).
    pub text: String,
    pub fg: Option<(u8, u8, u8)>,
    pub bg: Option<(u8, u8, u8)>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
}

/// A full snapshot of a server-pane's visible grid.
///
/// v1 always sends the complete grid rather than a true cell-level diff —
/// simplest correct thing to build first. Bandwidth optimization (only
/// send changed lines) is a follow-up, not required for correctness.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GridSnapshot {
    pub server_pane: ServerPaneId,
    pub size: Size,
    pub cursor: (u16, u16),
    /// Row-major: `lines[row][col]`.
    pub lines: Vec<Vec<Cell>>,
    /// How many rows back from the live tail this snapshot's `lines`
    /// starts at (0 = the live/current viewport, matching every
    /// snapshot before this field existed). Lets a frontend distinguish
    /// "this pane is showing history" from "this pane is live" without
    /// separately tracking offset state itself.
    pub scroll_offset: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    ServerSpawn {
        name: Option<String>,
        cmd: Option<String>,
        /// Starting working directory for the spawned process; `None`
        /// leaves the daemon's own cwd in place (the pre-existing
        /// default). Set by the attach menu's per-group "spawn new
        /// here" row to start the new pane in that group's directory.
        cwd: Option<String>,
        /// Same name-or-number-or-id addressing as `ClientSpawn`'s
        /// `workspace` field, but optional: `Some` records this pane's
        /// `owner_workspace` (every TUI-driven spawn passes the current
        /// workspace); `None` leaves it unowned/orphaned (e.g. `dimax
        /// server spawn` from the CLI, which has no workspace context).
        /// Unlike `ClientSpawn`, a `Some` value here never creates a
        /// workspace that doesn't already exist -- there is no
        /// client-pane to put in it, so an unknown workspace is just an
        /// error.
        workspace: Option<String>,
    },
    /// `target` is matched against both pane name and id (as a string).
    ServerKill {
        target: String,
    },
    /// `new_name: None` clears any custom name, resetting the pane back
    /// to its default display (its `short_id`, or a freshly re-derived
    /// session name -- see `daemon::state::apply_pending_session_names`)
    /// rather than being a no-op.
    ServerRename {
        target: String,
        new_name: Option<String>,
    },
    ServerList,
    /// Flip whether `dir` sorts to the top of the attach menu's
    /// directory groups -- pins it (if not already pinned) or unpins
    /// it (if it is). `dir` is an opaque string matched against
    /// `ServerPaneInfo::foreground.cwd` values on the client side (see
    /// `tui::group_servers_by_cwd`); the daemon does no validation of
    /// it beyond storing it verbatim, since pinning a directory with
    /// no server-panes in it right now is entirely reasonable (it'll
    /// just have no effect until one appears there). Persisted to disk
    /// so it survives a daemon restart -- see `daemon::pinned_dirs`'s
    /// module doc.
    ToggleDirectoryPin {
        dir: String,
    },
    /// The current pin order, earliest-pinned first -- fetched
    /// alongside `ServerList` (see `Response::PinnedDirsList`) whenever
    /// a client needs to reproduce the attach menu's grouping.
    PinnedDirsList,
    /// Atomically check-and-consume the "spawn a default shell instead
    /// of showing the picker" fallback for a fresh empty-workspace
    /// attach. `Response::ShellFallback { available: true }` the very
    /// first time this is ever sent to a given daemon instance; `false`
    /// every time after, for the lifetime of that daemon process (never
    /// persisted -- a restarted daemon grants the fallback again). See
    /// `tui::App::bootstrap`'s call site for why this exists: a
    /// brand-new install should get one pane with zero clicks, but
    /// every attach after that shows the real picker.
    ConsumeShellFallback,

    /// Create a client-pane in `workspace` (created if it doesn't exist).
    /// `split_of` names an existing leaf to split; if `None`, the
    /// workspace must currently be empty (ambiguous otherwise — see
    /// design doc "CLI surface").
    ClientSpawn {
        workspace: String,
        split_of: Option<ClientPaneId>,
        dir: Option<SplitDir>,
        bind: Option<String>,
    },
    ClientClose {
        workspace: String,
        pane: ClientPaneId,
    },
    ClientRename {
        workspace: String,
        pane: ClientPaneId,
        new_name: String,
    },
    ClientBind {
        workspace: String,
        pane: ClientPaneId,
        target: String,
    },
    /// Detach `pane` from whatever server-pane it's currently bound to,
    /// leaving it unbound. The detached server-pane keeps running (design
    /// doc "Error handling"'s "unbound placeholder" semantics, triggered
    /// here deliberately rather than as a side effect of a kill). A no-op
    /// (still `Ack`) if `pane` was already unbound.
    ClientUnbind {
        workspace: String,
        pane: ClientPaneId,
    },
    ClientList {
        workspace: Option<String>,
    },
    /// Bind another server-pane into `pane` as an additional tab and
    /// make it active. `target` uses the same name-or-id addressing as
    /// `ClientBind`.
    ClientAddTab {
        workspace: String,
        pane: ClientPaneId,
        target: String,
    },
    /// Move `pane`'s active tab one step forward (`forward`) or back,
    /// wrapping at either end. A no-op on a pane with fewer than two
    /// tabs.
    ClientCycleTab {
        workspace: String,
        pane: ClientPaneId,
        forward: bool,
    },
    /// Drop `pane`'s active tab, leaving the pane's other tabs (and the
    /// dropped tab's server-pane, which keeps running) intact. Closing
    /// the last tab closes `pane` itself, exactly like `ClientClose` —
    /// there is no "0-tab but still present" leaf state reachable this
    /// way (see design doc "Architecture").
    ClientCloseTab {
        workspace: String,
        pane: ClientPaneId,
    },

    /// Frontend-only: begin receiving `Event`s for this workspace and
    /// register this connection as a viewer of every server-pane
    /// currently bound within it (for PTY-sizing purposes).
    Subscribe {
        workspace: String,
    },
    /// Tell the daemon which server-pane (if any) this connection's TUI
    /// currently has keyboard focus on. Purely a broadcast-scheduling
    /// hint (see `daemon::state::State::throttled_subscribers_for_server_pane`):
    /// a focused pane's `GridDelta`s are never throttled; an unfocused
    /// one's are coalesced to a lower rate, since nobody is watching it
    /// in real time. The daemon always answers with one immediate,
    /// unthrottled catch-up push for the newly-focused pane (if any),
    /// so focusing a previously-throttled pane never shows stale
    /// content. Send `None` when nothing is focused (e.g. an empty
    /// workspace) and again whenever the focused leaf's binding
    /// changes -- a stale value only ever makes throttling slightly
    /// less effective, never wrong, so there's no need to resend on
    /// every keystroke, just on every *binding* change.
    SetFocus {
        server_pane: Option<ServerPaneId>,
    },
    Unsubscribe {
        workspace: WorkspaceId,
    },
    /// Report the current on-screen size of a client-pane (e.g. after a
    /// frontend terminal resize or a split-ratio change), so the daemon
    /// can recompute its bound server-pane's PTY size.
    ResizeClientPane {
        pane: ClientPaneId,
        size: Size,
    },
    /// Scroll `pane`'s bound server-pane's view back into (positive
    /// `delta`) or forward out of (negative `delta`) scrollback
    /// history, from this connection's own point of view. Addressed by
    /// client-pane, matching `Input`/`ResizeClientPane`'s convention,
    /// but the daemon resolves it to the bound server-pane and stores
    /// the resulting offset keyed by `(this connection, that
    /// server-pane)` -- NOT by `pane` itself. See design doc "Scroll
    /// offset ownership" for why: `GridSnapshot` carries only a
    /// `server_pane` id, so a connection can only ever see one grid per
    /// server-pane at a time regardless of how many client-panes it has
    /// bound -- offset has to live at that same granularity to be
    /// deliverable at all. The daemon clamps the resulting offset to
    /// `0..=` that server-pane's available scrollback server-side; the
    /// frontend never computes or owns the authoritative value. A
    /// `pane` that's currently unbound is a silent no-op (`Ack`, not
    /// `Error` -- an accidental mouse-wheel event over an unbound
    /// placeholder is a very plausible occurrence, not a client bug
    /// worth surfacing as an error).
    ScrollClientPane {
        pane: ClientPaneId,
        delta: i32,
    },
    /// Set a split's ratio directly (mouse-drag resizing). `new_ratio` is
    /// the fraction of space given to the split's `a` side; the daemon
    /// clamps it (see `SplitTree::resize_split`) so a drag can shrink a
    /// pane but never collapse it to zero.
    ResizeSplit {
        workspace: String,
        split: SplitId,
        new_ratio: f32,
    },
    /// Route raw input bytes (keystrokes) to the server-pane bound to
    /// `pane`.
    Input {
        pane: ClientPaneId,
        bytes: Vec<u8>,
    },
    /// Read a server-pane's current on-screen contents directly, with no
    /// workspace or client-pane involved -- for CLI/scripting callers
    /// (e.g. a Claude Skill) that just want to see what a pane is
    /// showing right now. `target` uses the same name-or-id addressing
    /// as `ServerKill`/`ServerRename`.
    ServerRead {
        target: String,
    },
    /// Type into a server-pane directly, with no workspace or
    /// client-pane involved -- the `ServerRead` counterpart for sending
    /// input. `enter` appends a trailing `\n` after `text`, matching a
    /// user typing a line and pressing Enter.
    ServerSend {
        target: String,
        text: String,
        enter: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Ack,
    Error {
        message: String,
    },
    ServerPane(ServerPaneInfo),
    ServerPaneList(Vec<ServerPaneInfo>),
    /// Reply to `PinnedDirsList` (and to `ToggleDirectoryPin`, so a
    /// caller can update its local grouping from the same round trip
    /// that changed it, with no separate re-fetch needed): the current
    /// pin order, earliest-pinned first.
    PinnedDirsList(Vec<String>),
    /// Reply to `ConsumeShellFallback`.
    ShellFallback {
        available: bool,
    },
    ClientPaneCreated {
        workspace: WorkspaceId,
        pane: ClientPaneId,
    },
    ClientPaneList {
        workspace: WorkspaceId,
        panes: Vec<ClientPane>,
    },
    /// Full state handed back on `Subscribe`: current layout plus a grid
    /// snapshot for every server-pane bound within the workspace.
    Snapshot {
        workspace: WorkspaceInfo,
        grids: Vec<GridSnapshot>,
    },
    /// Reply to `ServerRead`: the pane's current screen, rendered as
    /// plain text -- one line per row, trailing blank rows trimmed,
    /// styling dropped (a script wants the characters on screen, not
    /// SGR codes).
    ServerReadOutput {
        text: String,
    },
}

/// Pushed to subscribed connections outside the request/response cycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    LayoutDelta {
        workspace: WorkspaceId,
        tree: Option<SplitTree>,
    },
    GridDelta {
        snapshot: GridSnapshot,
    },
    ServerPaneDied {
        server_pane: ServerPaneId,
    },
}

/// Frames sent daemon -> client. Distinguishes a reply to a specific
/// request from an unsolicited pushed event, since both can interleave
/// once a connection has subscribed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMessage {
    Response(Response),
    Event(Event),
}

/// Resolve the Unix socket path the daemon listens on and clients
/// connect to. Prefers `$XDG_RUNTIME_DIR`, falling back to a
/// per-user path under `/tmp` (mirrors tmux's own fallback behavior).
pub fn socket_path() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("dimax.sock");
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
    PathBuf::from(format!("/tmp/dimax-{user}.sock"))
}

/// Length-prefixed JSON framing used for every message on the socket:
/// a little-endian `u32` byte length followed by that many bytes of
/// UTF-8 JSON. Shared by daemon, TUI, and CLI so all three agree on the
/// wire format without duplicating it.
pub mod framing {
    use anyhow::Result;
    use serde::{Serialize, de::DeserializeOwned};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    pub async fn write_frame<W, T>(w: &mut W, msg: &T) -> Result<()>
    where
        W: AsyncWriteExt + Unpin,
        T: Serialize,
    {
        let body = serde_json::to_vec(msg)?;
        let len = u32::try_from(body.len())?;
        w.write_all(&len.to_le_bytes()).await?;
        w.write_all(&body).await?;
        Ok(())
    }

    pub async fn read_frame<R, T>(r: &mut R) -> Result<T>
    where
        R: AsyncReadExt + Unpin,
        T: DeserializeOwned,
    {
        let mut len_buf = [0u8; 4];
        r.read_exact(&mut len_buf).await?;
        let len = u32::from_le_bytes(len_buf) as usize;
        let mut body = vec![0u8; len];
        r.read_exact(&mut body).await?;
        Ok(serde_json::from_slice(&body)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(id: ClientPaneId) -> ClientPane {
        ClientPane {
            id,
            name: None,
            tabs: vec![],
            active_tab: 0,
            short_id: "aa".to_string(),
        }
    }

    #[test]
    fn split_then_find_both_leaves() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let mut tree = SplitTree::Leaf(pane(a));
        tree.split_leaf(a, SplitDir::Vertical, pane(b)).unwrap();
        assert!(tree.find(a).is_some());
        assert!(tree.find(b).is_some());
        assert_eq!(tree.leaves().len(), 2);
    }

    #[test]
    fn split_missing_target_errors() {
        let a = Uuid::new_v4();
        let mut tree = SplitTree::Leaf(pane(a));
        let missing = Uuid::new_v4();
        assert!(
            tree.split_leaf(missing, SplitDir::Horizontal, pane(Uuid::new_v4()))
                .is_err()
        );
    }

    #[test]
    fn remove_leaf_collapses_split() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let mut tree = SplitTree::Leaf(pane(a));
        tree.split_leaf(a, SplitDir::Horizontal, pane(b)).unwrap();
        let remaining = tree.remove_leaf(a).unwrap();
        match remaining {
            Some(SplitTree::Leaf(p)) => assert_eq!(p.id, b),
            other => panic!("expected single leaf b, got {other:?}"),
        }
    }

    #[test]
    fn remove_last_leaf_empties_tree() {
        let a = Uuid::new_v4();
        let tree = SplitTree::Leaf(pane(a));
        assert!(tree.remove_leaf(a).unwrap().is_none());
    }

    #[test]
    fn resize_split_updates_ratio_and_clamps() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let mut tree = SplitTree::Leaf(pane(a));
        tree.split_leaf(a, SplitDir::Vertical, pane(b)).unwrap();
        let split_id = match &tree {
            SplitTree::Split { id, .. } => *id,
            _ => unreachable!(),
        };

        tree.resize_split(split_id, 0.3).unwrap();
        match &tree {
            SplitTree::Split { ratio, .. } => assert_eq!(*ratio, 0.3),
            _ => unreachable!(),
        }

        // Clamped, not rejected: a drag pushing past the limit shrinks
        // the pane to the minimum rather than erroring or vanishing it.
        tree.resize_split(split_id, 0.0).unwrap();
        match &tree {
            SplitTree::Split { ratio, .. } => assert_eq!(*ratio, 0.05),
            _ => unreachable!(),
        }
        tree.resize_split(split_id, 1.0).unwrap();
        match &tree {
            SplitTree::Split { ratio, .. } => assert_eq!(*ratio, 0.95),
            _ => unreachable!(),
        }
    }

    #[test]
    fn resize_split_unknown_id_errors() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let mut tree = SplitTree::Leaf(pane(a));
        tree.split_leaf(a, SplitDir::Vertical, pane(b)).unwrap();
        assert!(tree.resize_split(Uuid::new_v4(), 0.3).is_err());
    }

    #[tokio::test]
    async fn frame_roundtrip() {
        let mut buf = Vec::new();
        let req = Request::ServerList;
        framing::write_frame(&mut buf, &req).await.unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let decoded: Request = framing::read_frame(&mut cursor).await.unwrap();
        matches!(decoded, Request::ServerList);
    }
}
