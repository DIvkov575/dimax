//! The `dimux attach` frontend: connects to the daemon, subscribes to a
//! workspace, and renders it with `ratatui`. See design doc "Default
//! keybinds (TUI)" and "Protocol / Subscription model".
//!
//! Module split:
//! - `keys` — parses raw input bytes (including Kitty-forwarded Cmd-chord
//!   escape sequences) into an [`Action`], independent of rendering.
//! - `render` — turns the current workspace snapshot + grids into a
//!   `ratatui::Frame`, independent of networking.
//! - `mod.rs` (here) — the event loop tying input, network, and render
//!   together.
//!
//! # Design decisions worth flagging for the next agent
//!
//! - **Raw stdin reads, not crossterm's `KeyEvent`s.** `keys::parse`
//!   consumes raw bytes (the exact Kitty `send_text` escape payloads), one
//!   already-delimited event per call. crossterm's event abstraction
//!   parses those bytes into `KeyEvent`s *for you*, which would throw away
//!   the very bytes `keys::parse` needs and require reconstructing them
//!   (fragile, lossy). So this module reads raw bytes directly off
//!   `tokio::io::stdin()` instead of using `crossterm::event::{read,
//!   EventStream}` at all. `ratatui`'s `CrosstermBackend` still owns
//!   *stdout* for rendering (independent concern) and raw mode is still
//!   enabled via `ratatui::try_init()` — that's what makes a single
//!   `read()` call here return one keystroke or one complete escape
//!   sequence at a time rather than a line-buffered chunk, satisfying
//!   `keys::parse`'s "one complete event per read" contract in the common
//!   case (Kitty writes each chord as a single `write(2)`, and a local
//!   pty read after raw-mode setup returns whatever's currently queued).
//!   Known limitation: under heavy load a single chord's bytes could in
//!   principle be split across two reads, which would resolve to two
//!   `PassThrough`s instead of one chord — no reassembly buffer is
//!   implemented for this; not expected to matter at interactive typing
//!   speeds.
//! - **Because raw bytes are read directly, crossterm's `event-stream`
//!   Cargo feature is never needed.** (Verified: it is not enabled by
//!   `ratatui`'s default features via `ratatui-crossterm`, so
//!   `crossterm::event::EventStream` is not actually callable through this
//!   dependency graph as configured — confirmed by a scratch build.) Had
//!   this module needed parsed `KeyEvent`s, the fallback would have been
//!   `crossterm::event::poll` in a blocking task bridged through a
//!   channel, per this task's constraints (no `Cargo.toml` edits). Reading
//!   raw bytes sidesteps that fallback entirely.
//! - **Quit key: `Ctrl-Q` (byte `0x11`), not `Ctrl-C`.** The design doc's
//!   keybind table has no quit binding at all — every other chord is a
//!   Kitty-forwarded Cmd-sequence, and there was never a "how does the
//!   user leave `dimux attach`" decision made. `Ctrl-C` was deliberately
//!   *not* picked: it needs to `PassThrough` to whatever's running in the
//!   focused pane (interrupting a stuck program is one of the most common
//!   reasons to press it), and stealing it globally would make that
//!   impossible. `Ctrl-Q` is rarely bound by interactive programs and (in
//!   raw mode, with software flow control not in play) is just an inert
//!   byte otherwise. This is a real product decision the design doc
//!   didn't make — flagged here for it to be revisited/formalized
//!   (e.g. promoted to a proper Kitty-forwarded chord) rather than staying
//!   a hardcoded raw byte.
//! - **Focus movement (`FocusLeft/Right/Up/Down`) is leaf-cycling, not
//!   2D adjacency.** `SplitTree` stores only `dir`/`ratio` per split, no
//!   rendered rect — so "the pane geometrically to the left" isn't
//!   computable without that information. `cycle_focus` below instead
//!   walks `leaves()` in tree order and treats Left/Up as "previous leaf"
//!   and Right/Down as "next leaf" (wrapping). This is an intentionally
//!   honest v1 stand-in, not a geometrically correct implementation; doing
//!   the real thing needs the render layer to hand back computed rects
//!   per leaf, which it doesn't currently do.
//! - **Requests are sent through an `Event`-tolerant helper
//!   (`App::request`), not `Client::request`.** Once subscribed, the
//!   daemon can push an `Event` frame at any point on the same connection,
//!   including *between* this client writing a request and reading back
//!   its response (see `daemon/mod.rs`'s `broadcast_layout`/
//!   `broadcast_grid`, called from inside `dispatch` before the response
//!   is written, racing the connection's separate push-task). Reusing
//!   `Client::request` here would hit its "protocol violation: got Event
//!   before Response" bail-out. `App::request` instead loops: read one
//!   `ServerMessage`, apply it via `App::apply_event` if it's an `Event`,
//!   keep going until a `Response` arrives.
//! - **Frame reads never happen directly inside `run`'s `tokio::select!`
//!   — they go through [`FrameReader`], a background task + channel.**
//!   Root cause of the "dimux hangs often" reports: `run`'s loop used to
//!   race `stdin.read` against `protocol::framing::read_frame` on the
//!   socket directly in the same `select!`. `read_frame` is built on
//!   `AsyncReadExt::read_exact`, which tokio documents as NOT
//!   cancellation-safe — if that branch lost the race after
//!   `read_exact` had already pulled some (but not all) of a frame's
//!   bytes off the socket, those bytes were gone for good (dropping a
//!   future doesn't hand consumed bytes back to the stream). The next
//!   `read_frame` call then read a stray fragment as a fresh length
//!   prefix, permanently desyncing the length-prefixed protocol: every
//!   later read either errored on garbage or — the common case — blocked
//!   forever waiting for a bogus byte count that would never arrive,
//!   which is exactly "frozen, no response to any key" with every thread
//!   parked at 0% CPU (confirmed via `sample`, and reproduced directly in
//!   a scratch harness: racing `read_frame` against a fast timer inside
//!   `select!` while a frame's body arrived in several small writes lost
//!   100% of frames and left the reader hung on a bogus length; the same
//!   harness with no racing branch received every frame cleanly). Any
//!   frame the daemon writes in more than one syscall's worth of bytes —
//!   routine under real scheduling, not a contrived edge case — while the
//!   user is also typing can trigger this. `FrameReader` fixes it by
//!   giving `read_frame` a dedicated task that only ever awaits it one
//!   call at a time, never raced against anything, so `read_exact` always
//!   runs to completion; the main loop instead races `FrameReader::next`,
//!   which reads from an `mpsc` channel — cancellation-safe by tokio's own
//!   contract, so losing that race never drops a frame.
//! - **Mouse-drag divider resizing is drag-only, no keybind exists for
//!   it** (per user direction — dragging is the intended interaction,
//!   not a chord). Mouse input arrives on the same stdin stream as
//!   everything else, as an SGR escape sequence
//!   (`ESC [ < Cb ; Cx ; Cy M/m`); `mouse::parse` hand-parses just that
//!   format rather than using crossterm's mouse events, for the same
//!   reason raw bytes are read directly at all (see above) — crossterm's
//!   `parse_event` is `pub(crate)`, not callable from here regardless.
//!   `App::handle_mouse` hit-tests `Down` against `render::divider_rects`
//!   (recomputed fresh every call — the tree can change between mouse
//!   events). **A previous version of this sent `Request::ResizeSplit` on
//!   every single `Drag` event** — dozens of requests per second at
//!   typical drag speed, each awaited synchronously before the next
//!   stdin read, badly enough to stall the whole session. `Drag` now only
//!   updates `dragging_split`'s ratio locally (no request); `run`'s draw
//!   closure renders a workspace clone with that ratio patched in via
//!   `SplitTree::resize_split`, so the drag still looks smooth. Exactly
//!   one `Request::ResizeSplit` is sent, on `Up`, committing the final
//!   position — other frontends see the resize only once it's released,
//!   not live throughout the drag.

pub mod keys;
pub mod mouse;
pub mod render;

use crate::cli::Client;
use crate::protocol::{
    self, ClientPaneId, Event, GridSnapshot, Request, Response, ServerMessage, ServerPaneId,
    ServerPaneInfo, SplitDir, SplitTree, WorkspaceInfo,
};
use std::collections::HashMap;
use tokio::io::AsyncReadExt;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;

/// Owns the socket's read half in a dedicated background task, one
/// `read_frame` call at a time, and forwards each parsed [`ServerMessage`]
/// over an `mpsc` channel. See module doc "Frame reads never happen
/// directly inside `run`'s `tokio::select!`" for why this exists: the
/// channel's `recv` is cancellation-safe, so callers (both `App::request`
/// and `run`'s main loop) can freely race [`FrameReader::next`] inside a
/// `select!` without risking the desync that racing `read_frame` itself
/// caused.
struct FrameReader {
    rx: mpsc::Receiver<anyhow::Result<ServerMessage>>,
}

impl FrameReader {
    /// Spawn the background task and return the receiving half. The task
    /// runs until the channel closes (every `FrameReader` dropped) or the
    /// socket errors/closes, at which point it sends that final `Err` and
    /// exits — `next`'s `None` case below only fires after that Err has
    /// already been delivered, so callers always see the error rather
    /// than a silent channel closure.
    fn spawn(mut read_half: OwnedReadHalf) -> Self {
        let (tx, rx) = mpsc::channel(32);
        tokio::spawn(async move {
            loop {
                let frame = protocol::framing::read_frame::<_, ServerMessage>(&mut read_half).await;
                let is_err = frame.is_err();
                if tx.send(frame).await.is_err() || is_err {
                    break;
                }
            }
        });
        Self { rx }
    }

    /// Await the next frame. Safe to use as a `tokio::select!` branch:
    /// losing the race only drops this poll, never a byte already read
    /// off the socket, since the background task (not this call) owns the
    /// actual `read_frame` invocation.
    async fn next(&mut self) -> anyhow::Result<ServerMessage> {
        self.rx.recv().await.ok_or_else(|| anyhow::anyhow!("connection closed"))?
    }
}

/// User-facing actions a keypress can resolve to, decoupled from the
/// specific chord that produced it so `render`/the event loop don't need
/// to know about Kitty escape sequences.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    SwitchWorkspace(u8),
    SplitVertical,
    SplitHorizontal,
    CloseFocusedPane,
    KillFocusedServerPane,
    /// `cmd-shift-z`: detach the focused client-pane from its current
    /// server-pane (which keeps running) and open the attach menu to pick
    /// its replacement.
    DetachAndAttach,
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    /// Not a dimux chord — forward these raw bytes to the focused
    /// client-pane's bound server-pane as keyboard input.
    PassThrough,
}

/// State for the `cmd-shift-z` attach menu: pick an existing server-pane,
/// or spawn a new one, to bind into the client-pane that was just
/// detached — or, since this menu doubles as a lightweight server-pane
/// manager, delete/rename one instead. `servers.len()` is always the
/// trailing "spawn new" row's index (mirrors the removed picker's
/// convention — see `render::draw_attach_menu`).
struct AttachMenu {
    /// `(cwd_group_key, server)` pairs, pre-sorted into cwd-bucket order
    /// by `group_servers_by_cwd` every time this field is populated —
    /// see that function's doc comment for the exact ordering rules.
    /// `selected` indexes this `Vec` directly; grouping is a rendering
    /// concern layered on top by `render::draw_attach_menu`, not a
    /// change to how selection/nav math works.
    servers: Vec<(String, ServerPaneInfo)>,
    selected: usize,
    /// `Some(index into servers)` while that row's delete is armed
    /// (first `x` pressed, awaiting a confirming `x`/Enter or a
    /// cancelling any-other-key). Mutually exclusive with `rename` —
    /// opening one clears the other.
    pending_delete: Option<usize>,
    /// `Some` while the inline rename field is focused for the row at
    /// `.index`. See `RenameState`'s own doc comment.
    rename: Option<RenameState>,
}

/// Live edit state for the attach menu's inline rename field (`r` on a
/// row). `text`/`cursor` are the field's edit buffer and cursor
/// position — a byte offset into `text`, always kept on a UTF-8 char
/// boundary by every editing operation in `apply_rename_edit`. `error`
/// holds the daemon's last rejection message (e.g. a name collision) to
/// render under the field; cleared on the next edit so a stale error
/// doesn't linger once the user starts fixing it.
struct RenameState {
    index: usize,
    text: String,
    cursor: usize,
    error: Option<String>,
}

/// Raw byte, read outside `keys::parse`'s chord grammar entirely, that
/// exits `dimux attach`. See module doc "Quit key" for why this isn't
/// `Ctrl-C`.
const QUIT_BYTE: u8 = 0x11; // Ctrl-Q

fn is_quit(bytes: &[u8]) -> bool {
    bytes == [QUIT_BYTE]
}

/// All mutable state the event loop owns, per module doc "Local mutable
/// state" in the task this module implements. Kept as one struct (rather
/// than loose locals) so every state-mutating helper below can take
/// `&mut self` instead of five separate `&mut` parameters.
struct App {
    workspace: WorkspaceInfo,
    grids: HashMap<ServerPaneId, GridSnapshot>,
    focused: Option<ClientPaneId>,
    /// `Some` while the `cmd-shift-z` attach menu is open; input routes to
    /// `handle_attach_menu_input` instead of the normal keymap while set.
    attach_menu: Option<AttachMenu>,
    /// The full terminal area as of the most recent `terminal.draw` call
    /// -- refreshed every frame in `run`'s loop, since it's what
    /// `render::divider_rects` needs to recompute hit zones against the
    /// current layout on every mouse event (dividers move every frame the
    /// tree does, so this can't be cached across ticks).
    frame_area: ratatui::layout::Rect,
    /// `Some((split, live_ratio))` while a mouse-down on a divider's grab
    /// zone is currently held (a drag in progress); cleared on mouse-up.
    /// Reset to `None` on any workspace switch too, since the split it
    /// names may no longer exist in whatever workspace is now active.
    ///
    /// `live_ratio` tracks the drag's current position locally, updated
    /// on every `Drag` event with *no* network round-trip — only on
    /// `Up` does `handle_mouse` send the one `Request::ResizeSplit` that
    /// actually commits it. This is deliberate: sending a resize request
    /// per mouse-move (the original design) flooded the daemon with
    /// dozens of requests per second during a drag, each one blocking the
    /// event loop's next stdin read until its response came back, badly
    /// enough to stall the whole session. Dragging still feels smooth
    /// because `run`'s draw closure renders a workspace clone with
    /// `live_ratio` patched in via `SplitTree::resize_split`, reflecting
    /// the drag immediately without touching daemon state.
    dragging_split: Option<(crate::protocol::SplitId, f32)>,
}

impl App {
    /// Send the initial `Subscribe` and build the starting `App` state
    /// from its `Snapshot` response. Free function (not `&mut self`)
    /// because there is no `App` yet — this is what constructs the first
    /// one, per design doc "Subscription model": "On attach ... the
    /// frontend sends `Subscribe(workspace_id)`."
    async fn bootstrap(
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
        workspace: &str,
    ) -> anyhow::Result<Self> {
        protocol::framing::write_frame(
            write_half,
            &Request::Subscribe { workspace: workspace.to_string() },
        )
        .await?;
        loop {
            match reader.next().await? {
                ServerMessage::Response(Response::Snapshot { workspace, grids }) => {
                    let focused = first_leaf(&workspace);
                    return Ok(App {
                        workspace,
                        grids: grids.into_iter().map(|g| (g.server_pane, g)).collect(),
                        focused,
                        attach_menu: None,
                        frame_area: ratatui::layout::Rect::default(),
                        dragging_split: None,
                    });
                }
                ServerMessage::Response(other) => {
                    anyhow::bail!("unexpected response to initial Subscribe: {other:?}")
                }
                // No subscription is active yet at this point in startup,
                // so a pushed Event genuinely shouldn't arrive -- but
                // tolerate it rather than treat it as a protocol error.
                ServerMessage::Event(_) => continue,
            }
        }
    }

    /// Send one request, tolerating any number of pushed `Event`s
    /// arriving before the matching `Response` (see module doc). Every
    /// outgoing request in this module goes through this instead of
    /// `Client::request`.
    async fn request(
        &mut self,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
        req: Request,
    ) -> anyhow::Result<Response> {
        protocol::framing::write_frame(write_half, &req).await?;
        loop {
            match reader.next().await? {
                ServerMessage::Response(r) => return Ok(r),
                ServerMessage::Event(e) => self.apply_event(e),
            }
        }
    }

    /// Apply one pushed `Event` to local state. Design doc "Subscription
    /// model": `LayoutDelta`/`GridDelta` are what streams while
    /// subscribed; `ServerPaneDied` isn't itself a layout change (the
    /// client-pane stays bound, per design doc "Error handling" —
    /// "switches to an 'unbound' placeholder" is `ServerKill`'s effect,
    /// not death-while-displayed's) but its grid should stop showing
    /// stale content, hence the `grids.remove` — matches render.rs's
    /// "server-pane closed" placeholder, which triggers exactly when a
    /// bound pane's id is missing from `grids`.
    fn apply_event(&mut self, event: Event) {
        match event {
            Event::LayoutDelta { workspace, tree } => {
                if workspace == self.workspace.id {
                    self.workspace.tree = tree;
                    self.reconcile_focus();
                }
            }
            Event::GridDelta { snapshot } => {
                self.grids.insert(snapshot.server_pane, snapshot);
            }
            Event::ServerPaneDied { server_pane } => {
                self.grids.remove(&server_pane);
            }
        }
    }

    /// Re-point `focused` at a remaining leaf whenever it no longer names
    /// one in the current tree (pane closed locally, or by any other
    /// frontend/CLI caller live-editing the same workspace). Centralizing
    /// this here (called from every `LayoutDelta`) means every action
    /// that can shrink the tree — `CloseFocusedPane` locally, or a CLI
    /// `client close` from elsewhere — gets correct focus reassignment
    /// for free instead of needing its own bespoke recovery logic.
    fn reconcile_focus(&mut self) {
        let still_valid = self
            .focused
            .is_some_and(|id| self.workspace.tree.as_ref().is_some_and(|t| t.find(id).is_some()));
        if !still_valid {
            self.focused = first_leaf(&self.workspace);
        }
    }

    async fn switch_workspace(
        &mut self,
        n: u8,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        let old = self.workspace.id;
        let _ = self.request(write_half, reader, Request::Unsubscribe { workspace: old }).await?;
        let resp = self
            .request(write_half, reader, Request::Subscribe { workspace: n.to_string() })
            .await?;
        if let Response::Snapshot { workspace, grids } = resp {
            self.focused = first_leaf(&workspace);
            self.workspace = workspace;
            self.grids = grids.into_iter().map(|g| (g.server_pane, g)).collect();
            self.attach_menu = None;
            self.dragging_split = None;
        }
        Ok(())
    }

    /// `cmd-d`/`cmd-shift-d`: split the focused pane and immediately bind
    /// a freshly spawned server-pane into the new half — no menu. (The
    /// design doc originally specified a picker here; the picker was
    /// removed entirely in favor of this direct "split + new shell"
    /// shortcut. Binding a client-pane to an *existing* server-pane, e.g.
    /// to display one shell in two places, is still possible via the CLI:
    /// `dimux client bind <workspace>/<pane> <server-name>`.)
    ///
    /// `split_of: self.focused` splits the focused pane if there is one;
    /// if the workspace is empty (`self.focused` is `None`), `ClientSpawn`
    /// creates the workspace's sole leaf instead (no split), so this is
    /// also the entry point for spawning the very first pane.
    async fn split(
        &mut self,
        dir: SplitDir,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        let Response::ServerPane(server) = self
            .request(write_half, reader, Request::ServerSpawn { name: None, cmd: None })
            .await?
        else {
            return Ok(());
        };
        let req = Request::ClientSpawn {
            workspace: self.workspace.id.to_string(),
            split_of: self.focused,
            dir: Some(dir),
            bind: Some(server.id.to_string()),
        };
        if let Response::ClientPaneCreated { pane, .. } = self.request(write_half, reader, req).await? {
            self.focused = Some(pane);
        }
        Ok(())
    }

    /// `cmd-w`: close the focused client-pane. Its bound server-pane
    /// keeps running (design doc "CLI surface" / "Default keybinds").
    /// Focus reassignment happens via `reconcile_focus` once the
    /// resulting `LayoutDelta` lands (immediately, if it interleaved
    /// ahead of this request's `Response`; on the next loop iteration
    /// otherwise) rather than being computed here from a tree this
    /// function doesn't have an up-to-date copy of yet.
    async fn close_focused(
        &mut self,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        let Some(pane) = self.focused else { return Ok(()) };
        let req = Request::ClientClose { workspace: self.workspace.id.to_string(), pane };
        let _ = self.request(write_half, reader, req).await?;
        Ok(())
    }

    /// `cmd-shift-w`: kill the server-pane bound to the focused
    /// client-pane, if any.
    async fn kill_focused_server_pane(
        &mut self,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        let Some(pane) = self.focused else { return Ok(()) };
        let Some(bound) = self.workspace.tree.as_ref().and_then(|t| t.find(pane)).and_then(|p| p.bound)
        else {
            return Ok(());
        };
        let req = Request::ServerKill { target: bound.to_string() };
        let _ = self.request(write_half, reader, req).await?;
        Ok(())
    }

    /// `cmd-shift-z`: detach the focused client-pane from its current
    /// server-pane (which keeps running — see `state::client_unbind`) and
    /// open the attach menu so a replacement can be picked. A no-op if
    /// nothing is focused (empty workspace).
    async fn detach_and_open_menu(
        &mut self,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        let Some(pane) = self.focused else { return Ok(()) };
        let req = Request::ClientUnbind { workspace: self.workspace.id.to_string(), pane };
        let _ = self.request(write_half, reader, req).await?;
        if let Response::ServerPaneList(servers) =
            self.request(write_half, reader, Request::ServerList).await?
        {
            self.attach_menu = Some(AttachMenu {
                servers: group_servers_by_cwd(servers),
                selected: 0,
                pending_delete: None,
                rename: None,
            });
        }
        Ok(())
    }

    /// Route one raw byte chunk while the attach menu is open. A distinct,
    /// simpler modal grammar than the main keymap (same approach the
    /// removed picker used) rather than reusing `keys::parse`'s chords.
    async fn handle_attach_menu_input(
        &mut self,
        bytes: &[u8],
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        let has_pending_delete =
            self.attach_menu.as_ref().is_some_and(|m| m.pending_delete.is_some());
        if has_pending_delete {
            match bytes {
                b"x" | b"\r" | b"\n" => self.confirm_delete(write_half, reader).await?,
                _ => {
                    if let Some(menu) = &mut self.attach_menu {
                        menu.pending_delete = None;
                    }
                    // A cancelling keystroke isn't swallowed -- e.g. `j`
                    // both cancels the pending delete *and* moves the
                    // selection down, in one keypress. Fall through to
                    // normal dispatch for this same byte.
                    self.dispatch_attach_menu_action(
                        parse_attach_menu_input(bytes),
                        write_half,
                        reader,
                    )
                    .await?;
                }
            }
            return Ok(());
        }
        self.dispatch_attach_menu_action(parse_attach_menu_input(bytes), write_half, reader).await
    }

    /// Browse-mode dispatch: everything `handle_attach_menu_input` routes
    /// to once neither a pending delete nor an active rename is
    /// intercepting input first. Split out so the pending-delete
    /// cancel-and-fall-through path (above) can re-dispatch the same
    /// byte's `AttachMenuAction` without duplicating this match.
    async fn dispatch_attach_menu_action(
        &mut self,
        action: AttachMenuAction,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        match action {
            AttachMenuAction::Up => self.move_attach_menu_selection(false),
            AttachMenuAction::Down => self.move_attach_menu_selection(true),
            AttachMenuAction::Cancel => self.attach_menu = None,
            AttachMenuAction::Confirm => self.confirm_attach_menu(write_half, reader).await?,
            AttachMenuAction::Delete => self.arm_delete(),
            AttachMenuAction::StartRename => self.start_rename(),
            AttachMenuAction::Ignore => {}
        }
        Ok(())
    }

    fn move_attach_menu_selection(&mut self, forward: bool) {
        let Some(menu) = &mut self.attach_menu else { return };
        // +1 for the trailing "spawn new" row, same convention the
        // removed picker used (see `render::draw_attach_menu`).
        let len = menu.servers.len() + 1;
        menu.selected = if forward { (menu.selected + 1) % len } else { (menu.selected + len - 1) % len };
    }

    /// Arm the selected row's deletion (first `x`), a no-op on the
    /// trailing "spawn new" row (nothing to delete there).
    fn arm_delete(&mut self) {
        let Some(menu) = &mut self.attach_menu else { return };
        if menu.selected < menu.servers.len() {
            menu.pending_delete = Some(menu.selected);
        }
    }

    /// Confirm a previously-armed deletion: kill the server-pane, then
    /// re-fetch and re-group the server list so the menu reflects its
    /// removal. Clamps `selected` into the shrunk list -- if the deleted
    /// row was the last one, selection moves to the new last row;
    /// otherwise the numeric index is left as-is (which now names
    /// whatever slid up into that slot, an acceptable "selection moved
    /// on" side effect of any list shrinking under the cursor).
    async fn confirm_delete(
        &mut self,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        let Some(menu) = &mut self.attach_menu else { return Ok(()) };
        let Some(index) = menu.pending_delete.take() else { return Ok(()) };
        let target = menu.servers[index].1.id.to_string();
        let _ = self.request(write_half, reader, Request::ServerKill { target }).await?;
        if let Response::ServerPaneList(servers) =
            self.request(write_half, reader, Request::ServerList).await?
        {
            let grouped = group_servers_by_cwd(servers);
            let Some(menu) = &mut self.attach_menu else { return Ok(()) };
            menu.servers = grouped;
            let spawn_index = menu.servers.len();
            if menu.selected > spawn_index {
                menu.selected = spawn_index;
            }
        }
        Ok(())
    }

    fn start_rename(&mut self) {
        // Implemented in Task 5.
    }

    /// Confirm the attach menu's current selection: bind the just-detached
    /// client-pane to the selected server-pane, spawning a fresh one first
    /// if the trailing "spawn new" row is selected.
    async fn confirm_attach_menu(
        &mut self,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        let Some(menu) = self.attach_menu.take() else { return Ok(()) };
        let Some(pane) = self.focused else { return Ok(()) };
        let spawn_new_index = menu.servers.len();
        let target = if menu.selected == spawn_new_index {
            match self
                .request(write_half, reader, Request::ServerSpawn { name: None, cmd: None })
                .await?
            {
                Response::ServerPane(info) => info.id.to_string(),
                _ => return Ok(()),
            }
        } else {
            menu.servers[menu.selected].1.id.to_string()
        };
        let req = Request::ClientBind { workspace: self.workspace.id.to_string(), pane, target };
        let _ = self.request(write_half, reader, req).await?;
        Ok(())
    }

    fn move_focus(&mut self, forward: bool) {
        if let Some(tree) = &self.workspace.tree {
            self.focused = cycle_focus(tree, self.focused, forward);
        }
    }

    /// Route one parsed [`mouse::MouseEvent`] — divider drag-resizing, per
    /// user direction: no keybind, drag only. `Down` hit-tests against the
    /// current layout's divider grab zones (recomputed fresh every call
    /// via `render::divider_rects`, since the layout can change between
    /// mouse events). `Drag` updates `dragging_split`'s live ratio purely
    /// locally — no request sent — so the drag renders smoothly without
    /// flooding the daemon (see the field doc on `dragging_split` for why
    /// this replaced the original "send on every move" design). `Up`
    /// commits the final position with exactly one `Request::ResizeSplit`
    /// and clears the drag. A `Drag`/`Up` with no split currently held is
    /// a plain mouse move with no button pressed on this pane (or a drag
    /// that started outside any divider) and is ignored.
    async fn handle_mouse(
        &mut self,
        event: mouse::MouseEvent,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        match event {
            mouse::MouseEvent::Down { col, row } => {
                let Some(tree) = &self.workspace.tree else { return Ok(()) };
                let hits = render::divider_rects(tree, self.frame_area);
                self.dragging_split = hit_test_dividers(&hits, col, row).map(|split| {
                    let hit = hits.iter().find(|h| h.split == split).expect(
                        "hit_test_dividers only returns a split id that came from this exact `hits` list",
                    );
                    (split, render::ratio_at(hit, col, row))
                });
            }
            mouse::MouseEvent::Drag { col, row } => {
                let Some((split, _)) = self.dragging_split else { return Ok(()) };
                let Some(tree) = &self.workspace.tree else { return Ok(()) };
                let Some(hit) = render::divider_rects(tree, self.frame_area)
                    .into_iter()
                    .find(|hit| hit.split == split)
                else {
                    // The split disappeared from under the drag (e.g.
                    // closed by another frontend mid-drag) -- nothing
                    // left to resize.
                    self.dragging_split = None;
                    return Ok(());
                };
                self.dragging_split = Some((split, render::ratio_at(&hit, col, row)));
            }
            mouse::MouseEvent::Up { .. } => {
                let Some((split, ratio)) = self.dragging_split.take() else { return Ok(()) };
                let req = Request::ResizeSplit {
                    workspace: self.workspace.id.to_string(),
                    split,
                    new_ratio: ratio,
                };
                let _ = self.request(write_half, reader, req).await?;
            }
        }
        Ok(())
    }

    /// Route one resolved `Action` to its effect.
    async fn handle_action(
        &mut self,
        action: Action,
        raw: &[u8],
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        match action {
            Action::SwitchWorkspace(n) => self.switch_workspace(n, write_half, reader).await?,
            Action::SplitVertical => self.split(SplitDir::Vertical, write_half, reader).await?,
            Action::SplitHorizontal => self.split(SplitDir::Horizontal, write_half, reader).await?,
            Action::CloseFocusedPane => self.close_focused(write_half, reader).await?,
            Action::KillFocusedServerPane => {
                self.kill_focused_server_pane(write_half, reader).await?
            }
            Action::DetachAndAttach => self.detach_and_open_menu(write_half, reader).await?,
            Action::FocusLeft | Action::FocusUp => self.move_focus(false),
            Action::FocusRight | Action::FocusDown => self.move_focus(true),
            Action::PassThrough => {
                if let Some(pane) = self.focused {
                    let req = Request::Input { pane, bytes: raw.to_vec() };
                    let _ = self.request(write_half, reader, req).await?;
                }
            }
        }
        Ok(())
    }
}

/// First leaf (tree order) of `workspace`'s tree, or `None` if the
/// workspace is empty. Used both for the initial focus pick and whenever
/// the tree changes out from under the current focus.
fn first_leaf(workspace: &WorkspaceInfo) -> Option<ClientPaneId> {
    workspace.tree.as_ref().and_then(|t| t.leaves().first().map(|p| p.id))
}

/// Pick the next leaf to focus, cycling through `tree.leaves()` in tree
/// order. See module doc "Focus movement is leaf-cycling, not 2D
/// adjacency" for why this isn't real geometric adjacency: `SplitTree`
/// carries no rect information, only split ratios, so there is nothing to
/// compute real "left of"/"above" against. `forward = true` is
/// Right/Down (next leaf); `forward = false` is Left/Up (previous leaf).
/// Wraps at either end. Returns `None` only if the tree has zero leaves,
/// which cannot happen for a real `SplitTree` (every node is `Leaf` or a
/// `Split` of two subtrees, never empty) but is handled defensively
/// anyway.
fn cycle_focus(tree: &SplitTree, current: Option<ClientPaneId>, forward: bool) -> Option<ClientPaneId> {
    let leaves = tree.leaves();
    if leaves.is_empty() {
        return None;
    }
    let idx = current.and_then(|id| leaves.iter().position(|p| p.id == id));
    let next_idx = match idx {
        Some(i) if forward => (i + 1) % leaves.len(),
        Some(i) => (i + leaves.len() - 1) % leaves.len(),
        // Focused pane not found among the current leaves (e.g. it was
        // just closed elsewhere) -- land on the first leaf rather than
        // picking an arbitrary direction-dependent index.
        None => 0,
    };
    Some(leaves[next_idx].id)
}

/// Find whichever divider's grab zone contains `(col, row)`, if any. Pure
/// hit-testing logic factored out of `App::handle_mouse` so it's directly
/// unit-testable without a live connection.
fn hit_test_dividers(hits: &[render::DividerHit], col: u16, row: u16) -> Option<crate::protocol::SplitId> {
    hits.iter()
        .find(|hit| {
            col >= hit.grab_zone.x
                && col < hit.grab_zone.x + hit.grab_zone.width
                && row >= hit.grab_zone.y
                && row < hit.grab_zone.y + hit.grab_zone.height
        })
        .map(|hit| hit.split)
}

/// Sort `servers` into cwd-bucket order for the attach menu's grouped
/// display: ascending by cwd string, with a synthetic `"Unknown"` bucket
/// (no resolvable foreground `cwd` — a `Dead` pane, or a live one whose
/// lookup failed) always sorted last regardless of where it would fall
/// alphabetically. Within a bucket, panes keep their relative input
/// order (stable sort, no secondary key) — whatever order `ServerList`/
/// `ServerPaneList` returned. Returns `(group_key, server)` pairs rather
/// than a nested `Vec<Vec<_>>` so callers can walk it once and detect
/// group boundaries by comparing consecutive keys, which is exactly what
/// `render::draw_attach_menu` needs to decide where to emit a header.
fn group_servers_by_cwd(mut servers: Vec<ServerPaneInfo>) -> Vec<(String, ServerPaneInfo)> {
    const UNKNOWN: &str = "Unknown";
    servers.sort_by(|a, b| {
        let key_a = a.foreground.as_ref().and_then(|f| f.cwd.as_deref()).unwrap_or(UNKNOWN);
        let key_b = b.foreground.as_ref().and_then(|f| f.cwd.as_deref()).unwrap_or(UNKNOWN);
        match (key_a == UNKNOWN, key_b == UNKNOWN) {
            (true, true) => std::cmp::Ordering::Equal,
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            (false, false) => key_a.cmp(key_b),
        }
    });
    servers
        .into_iter()
        .map(|s| {
            let key = s.foreground.as_ref().and_then(|f| f.cwd.clone()).unwrap_or_else(|| UNKNOWN.to_string());
            (key, s)
        })
        .collect()
}

/// Attach-menu input, decoupled from the raw bytes that produced it (same
/// approach the removed pane-picker used: a distinct, simpler modal
/// grammar rather than reusing `keys::parse`'s chords).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttachMenuAction {
    Up,
    Down,
    Confirm,
    Cancel,
    /// `x` on a real server-pane row: arm (first press) or confirm
    /// (second press) that row's deletion — see
    /// `App::handle_attach_menu_input`'s `pending_delete` branch for the
    /// actual arm/confirm/cancel state machine; this variant alone
    /// doesn't distinguish arm from confirm, since that distinction
    /// depends on `AttachMenu.pending_delete`'s current state, not on
    /// the byte itself.
    Delete,
    /// `r` on a real server-pane row: open the inline rename field.
    StartRename,
    Ignore,
}

/// Map one raw input chunk to an `AttachMenuAction`. Direct byte matching:
/// arrow-key escape sequences and vi-style `j`/`k` move the selection,
/// Enter (`\r` or `\n`) confirms, a bare `Esc` cancels (leaving the pane
/// unbound — it was already detached by `detach_and_open_menu` before the
/// menu opened), `x` arms/confirms delete, `r` opens rename. Anything else
/// is ignored rather than passed through — the menu is modal and has no
/// server-pane to forward keystrokes to yet. Note: this table is only
/// consulted for *browsing* mode — `App::handle_attach_menu_input` routes
/// to entirely separate byte-handling while `pending_delete`/`rename` are
/// active, so `x`/Enter's *confirm* behavior when a delete is already
/// armed is handled there, not by this function returning some third
/// "confirm delete" variant.
fn parse_attach_menu_input(bytes: &[u8]) -> AttachMenuAction {
    match bytes {
        b"\r" | b"\n" => AttachMenuAction::Confirm,
        b"\x1b" => AttachMenuAction::Cancel,
        b"\x1b[A" | b"k" => AttachMenuAction::Up,
        b"\x1b[B" | b"j" => AttachMenuAction::Down,
        b"x" => AttachMenuAction::Delete,
        b"r" => AttachMenuAction::StartRename,
        _ => AttachMenuAction::Ignore,
    }
}

/// RAII guard ensuring the terminal is restored (raw mode disabled,
/// alternate screen left, mouse capture disabled) on every exit path out
/// of `run` — normal return, `?`-propagated error, or panic (via the
/// panic hook `ratatui::try_init` installs). Leaving the user's real
/// terminal in raw/alternate-screen/mouse-capture mode on a crash is a
/// real usability bug, not a nitpick (task brief) — this is what closes
/// that gap unconditionally, since `Drop` runs on every one of those
/// paths. Mouse capture is disabled best-effort (a failed write here
/// shouldn't panic mid-unwind); raw mode/alt screen restoration via
/// `ratatui::restore()` already handles its own errors the same way.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_button_event_mouse_tracking();
        ratatui::restore();
    }
}

/// Enable exactly the xterm mouse modes dimux's drag-resize needs: normal
/// tracking (`?1000h`, click/release) + button-event tracking (`?1002h`,
/// motion reported only *while a button is held* — i.e. drags) + SGR
/// extended-coordinate encoding (`?1006h`, what `mouse::parse` expects).
///
/// Deliberately NOT `ratatui::crossterm::event::EnableMouseCapture`: that
/// command additionally enables any-event tracking (`?1003h`), which
/// reports *every* mouse movement — including with no button held at
/// all, e.g. just passing the cursor over the window while typing
/// elsewhere in it. `mouse::parse` only understands the left button
/// (dimux has no use for right/middle/movement-only events) and returns
/// `None` for anything else, which then falls through to `keys::parse`,
/// which also doesn't recognize it, resolves to `Action::PassThrough`,
/// and writes the raw unparsed escape sequence into the focused pane as
/// literal keystrokes. Under `?1003h` this happens on every pixel of
/// mouse motion, which is both the garbage-character symptom (a mouse
/// move's SGR encoding — button number 3, i.e. `Cb=35` — landing in the
/// shell as text) and the "random hangup" (each stray byte round-trips a
/// `Request::Input` through the daemon; enough volume visibly stalls the
/// event loop). `?1002h` alone never asks the terminal to report motion
/// without a button down, so this class of event never arrives.
fn enable_button_event_mouse_tracking() -> std::io::Result<()> {
    use std::io::Write;
    write!(std::io::stdout(), "\x1b[?1000h\x1b[?1002h\x1b[?1006h")?;
    std::io::stdout().flush()
}

/// Inverse of [`enable_button_event_mouse_tracking`], in reverse order —
/// same convention `crossterm::event::DisableMouseCapture` follows for
/// its own mode list.
fn disable_button_event_mouse_tracking() -> std::io::Result<()> {
    use std::io::Write;
    write!(std::io::stdout(), "\x1b[?1006l\x1b[?1002l\x1b[?1000l")?;
    std::io::stdout().flush()
}

/// Drives one attached frontend: connect, subscribe to the initial
/// workspace, then loop handling terminal input events and pushed daemon
/// `Event`s until the user quits.
pub async fn run() -> anyhow::Result<()> {
    let client = Client::connect().await?;
    let (read_half, mut write_half) = client.into_split();
    let mut reader = FrameReader::spawn(read_half);

    // `try_init` (rather than the panicking `init`) so a setup failure
    // becomes a normal `anyhow::Result` error via `?` instead of a panic
    // -- it still installs the same "restore terminal before panicking"
    // hook `init` does, so a *later* panic is still handled safely; this
    // only changes how a failure *during setup itself* is reported.
    let mut terminal = ratatui::try_init()?;
    // From here on, every exit path (including the `?`s below) restores
    // the terminal via `TerminalGuard::drop` (including disabling mouse
    // capture, enabled next).
    let _guard = TerminalGuard;
    enable_button_event_mouse_tracking()?;

    let mut app = App::bootstrap(&mut write_half, &mut reader, "1").await?;

    let mut stdin = tokio::io::stdin();
    let mut buf = [0u8; 512];

    loop {
        terminal.draw(|frame| {
            app.frame_area = frame.area();
            // While a divider drag is in progress, render a locally
            // patched clone with the live ratio applied -- this is what
            // makes the drag look smooth (drawn every frame with no
            // round-trip) even though the daemon isn't told about it
            // until `Up` (see `dragging_split`'s field doc for why).
            match app.dragging_split {
                Some((split, ratio)) => {
                    let mut preview = app.workspace.clone();
                    if let Some(tree) = &mut preview.tree {
                        let _ = tree.resize_split(split, ratio);
                    }
                    render::draw(frame, &preview, &app.grids, app.focused);
                }
                None => render::draw(frame, &app.workspace, &app.grids, app.focused),
            }
            if let Some(menu) = &app.attach_menu {
                render::draw_attach_menu(frame, &menu.servers, menu.selected);
            }
        })?;

        tokio::select! {
            result = stdin.read(&mut buf) => {
                let n = result?;
                if n == 0 {
                    // stdin closed (e.g. piped input exhausted) -- nothing
                    // further to read, so there is no way to drive the UI;
                    // exit cleanly rather than spin.
                    break;
                }
                let bytes = &buf[..n];
                if is_quit(bytes) {
                    break;
                }
                match mouse::parse(bytes) {
                    mouse::ParsedInput::Mouse(event) => {
                        app.handle_mouse(event, &mut write_half, &mut reader).await?;
                    }
                    // A recognized SGR mouse sequence dimux has no use
                    // for (see mouse.rs module doc "Defense in depth") --
                    // definitely mouse input, definitely not keyboard
                    // input, so discard it rather than falling through to
                    // the chord/pass-through paths below.
                    mouse::ParsedInput::Ignored => {}
                    mouse::ParsedInput::NotMouse => {
                        if app.attach_menu.is_some() {
                            app.handle_attach_menu_input(bytes, &mut write_half, &mut reader).await?;
                        } else {
                            let action = keys::parse(bytes);
                            app.handle_action(action, bytes, &mut write_half, &mut reader).await?;
                        }
                    }
                }
            }
            frame = reader.next() => {
                match frame {
                    Ok(ServerMessage::Event(event)) => app.apply_event(event),
                    // Every request this loop sends is awaited synchronously
                    // via `App::request` (which itself drains any Events
                    // ahead of the Response) before control returns here, so
                    // a bare `Response` surfacing on this branch would mean
                    // the daemon sent one with no in-flight request from
                    // this connection -- a protocol-level surprise, not a
                    // reason to crash the frontend.
                    Ok(ServerMessage::Response(_)) => {}
                    // Connection lost -- nothing further to drive the UI
                    // with.
                    Err(_) => break,
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ClientPane;
    use uuid::Uuid;

    fn leaf(id: ClientPaneId) -> SplitTree {
        SplitTree::Leaf(ClientPane { id, name: None, bound: None })
    }

    fn split(dir: SplitDir, a: SplitTree, b: SplitTree) -> SplitTree {
        SplitTree::Split { id: Uuid::new_v4(), dir, ratio: 0.5, a: Box::new(a), b: Box::new(b) }
    }

    #[test]
    fn cycle_focus_single_leaf_stays_put() {
        let id = Uuid::new_v4();
        let tree = leaf(id);
        assert_eq!(cycle_focus(&tree, Some(id), true), Some(id));
        assert_eq!(cycle_focus(&tree, Some(id), false), Some(id));
    }

    #[test]
    fn cycle_focus_no_current_focus_lands_on_first_leaf() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let tree = split(SplitDir::Vertical, leaf(a), leaf(b));
        assert_eq!(cycle_focus(&tree, None, true), Some(a));
        assert_eq!(cycle_focus(&tree, None, false), Some(a));
    }

    #[test]
    fn cycle_focus_forward_moves_to_next_leaf_and_wraps() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        // ((a, b), c) -- leaves() order is a, b, c.
        let tree = split(SplitDir::Vertical, split(SplitDir::Horizontal, leaf(a), leaf(b)), leaf(c));
        assert_eq!(cycle_focus(&tree, Some(a), true), Some(b));
        assert_eq!(cycle_focus(&tree, Some(b), true), Some(c));
        assert_eq!(cycle_focus(&tree, Some(c), true), Some(a), "forward wraps past the last leaf");
    }

    #[test]
    fn cycle_focus_backward_moves_to_previous_leaf_and_wraps() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        let tree = split(SplitDir::Vertical, split(SplitDir::Horizontal, leaf(a), leaf(b)), leaf(c));
        assert_eq!(cycle_focus(&tree, Some(c), false), Some(b));
        assert_eq!(cycle_focus(&tree, Some(b), false), Some(a));
        assert_eq!(cycle_focus(&tree, Some(a), false), Some(c), "backward wraps past the first leaf");
    }

    #[test]
    fn cycle_focus_unknown_current_id_lands_on_first_leaf() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let tree = split(SplitDir::Vertical, leaf(a), leaf(b));
        let stale = Uuid::new_v4();
        assert_eq!(cycle_focus(&tree, Some(stale), true), Some(a));
        assert_eq!(cycle_focus(&tree, Some(stale), false), Some(a));
    }

    fn divider_hit(split: crate::protocol::SplitId, zone: ratatui::layout::Rect) -> render::DividerHit {
        render::DividerHit {
            split,
            dir: SplitDir::Vertical,
            grab_zone: zone,
            parent_area: ratatui::layout::Rect { x: 0, y: 0, width: 40, height: 10 },
        }
    }

    #[test]
    fn hit_test_dividers_finds_the_containing_zone() {
        let split_id = Uuid::new_v4();
        let hits = vec![divider_hit(split_id, ratatui::layout::Rect { x: 20, y: 0, width: 1, height: 10 })];
        assert_eq!(hit_test_dividers(&hits, 20, 5), Some(split_id));
    }

    #[test]
    fn hit_test_dividers_misses_outside_the_zone() {
        let split_id = Uuid::new_v4();
        let hits = vec![divider_hit(split_id, ratatui::layout::Rect { x: 20, y: 0, width: 1, height: 10 })];
        assert_eq!(hit_test_dividers(&hits, 19, 5), None);
        assert_eq!(hit_test_dividers(&hits, 21, 5), None);
        assert_eq!(hit_test_dividers(&hits, 20, 10), None); // one past the bottom edge
    }

    #[test]
    fn hit_test_dividers_picks_the_first_match_among_several() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let hits = vec![
            divider_hit(a, ratatui::layout::Rect { x: 10, y: 0, width: 1, height: 10 }),
            divider_hit(b, ratatui::layout::Rect { x: 30, y: 0, width: 1, height: 10 }),
        ];
        assert_eq!(hit_test_dividers(&hits, 10, 0), Some(a));
        assert_eq!(hit_test_dividers(&hits, 30, 0), Some(b));
        assert_eq!(hit_test_dividers(&hits, 20, 0), None);
    }

    #[test]
    fn attach_menu_input_enter_and_newline_confirm() {
        assert_eq!(parse_attach_menu_input(b"\r"), AttachMenuAction::Confirm);
        assert_eq!(parse_attach_menu_input(b"\n"), AttachMenuAction::Confirm);
    }

    #[test]
    fn attach_menu_input_bare_escape_cancels() {
        assert_eq!(parse_attach_menu_input(b"\x1b"), AttachMenuAction::Cancel);
    }

    #[test]
    fn attach_menu_input_arrow_and_vi_keys_move_selection() {
        assert_eq!(parse_attach_menu_input(b"\x1b[A"), AttachMenuAction::Up);
        assert_eq!(parse_attach_menu_input(b"k"), AttachMenuAction::Up);
        assert_eq!(parse_attach_menu_input(b"\x1b[B"), AttachMenuAction::Down);
        assert_eq!(parse_attach_menu_input(b"j"), AttachMenuAction::Down);
    }

    #[test]
    fn attach_menu_input_unrecognized_bytes_are_ignored() {
        assert_eq!(parse_attach_menu_input(b"q"), AttachMenuAction::Ignore);
        assert_eq!(parse_attach_menu_input(b""), AttachMenuAction::Ignore);
    }

    #[test]
    fn parse_attach_menu_input_x_is_delete() {
        assert_eq!(parse_attach_menu_input(b"x"), AttachMenuAction::Delete);
    }

    #[test]
    fn parse_attach_menu_input_r_is_start_rename() {
        assert_eq!(parse_attach_menu_input(b"r"), AttachMenuAction::StartRename);
    }

    #[test]
    fn quit_byte_is_ctrl_q_not_ctrl_c() {
        assert!(is_quit(&[0x11]));
        assert!(!is_quit(&[0x03]), "Ctrl-C must remain a pass-through byte, not the quit key");
    }

    #[test]
    fn first_leaf_of_empty_workspace_is_none() {
        let workspace = WorkspaceInfo { id: Uuid::new_v4(), number: 1, name: None, tree: None };
        assert_eq!(first_leaf(&workspace), None);
    }

    #[test]
    fn first_leaf_of_populated_workspace_is_leftmost_leaf() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let workspace = WorkspaceInfo {
            id: Uuid::new_v4(),
            number: 1,
            name: None,
            tree: Some(split(SplitDir::Vertical, leaf(a), leaf(b))),
        };
        assert_eq!(first_leaf(&workspace), Some(a));
    }

    fn server_with_cwd(name: &str, cwd: Option<&str>) -> ServerPaneInfo {
        ServerPaneInfo {
            id: Uuid::new_v4(),
            name: Some(name.to_string()),
            size: crate::protocol::Size { rows: 24, cols: 80 },
            status: crate::protocol::ServerPaneStatus::Running,
            foreground: cwd.map(|c| crate::protocol::ForegroundProcessInfo {
                process_name: "bash".to_string(),
                cwd: Some(c.to_string()),
            }),
        }
    }

    #[test]
    fn group_servers_by_cwd_groups_matching_dirs_together() {
        let servers = vec![
            server_with_cwd("a", Some("/home/dev/api")),
            server_with_cwd("b", Some("/home/dev/web")),
            server_with_cwd("c", Some("/home/dev/api")),
        ];
        let grouped = group_servers_by_cwd(servers);
        let names: Vec<&str> = grouped.iter().map(|(_, s)| s.name.as_deref().unwrap()).collect();
        // Ascending by cwd: api's two panes (in original relative order),
        // then web's one pane.
        assert_eq!(names, vec!["a", "c", "b"]);
        let keys: Vec<&str> = grouped.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["/home/dev/api", "/home/dev/api", "/home/dev/web"]);
    }

    #[test]
    fn group_servers_by_cwd_sorts_unknown_group_last() {
        let servers = vec![
            server_with_cwd("no-cwd", None),
            server_with_cwd("has-cwd", Some("/zzz/last-alphabetically")),
        ];
        let grouped = group_servers_by_cwd(servers);
        let names: Vec<&str> = grouped.iter().map(|(_, s)| s.name.as_deref().unwrap()).collect();
        // "/zzz/..." sorts after "Unknown" alphabetically, but Unknown is
        // forced last regardless.
        assert_eq!(names, vec!["has-cwd", "no-cwd"]);
        assert_eq!(grouped[1].0, "Unknown");
    }

    #[test]
    fn group_servers_by_cwd_empty_list_is_empty() {
        // `ServerPaneInfo` doesn't derive `PartialEq`, so compare lengths
        // rather than the whole `Vec` via `assert_eq!`.
        assert_eq!(group_servers_by_cwd(vec![]).len(), 0);
    }

    #[test]
    fn arm_delete_sets_pending_delete_on_a_real_row() {
        let servers = vec![("Unknown".to_string(), server_with_cwd("a", None))];
        let mut app = App {
            workspace: WorkspaceInfo { id: Uuid::new_v4(), number: 1, name: None, tree: None },
            grids: HashMap::new(),
            focused: None,
            attach_menu: Some(AttachMenu { servers, selected: 0, pending_delete: None, rename: None }),
            frame_area: ratatui::layout::Rect::default(),
            dragging_split: None,
        };
        app.arm_delete();
        assert_eq!(app.attach_menu.unwrap().pending_delete, Some(0));
    }

    #[test]
    fn arm_delete_on_spawn_new_row_is_a_no_op() {
        let servers = vec![("Unknown".to_string(), server_with_cwd("a", None))];
        let spawn_index = servers.len();
        let mut app = App {
            workspace: WorkspaceInfo { id: Uuid::new_v4(), number: 1, name: None, tree: None },
            grids: HashMap::new(),
            focused: None,
            attach_menu: Some(AttachMenu {
                servers,
                selected: spawn_index,
                pending_delete: None,
                rename: None,
            }),
            frame_area: ratatui::layout::Rect::default(),
            dragging_split: None,
        };
        app.arm_delete();
        assert_eq!(app.attach_menu.unwrap().pending_delete, None);
    }

    /// Regression test for the "dimux hangs often" bug: racing a frame
    /// read against a faster competing branch inside `tokio::select!`
    /// must never desync the connection. This is what `run`'s loop does
    /// every iteration (racing `stdin.read` against the next server
    /// frame) — before `FrameReader` existed, that raced
    /// `protocol::framing::read_frame` directly, and `read_exact`
    /// (which it's built on) is not cancellation-safe: losing the race
    /// after partially reading a frame's bytes drops them permanently,
    /// desyncing the length-prefixed protocol so every later read either
    /// errors on garbage or blocks forever on a bogus length — exactly
    /// "frozen, no response" with idle threads. `FrameReader` fixes this
    /// by giving `read_frame` a dedicated task that's never raced;
    /// `next()` instead races an `mpsc::Receiver`, which tokio guarantees
    /// is cancellation-safe.
    ///
    /// Writes each frame's body one byte at a time with a delay between
    /// bytes, so a `read_exact` inside the background task's `read_frame`
    /// call would (pre-fix) very likely get cancelled mid-body if it were
    /// raced directly, the same way `run`'s real loop could hit it under
    /// ordinary scheduling while the user types.
    #[tokio::test]
    async fn frame_reader_survives_being_raced_against_a_faster_branch() {
        let path = std::env::temp_dir().join(format!("dmx-{}.sock", std::process::id()));
        let listener = tokio::net::UnixListener::bind(&path).unwrap();

        let writer_path = path.clone();
        let writer = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let mut stream = tokio::net::UnixStream::connect(&writer_path).await.unwrap();
            for _ in 0..8u32 {
                let msg = ServerMessage::Response(Response::Ack);
                let body = serde_json::to_vec(&msg).unwrap();
                let len = (body.len() as u32).to_le_bytes();
                stream.write_all(&len).await.unwrap();
                for byte in &body {
                    stream.write_all(std::slice::from_ref(byte)).await.unwrap();
                    tokio::time::sleep(std::time::Duration::from_micros(200)).await;
                }
            }
        });

        let (server_stream, _addr) = listener.accept().await.unwrap();
        let (read_half, _write_half) = server_stream.into_split();
        let mut reader = FrameReader::spawn(read_half);

        let mut received = 0;
        for _ in 0..400 {
            tokio::select! {
                // Fires far more often than the 200us-per-byte dribble
                // above, so it wins the race constantly -- exactly the
                // adversarial timing that broke the old direct-read_frame
                // version.
                _ = tokio::time::sleep(std::time::Duration::from_micros(50)) => {}
                frame = reader.next() => {
                    assert!(matches!(frame, Ok(ServerMessage::Response(Response::Ack))), "unexpected frame: {frame:?}");
                    received += 1;
                    if received >= 8 {
                        break;
                    }
                }
            }
        }

        assert_eq!(received, 8, "expected all 8 frames to survive being raced against a faster branch");
        writer.await.unwrap();
        let _ = std::fs::remove_file(&path);
    }
}
