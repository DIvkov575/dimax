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

pub mod keys;
pub mod render;

use crate::cli::Client;
use crate::protocol::{
    self, ClientPaneId, Event, GridSnapshot, Request, Response, ServerMessage, ServerPaneId,
    SplitDir, SplitTree, WorkspaceInfo,
};
use std::collections::HashMap;
use tokio::io::AsyncReadExt;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

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
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    /// Not a dimux chord — forward these raw bytes to the focused
    /// client-pane's bound server-pane as keyboard input.
    PassThrough,
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
}

impl App {
    /// Send the initial `Subscribe` and build the starting `App` state
    /// from its `Snapshot` response. Free function (not `&mut self`)
    /// because there is no `App` yet — this is what constructs the first
    /// one, per design doc "Subscription model": "On attach ... the
    /// frontend sends `Subscribe(workspace_id)`."
    async fn bootstrap(
        write_half: &mut OwnedWriteHalf,
        read_half: &mut OwnedReadHalf,
        workspace: &str,
    ) -> anyhow::Result<Self> {
        protocol::framing::write_frame(
            write_half,
            &Request::Subscribe { workspace: workspace.to_string() },
        )
        .await?;
        loop {
            match protocol::framing::read_frame::<_, ServerMessage>(read_half).await? {
                ServerMessage::Response(Response::Snapshot { workspace, grids }) => {
                    let focused = first_leaf(&workspace);
                    return Ok(App {
                        workspace,
                        grids: grids.into_iter().map(|g| (g.server_pane, g)).collect(),
                        focused,
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
        read_half: &mut OwnedReadHalf,
        req: Request,
    ) -> anyhow::Result<Response> {
        protocol::framing::write_frame(write_half, &req).await?;
        loop {
            match protocol::framing::read_frame::<_, ServerMessage>(read_half).await? {
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
        read_half: &mut OwnedReadHalf,
    ) -> anyhow::Result<()> {
        let old = self.workspace.id;
        let _ = self.request(write_half, read_half, Request::Unsubscribe { workspace: old }).await?;
        let resp = self
            .request(write_half, read_half, Request::Subscribe { workspace: n.to_string() })
            .await?;
        if let Response::Snapshot { workspace, grids } = resp {
            self.focused = first_leaf(&workspace);
            self.workspace = workspace;
            self.grids = grids.into_iter().map(|g| (g.server_pane, g)).collect();
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
        read_half: &mut OwnedReadHalf,
    ) -> anyhow::Result<()> {
        let Response::ServerPane(server) = self
            .request(write_half, read_half, Request::ServerSpawn { name: None, cmd: None })
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
        if let Response::ClientPaneCreated { pane, .. } = self.request(write_half, read_half, req).await? {
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
        read_half: &mut OwnedReadHalf,
    ) -> anyhow::Result<()> {
        let Some(pane) = self.focused else { return Ok(()) };
        let req = Request::ClientClose { workspace: self.workspace.id.to_string(), pane };
        let _ = self.request(write_half, read_half, req).await?;
        Ok(())
    }

    /// `cmd-shift-w`: kill the server-pane bound to the focused
    /// client-pane, if any.
    async fn kill_focused_server_pane(
        &mut self,
        write_half: &mut OwnedWriteHalf,
        read_half: &mut OwnedReadHalf,
    ) -> anyhow::Result<()> {
        let Some(pane) = self.focused else { return Ok(()) };
        let Some(bound) = self.workspace.tree.as_ref().and_then(|t| t.find(pane)).and_then(|p| p.bound)
        else {
            return Ok(());
        };
        let req = Request::ServerKill { target: bound.to_string() };
        let _ = self.request(write_half, read_half, req).await?;
        Ok(())
    }

    fn move_focus(&mut self, forward: bool) {
        if let Some(tree) = &self.workspace.tree {
            self.focused = cycle_focus(tree, self.focused, forward);
        }
    }

    /// Route one resolved `Action` to its effect.
    async fn handle_action(
        &mut self,
        action: Action,
        raw: &[u8],
        write_half: &mut OwnedWriteHalf,
        read_half: &mut OwnedReadHalf,
    ) -> anyhow::Result<()> {
        match action {
            Action::SwitchWorkspace(n) => self.switch_workspace(n, write_half, read_half).await?,
            Action::SplitVertical => self.split(SplitDir::Vertical, write_half, read_half).await?,
            Action::SplitHorizontal => self.split(SplitDir::Horizontal, write_half, read_half).await?,
            Action::CloseFocusedPane => self.close_focused(write_half, read_half).await?,
            Action::KillFocusedServerPane => {
                self.kill_focused_server_pane(write_half, read_half).await?
            }
            Action::FocusLeft | Action::FocusUp => self.move_focus(false),
            Action::FocusRight | Action::FocusDown => self.move_focus(true),
            Action::PassThrough => {
                if let Some(pane) = self.focused {
                    let req = Request::Input { pane, bytes: raw.to_vec() };
                    let _ = self.request(write_half, read_half, req).await?;
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

/// RAII guard ensuring the terminal is restored (raw mode disabled,
/// alternate screen left) on every exit path out of `run` — normal
/// return, `?`-propagated error, or panic (via the panic hook
/// `ratatui::try_init` installs). Leaving the user's real terminal in
/// raw/alternate-screen mode on a crash is a real usability bug, not a
/// nitpick (task brief) — this is what closes that gap unconditionally,
/// since `Drop` runs on every one of those paths.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

/// Drives one attached frontend: connect, subscribe to the initial
/// workspace, then loop handling terminal input events and pushed daemon
/// `Event`s until the user quits.
pub async fn run() -> anyhow::Result<()> {
    let client = Client::connect().await?;
    let (mut read_half, mut write_half) = client.into_split();

    // `try_init` (rather than the panicking `init`) so a setup failure
    // becomes a normal `anyhow::Result` error via `?` instead of a panic
    // -- it still installs the same "restore terminal before panicking"
    // hook `init` does, so a *later* panic is still handled safely; this
    // only changes how a failure *during setup itself* is reported.
    let mut terminal = ratatui::try_init()?;
    // From here on, every exit path (including the `?`s below) restores
    // the terminal via `TerminalGuard::drop`.
    let _guard = TerminalGuard;

    let mut app = App::bootstrap(&mut write_half, &mut read_half, "1").await?;

    let mut stdin = tokio::io::stdin();
    let mut buf = [0u8; 512];

    loop {
        terminal.draw(|frame| {
            render::draw(frame, &app.workspace, &app.grids, app.focused);
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
                let action = keys::parse(bytes);
                app.handle_action(action, bytes, &mut write_half, &mut read_half).await?;
            }
            frame = protocol::framing::read_frame::<_, ServerMessage>(&mut read_half) => {
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
        SplitTree::Split { dir, ratio: 0.5, a: Box::new(a), b: Box::new(b) }
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
}
