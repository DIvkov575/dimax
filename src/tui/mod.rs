//! The `dimax attach` frontend: connects to the daemon, subscribes to a
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
//! - **Quit key: `Ctrl-Q` (byte `0x11`), not `Ctrl-C`.** `Ctrl-C` was
//!   deliberately *not* picked: it needs to `PassThrough` to whatever's
//!   running in the focused pane (interrupting a stuck program is one of
//!   the most common reasons to press it), and stealing it globally
//!   would make that impossible. `Ctrl-Q` is rarely bound by interactive
//!   programs and (in raw mode, with software flow control not in play)
//!   is just an inert byte otherwise -- checked directly on raw input
//!   before any mode-specific parsing, so it works in every keybinding
//!   mode, and even before one has been chosen. `Action::Quit` (`cmd-
//!   shift-q`/`Ctrl-Space q`, see `keys::BINDINGS`) is the formalized,
//!   discoverable counterpart once a mode is configured -- this raw byte
//!   check remains alongside it as the always-available fallback, not a
//!   stand-in for it.
//! - **Focus movement (`FocusLeft/Right/Up/Down`) uses real screen
//!   adjacency.** `nearest_leaf_in_direction` computes each leaf's
//!   actual on-screen `Rect` via `render::leaf_rects` (the same data
//!   mouse hit-testing already uses) and picks whichever leaf lies in
//!   the requested direction, nearest first. No wraparound: if nothing
//!   is in that direction, the chord is a no-op.
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
//!   Root cause of the "dimax hangs often" reports: `run`'s loop used to
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
//! - **Text selection reuses the same left-button event stream.** A press
//!   inside pane content maps screen coordinates back to that pane's
//!   `GridSnapshot`; drag updates a pane-local range and release returns
//!   its text for an OSC 52 clipboard write. No terminal configuration or
//!   platform-specific clipboard process is required.

mod first_run;
pub mod keys;
pub(crate) mod kitty_setup;
pub mod mouse;
pub mod render;
mod selection;

use crate::cli::Client;
use crate::protocol::{
    self, ClientPaneId, Event, GridSnapshot, Request, Response, ServerMessage, ServerPaneId,
    ServerPaneInfo, Size, SplitDir, SplitTree, WorkspaceInfo,
};
use std::collections::{HashMap, HashSet};
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
        self.rx
            .recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("connection closed"))?
    }
}

/// User-facing actions a keypress can resolve to, decoupled from the
/// specific chord that produced it so `render`/the event loop don't need
/// to know about Kitty escape sequences.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    SwitchWorkspace(u8),
    JumpSession(u8),
    SplitVertical,
    SplitHorizontal,
    CloseFocusedPane,
    KillFocusedServerPane,
    /// `cmd-shift-z`: detach the focused client-pane from its current
    /// server-pane (which keeps running) and open the attach menu to pick
    /// its replacement.
    DetachAndAttach,
    AddTab,
    CycleTabForward,
    CycleTabBackward,
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    /// Exit `dimax attach` entirely, returning to the shell -- reachable
    /// through the normal chord machinery in both binding modes (see
    /// `BINDINGS`' `cmd+shift+q`/`Ctrl-Space q` entry in `keys.rs`), so
    /// it's discoverable via `dimax keys list`/`print` and aliasable
    /// like every other action. The raw `Ctrl-Q` byte check in `run`'s
    /// read loop (module doc "Quit key") is a separate, always-available
    /// fallback that works even before a keybinding mode has been
    /// chosen; this is not a replacement for it.
    Quit,
    /// Not a dimax chord — forward these raw bytes to the focused
    /// client-pane's bound server-pane as keyboard input.
    PassThrough,
}

/// A screen-relative direction for [`nearest_leaf_in_direction`]. Not
/// `SplitDir` (which names a divider's orientation) or
/// `ratatui::layout::Direction` (which names a layout axis) — both
/// already mean something else in this codebase; see `render.rs`
/// module doc "SplitDir -> ratatui::Direction mapping" for why a third,
/// unambiguous name is worth it here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Left,
    Right,
    Up,
    Down,
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
    /// `selected` does NOT index this `Vec` directly — group headers
    /// are selectable rows too (see [`visible_attach_menu_rows`]), so
    /// grouping stays this field's own concern while *which row is
    /// selected* is a layer on top.
    servers: Vec<(String, ServerPaneInfo)>,
    /// Index into `visible_attach_menu_rows(self, collapsed)`'s
    /// returned row list — NOT into `servers` directly. `collapsed`
    /// lives on `App` (`App.collapsed_groups`), not here, so any method
    /// reading or moving `selected` needs both `self.attach_menu` and
    /// `self.collapsed_groups` to make sense of it.
    selected: usize,
    /// `Some(index into servers)` while that row's delete is armed
    /// (first `x` pressed, awaiting a confirming `x`/Enter or a
    /// cancelling any-other-key). Mutually exclusive with `rename` —
    /// opening one clears the other.
    pending_delete: Option<usize>,
    /// `Some` while the inline rename field is focused for the row at
    /// `.index`. See `RenameState`'s own doc comment.
    rename: Option<RenameState>,
    /// The server-pane the focused client-pane was bound to right
    /// before `detach_and_open_menu` unbound it to open this menu --
    /// `None` if it was already unbound (nothing to mark). Captured
    /// from local state *before* sending `ClientUnbind` (once that
    /// request lands, the binding is gone server-side too, so there's
    /// no later point at which this could still be read back).
    /// Rendered as a `*` marker on that row so picking a *different*
    /// server-pane vs. re-attaching the same one is an informed choice,
    /// not a guess from memory.
    previously_bound: Option<ServerPaneId>,
    /// `Some` while a group's "+ spawn new in <dir>" row has its inline
    /// input field open. See `SpawnInGroupState`'s own doc comment.
    /// Mutually exclusive with `rename`/`pending_delete` — opening this
    /// clears both, same as `rename` already does.
    spawn_in_group: Option<SpawnInGroupState>,
    /// `true` when `cmd-t` opened this menu (append the pick as a new
    /// tab, leaving existing tabs bound), `false` when `cmd-shift-z`
    /// did (replace the active tab, having already unbound it). Set at
    /// construction and read at exactly one point per commit path —
    /// `confirm_attach_menu` and `confirm_spawn_in_group`'s bind branch
    /// — to choose between `ClientAddTab` and `ClientBind`. Every other
    /// aspect of the menu (grouping, rename/delete, spawn field,
    /// preview) behaves identically in both modes.
    adding_tab: bool,
}

/// Live edit state for the attach menu's inline rename field (`r` on a
/// row). `text`/`cursor` are the field's edit buffer and cursor
/// position — a byte offset into `text`, always kept on a UTF-8 char
/// boundary by every editing operation in `apply_text_edit`. `error`
/// holds the daemon's last rejection message (e.g. a name collision) to
/// render under the field; cleared on the next edit so a stale error
/// doesn't linger once the user starts fixing it.
struct RenameState {
    index: usize,
    text: String,
    cursor: usize,
    error: Option<String>,
}

/// Live edit state for a group's "+ spawn new in <dir>" row, opened by
/// pressing space while that row is selected (see `App::
/// handle_attach_menu_input`'s browse-mode dispatch — this is not
/// reached via `AttachMenuAction`/`parse_attach_menu_input` like
/// `StartRename` is, since space opens it directly rather than through
/// a dedicated `AttachMenuAction` variant). `group_server_index` is the
/// same first-member index
/// `AttachMenuRow::SpawnNewInGroup` carries, from which the group's cwd
/// key (`servers[group_server_index].0`) is looked up when the field is
/// confirmed. `text`/`cursor`/`error` mirror `RenameState` exactly and
/// share its editing logic (`apply_text_edit`) — this field only differs
/// in what confirming it *does* (spawn+bind+send vs. rename), not in how
/// it's edited.
struct SpawnInGroupState {
    group_server_index: usize,
    text: String,
    cursor: usize,
    error: Option<String>,
}

/// Raw byte, read outside `keys::parse`'s chord grammar entirely, that
/// exits `dimax attach`. See module doc "Quit key" for why this isn't
/// `Ctrl-C`.
const QUIT_BYTE: u8 = 0x11; // Ctrl-Q

fn is_quit(bytes: &[u8]) -> bool {
    bytes == [QUIT_BYTE]
}

/// Rows scrolled per mouse-wheel tick. Arbitrary but
/// conventional-feeling default -- tune freely, no protocol
/// implications either way since `Request::ScrollClientPane::delta` is
/// already an arbitrary signed `i32`.
const SCROLL_ROWS_PER_TICK: i32 = 3;

/// All mutable state the event loop owns, per module doc "Local mutable
/// state" in the task this module implements. Kept as one struct (rather
/// than loose locals) so every state-mutating helper below can take
/// `&mut self` instead of five separate `&mut` parameters.
struct App {
    workspace: WorkspaceInfo,
    grids: HashMap<ServerPaneId, GridSnapshot>,
    /// Last on-screen size this frontend told the daemon about for each
    /// client-pane (`Request::ResizeClientPane`), so `run`'s loop only
    /// sends a new report when something actually changed instead of
    /// re-sending every pane's size on every single frame. Mirrors the
    /// daemon's own `State::client_pane_sizes`, one-directionally: this
    /// is purely this frontend's memory of what it last *told* the
    /// daemon, not a second source of truth.
    pane_sizes: HashMap<ClientPaneId, Size>,
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
    /// Pane-local text selection created by a left-button drag. The
    /// completed range remains visible after release; a new click,
    /// keyboard action, scroll, or incompatible layout change clears it.
    text_selection: Option<selection::TextSelection>,
    /// Directory groups in the attach menu currently collapsed (their
    /// member server-pane rows hidden, header still shown) -- keyed by
    /// the same group-key strings `group_servers_by_cwd` produces.
    /// Lives here rather than on `AttachMenu` so it survives closing and
    /// reopening the menu (and switching workspaces): collapsing a noisy
    /// group is expected to stay collapsed next time the menu opens, not
    /// silently reset.
    collapsed_groups: HashSet<String>,
    /// Whether the attach menu's rows are currently grouped under per-cwd
    /// headers (`true`, the default/original behavior) or shown as a
    /// flat list of `Server` rows with no headers at all (`false`). A
    /// purely local UI preference, same category as `collapsed_groups` --
    /// lives here so it survives closing and reopening the menu, toggled
    /// by `AttachMenuAction::ToggleGrouping` (`g` on any row) and applied
    /// by `visible_attach_menu_rows`/`initial_selection_for` everywhere
    /// the menu's row list is built.
    grouped_view: bool,
    /// Whether the attach menu shows only recognized AI-coding-CLI
    /// sessions (see `protocol::SessionKind`) or every server-pane.
    /// Defaults to `true` -- the whole point of tagging sessions is to
    /// make "which of these panes is actually an agent I started" the
    /// *easy* question to answer, and a picker mixing plain shells in
    /// with agent sessions by default undoes that. Toggled by
    /// `AttachMenuAction::ToggleAgentsOnly` (`f` on any row), same
    /// local-UI-preference category as `grouped_view` -- lives here so it
    /// survives closing and reopening the menu.
    agents_only_view: bool,
    /// Every known server-pane's current id/name, keyed by id -- kept
    /// fresh by `refresh_server_names` on the same tick as
    /// `refresh_attach_menu_preview`, and consulted by the render loop so
    /// each grid leaf's title bar can show the *bound server-pane's*
    /// name (see `render::draw_leaf`), not the client-pane wrapper's own
    /// (almost always unset) one. Unlike `attach_menu_preview`, this is
    /// wanted whether or not the attach menu is even open, since it
    /// drives the main grid view.
    server_names: HashMap<ServerPaneId, ServerPaneInfo>,
    /// The attach menu's preview panel content: whichever server-pane's
    /// row was selected as of the last `refresh_attach_menu_preview`
    /// call, and the plain-text screen contents fetched for it via
    /// `Request::ServerRead` at that moment. `None` before any fetch has
    /// completed, or whenever the selection isn't on a `Server` row at
    /// all (a header/spawn row has no pane to preview) -- rendered as a
    /// blank panel in both cases, per the "fixed-height panel, content
    /// optional" design (see `render::draw_attach_menu`'s doc comment):
    /// the popup's layout split never changes shape based on whether
    /// there's anything to show.
    ///
    /// Deliberately NOT part of `AttachMenu` (unlike `rename`/
    /// `pending_delete`/`spawn_in_group`): those are edit *state* the
    /// user is actively manipulating, while this is a fetched *cache*
    /// of read-only server data, refreshed by network round-trips the
    /// same way `grids` is -- keeping it here mirrors that split and
    /// means closing/reopening the menu doesn't need to remember to
    /// carry it along.
    attach_menu_preview: Option<(ServerPaneId, String)>,
    /// The daemon's current pin order (earliest-pinned first), cached
    /// client-side the same way `collapsed_groups` is -- fetched via
    /// `Request::PinnedDirsList`/`ToggleDirectoryPin` whenever the
    /// server list is (re-)fetched, and passed to
    /// `group_servers_by_cwd` every time `menu.servers` is (re)built.
    /// Lives on `App`, not `AttachMenu`: unlike `collapsed_groups` this
    /// is genuinely server-owned state (persisted to disk, shared
    /// across every connected frontend), not a purely local UI
    /// preference, but it's still most convenient to read from
    /// wherever the menu's grouping gets rebuilt, same as
    /// `collapsed_groups`.
    pinned_dirs: Vec<String>,
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
            &Request::Subscribe {
                workspace: workspace.to_string(),
            },
        )
        .await?;
        loop {
            match reader.next().await? {
                ServerMessage::Response(Response::Snapshot { workspace, grids }) => {
                    let focused = first_leaf(&workspace);
                    let is_empty = workspace.tree.is_none();
                    let mut app = App {
                        workspace,
                        grids: grids.into_iter().map(|g| (g.server_pane, g)).collect(),
                        pane_sizes: HashMap::new(),
                        focused,
                        attach_menu: None,
                        frame_area: ratatui::layout::Rect::default(),
                        dragging_split: None,
                        text_selection: None,
                        collapsed_groups: HashSet::new(),
                        grouped_view: true,
                        agents_only_view: true,
                        server_names: HashMap::new(),
                        attach_menu_preview: None,
                        pinned_dirs: Vec::new(),
                    };
                    if is_empty {
                        app.bootstrap_empty_workspace(write_half, reader).await?;
                    }
                    return Ok(app);
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

    /// Called from [`Self::bootstrap`] exactly when the just-fetched
    /// workspace has no tree at all -- decides, via
    /// `Request::ConsumeShellFallback`, whether this is the very first
    /// such attach against this daemon (spawn a default shell directly,
    /// matching `cmd-d`'s own spawn args) or a later one (open the
    /// picker with no leaf yet to bind into, per
    /// `confirm_attach_menu`'s new no-`focused` branch).
    async fn bootstrap_empty_workspace(
        &mut self,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        let available = match self
            .request(write_half, reader, Request::ConsumeShellFallback)
            .await?
        {
            Response::ShellFallback { available } => available,
            _ => false,
        };
        if available {
            let Response::ServerPane(server) = self
                .request(
                    write_half,
                    reader,
                    Request::ServerSpawn {
                        name: None,
                        cmd: None,
                        cwd: None,
                    },
                )
                .await?
            else {
                return Ok(());
            };
            let req = Request::ClientSpawn {
                workspace: self.workspace.id.to_string(),
                split_of: None,
                dir: None,
                bind: Some(server.id.to_string()),
            };
            if let Response::ClientPaneCreated { pane, .. } =
                self.request(write_half, reader, req).await?
            {
                self.focused = Some(pane);
            }
            return Ok(());
        }
        if let Response::PinnedDirsList(pinned) = self
            .request(write_half, reader, Request::PinnedDirsList)
            .await?
        {
            self.pinned_dirs = pinned;
        }
        if let Response::ServerPaneList(servers) = self
            .request(write_half, reader, Request::ServerList)
            .await?
        {
            self.attach_menu = Some(AttachMenu {
                servers: group_servers_by_cwd(
                    filter_servers_for_menu(servers, self.agents_only_view),
                    &self.pinned_dirs,
                ),
                selected: 0,
                pending_delete: None,
                rename: None,
                previously_bound: None,
                spawn_in_group: None,
                adding_tab: false,
            });
        }
        Ok(())
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
                    self.reconcile_text_selection();
                }
            }
            Event::GridDelta { snapshot } => {
                self.grids.insert(snapshot.server_pane, snapshot);
            }
            Event::ServerPaneDied { server_pane } => {
                self.grids.remove(&server_pane);
                if self
                    .text_selection
                    .as_ref()
                    .is_some_and(|selection| selection.server_pane() == server_pane)
                {
                    self.text_selection = None;
                }
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
        let still_valid = self.focused.is_some_and(|id| {
            self.workspace
                .tree
                .as_ref()
                .is_some_and(|t| t.find(id).is_some())
        });
        if !still_valid {
            self.focused = first_leaf(&self.workspace);
        }
    }

    fn reconcile_text_selection(&mut self) {
        let still_valid = self.text_selection.as_ref().is_some_and(|selection| {
            self.workspace
                .tree
                .as_ref()
                .and_then(|tree| tree.find(selection.pane()))
                .and_then(|pane| pane.active_bound())
                == Some(selection.server_pane())
                && self.grids.contains_key(&selection.server_pane())
        });
        if !still_valid {
            self.text_selection = None;
        }
    }

    async fn switch_workspace(
        &mut self,
        n: u8,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        let old = self.workspace.id;
        let _ = self
            .request(write_half, reader, Request::Unsubscribe { workspace: old })
            .await?;
        let resp = self
            .request(
                write_half,
                reader,
                Request::Subscribe {
                    workspace: n.to_string(),
                },
            )
            .await?;
        if let Response::Snapshot { workspace, grids } = resp {
            self.focused = first_leaf(&workspace);
            self.workspace = workspace;
            self.grids = grids.into_iter().map(|g| (g.server_pane, g)).collect();
            self.attach_menu = None;
            self.dragging_split = None;
            self.text_selection = None;
        }
        Ok(())
    }

    /// `cmd-d`/`cmd-shift-d`: split the focused pane and immediately bind
    /// a freshly spawned server-pane into the new half — no menu. (The
    /// design doc originally specified a picker here; the picker was
    /// removed entirely in favor of this direct "split + new shell"
    /// shortcut. Binding a client-pane to an *existing* server-pane, e.g.
    /// to display one shell in two places, is still possible via the CLI:
    /// `dimax client bind <workspace>/<pane> <server-name>`.)
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
            .request(
                write_half,
                reader,
                Request::ServerSpawn {
                    name: None,
                    cmd: None,
                    cwd: None,
                },
            )
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
        if let Response::ClientPaneCreated { pane, .. } =
            self.request(write_half, reader, req).await?
        {
            self.focused = Some(pane);
        }
        Ok(())
    }

    /// `cmd-shift-w`: kill the server-pane bound to the focused
    /// client-pane, if any.
    async fn kill_focused_server_pane(
        &mut self,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        let Some(pane) = self.focused else {
            return Ok(());
        };
        let Some(bound) = self
            .workspace
            .tree
            .as_ref()
            .and_then(|t| t.find(pane))
            .and_then(|p| p.active_bound())
        else {
            return Ok(());
        };
        let req = Request::ServerKill {
            target: bound.to_string(),
        };
        let _ = self.request(write_half, reader, req).await?;
        Ok(())
    }

    /// `cmd-shift-z`: detach the focused client-pane from its current
    /// server-pane (which keeps running — see `state::client_unbind`) and
    /// open the attach menu so a replacement can be picked. A no-op if
    /// nothing is focused (empty workspace).
    ///
    /// On a leaf with more than one tab, this deliberately skips sending
    /// `ClientUnbind`: unbinding now *removes* the active tab from the
    /// list rather than just blanking a slot, so on a multi-tab leaf it
    /// would shift every later tab's index down by one before the
    /// picker's eventual `ClientBind` replaces "the active tab" -- which
    /// would then land in the wrong slot and silently drop a sibling tab
    /// (see design doc "TUI": "the user never loses tabs 2 and 3 by
    /// re-picking tab 1's binding"). `ClientBind` already replaces the
    /// active tab in place, so nothing needs to be pre-cleared for it.
    async fn detach_and_open_menu(
        &mut self,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        let Some(pane) = self.focused else {
            return Ok(());
        };
        let leaf = self
            .workspace
            .tree
            .as_ref()
            .and_then(|tree| tree.find(pane));
        // Read the current binding from local workspace state *before*
        // unbinding -- see `AttachMenu.previously_bound`'s doc comment
        // for why this has to happen here, not after `ClientUnbind`
        // lands (the binding is gone server-side by then too).
        let previously_bound = leaf.and_then(|leaf| leaf.active_bound());
        let has_multiple_tabs = leaf.is_some_and(|leaf| leaf.tabs.len() > 1);
        if !has_multiple_tabs {
            let req = Request::ClientUnbind {
                workspace: self.workspace.id.to_string(),
                pane,
            };
            let _ = self.request(write_half, reader, req).await?;
        }
        if let Response::PinnedDirsList(pinned) = self
            .request(write_half, reader, Request::PinnedDirsList)
            .await?
        {
            self.pinned_dirs = pinned;
        }
        if let Response::ServerPaneList(servers) = self
            .request(write_half, reader, Request::ServerList)
            .await?
        {
            let grouped = group_servers_by_cwd(
                filter_servers_for_menu(servers, self.agents_only_view),
                &self.pinned_dirs,
            );
            let selected = initial_selection_for(
                &grouped,
                &self.collapsed_groups,
                self.grouped_view,
                previously_bound,
            );
            self.attach_menu = Some(AttachMenu {
                servers: grouped,
                selected,
                pending_delete: None,
                rename: None,
                previously_bound,
                spawn_in_group: None,
                adding_tab: false,
            });
        }
        Ok(())
    }

    /// `cmd-t`: open the attach menu in add-tab mode. Unlike
    /// `detach_and_open_menu` this deliberately does NOT unbind first —
    /// the focused client-pane keeps every tab it has, and the pick is
    /// appended as a new one (see `AttachMenu.adding_tab`). Consequently
    /// there is no `previously_bound` to mark: nothing was detached.
    async fn open_add_tab_menu(
        &mut self,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        let Some(_pane) = self.focused else {
            return Ok(());
        };
        if let Response::PinnedDirsList(pinned) = self
            .request(write_half, reader, Request::PinnedDirsList)
            .await?
        {
            self.pinned_dirs = pinned;
        }
        if let Response::ServerPaneList(servers) = self
            .request(write_half, reader, Request::ServerList)
            .await?
        {
            let grouped = group_servers_by_cwd(
                filter_servers_for_menu(servers, self.agents_only_view),
                &self.pinned_dirs,
            );
            // Default to the trailing `SpawnNew` row ("generic new tab")
            // rather than row 0 -- cmd-t is for opening a fresh tab, not
            // for re-picking an existing session, so the cursor should
            // start on the row that does that with a single Enter press.
            let selected =
                visible_attach_menu_rows(&grouped, &self.collapsed_groups, self.grouped_view)
                    .len()
                    .saturating_sub(1);
            self.attach_menu = Some(AttachMenu {
                servers: grouped,
                selected,
                pending_delete: None,
                rename: None,
                previously_bound: None,
                spawn_in_group: None,
                adding_tab: true,
            });
        }
        Ok(())
    }

    /// `cmd-]` / `cmd-[`: step the focused client-pane's active tab one
    /// position forward/back, wrapping. The daemon owns the wrapping and
    /// the resulting `LayoutDelta` broadcast; this only forwards the
    /// intent.
    async fn cycle_tab(
        &mut self,
        forward: bool,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        let Some(pane) = self.focused else {
            return Ok(());
        };
        let req = Request::ClientCycleTab {
            workspace: self.workspace.id.to_string(),
            pane,
            forward,
        };
        let _ = self.request(write_half, reader, req).await?;
        Ok(())
    }

    /// `cmd-w`: drop the focused client-pane's active tab AND kill its
    /// bound server-pane -- unlike `dimax client close`/`ClientCloseTab`
    /// alone (which deliberately leaves the server-pane running for
    /// scripted callers), this chord is meant to fully clean up what it
    /// was looking at, the same way `cmd-shift-w` kills on demand. When
    /// that was the pane's only tab the daemon closes the whole leaf
    /// instead (see `state::client_close_tab`), so this degrades to the
    /// old close-the-pane behavior on a single-tab leaf rather than
    /// needing a separate chord -- in that leaf-closing case focus
    /// reassignment happens via `reconcile_focus` once the resulting
    /// `LayoutDelta` lands, rather than being computed here from a tree
    /// this function doesn't have an up-to-date copy of yet.
    ///
    /// A pane that's *already* unbound (no tabs at all -- e.g. right
    /// after detaching via `cmd-shift-z` and backing out of the picker)
    /// is a complete no-op: there is no tab to drop and nothing to kill,
    /// so unlike the daemon's own `client_close_tab` (which treats an
    /// empty leaf the same as "just removed its last tab" and closes it
    /// -- a reasonable default for a scripted `dimax client close-tab`
    /// caller), cmd-w must not fall through to removing the pane from
    /// the layout. Losing a grid slot you never asked to close is
    /// exactly the surprise this chord already avoids for the
    /// multi-tab case.
    async fn close_tab(
        &mut self,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        let Some(pane) = self.focused else {
            return Ok(());
        };
        // Read the active tab's binding *before* closing it -- once
        // `ClientCloseTab` lands, the tab (and, if it was the last one,
        // the leaf itself) is already gone, so there's no later point
        // at which this could still be read back (same reasoning as
        // `AttachMenu.previously_bound`'s doc comment). `None` here means
        // the leaf has no tabs at all, not just that the active one
        // happens to be unbound (every leaf with a non-empty `tabs` has
        // a valid `active_tab` index, so `active_bound` only returns
        // `None` when `tabs` is empty) -- see this method's doc comment
        // for why that case returns early instead of proceeding.
        let Some(bound) = self
            .workspace
            .tree
            .as_ref()
            .and_then(|t| t.find(pane))
            .and_then(|p| p.active_bound())
        else {
            return Ok(());
        };
        let req = Request::ClientCloseTab {
            workspace: self.workspace.id.to_string(),
            pane,
        };
        let _ = self.request(write_half, reader, req).await?;
        let req = Request::ServerKill {
            target: bound.to_string(),
        };
        let _ = self.request(write_half, reader, req).await?;
        Ok(())
    }

    /// Route one raw byte chunk while the attach menu is open. A distinct,
    /// simpler modal grammar than the main keymap (same approach the
    /// removed picker used) rather than reusing `keys::parse`'s chords.
    /// Returns `true` when the input resolved to `AttachMenuAction::Quit`
    /// -- the caller's read loop must break in that case, same as
    /// `handle_action`'s return value outside the menu (see its doc
    /// comment). Every other path through this function -- renaming,
    /// editing a spawn-in-group field, an armed delete, or any other
    /// browse-mode action -- returns `false`.
    async fn handle_attach_menu_input(
        &mut self,
        bytes: &[u8],
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<bool> {
        let is_renaming = self
            .attach_menu
            .as_ref()
            .is_some_and(|m| m.rename.is_some());
        if is_renaming {
            match bytes {
                b"\r" | b"\n" => self.confirm_rename(write_half, reader).await?,
                b"\x1b" => {
                    if let Some(menu) = &mut self.attach_menu {
                        menu.rename = None;
                    }
                }
                _ => {
                    if let Some(menu) = &mut self.attach_menu
                        && let Some(rename) = &mut menu.rename
                    {
                        apply_rename_edit(rename, bytes);
                    }
                }
            }
            return Ok(false);
        }

        let is_spawning_in_group = self
            .attach_menu
            .as_ref()
            .is_some_and(|m| m.spawn_in_group.is_some());
        if is_spawning_in_group {
            match bytes {
                // Plain Enter: spawn, bind into the just-detached
                // client-pane, then send the typed text. Shift+Enter
                // (the `\x1b_Ds\x1b\\` chord -- see `keys.rs` module
                // doc's chord table): spawn and send, but leave it
                // unbound so it doesn't disturb the current pane.
                b"\r" | b"\n" => {
                    self.confirm_spawn_in_group(true, write_half, reader)
                        .await?
                }
                keys::SHIFT_ENTER_CHORD => {
                    self.confirm_spawn_in_group(false, write_half, reader)
                        .await?
                }
                b"\x1b" => {
                    if let Some(menu) = &mut self.attach_menu {
                        menu.spawn_in_group = None;
                    }
                }
                _ => {
                    if let Some(menu) = &mut self.attach_menu
                        && let Some(spawn) = &mut menu.spawn_in_group
                    {
                        apply_spawn_in_group_edit(spawn, bytes);
                    }
                }
            }
            return Ok(false);
        }

        let has_pending_delete = self
            .attach_menu
            .as_ref()
            .is_some_and(|m| m.pending_delete.is_some());
        if has_pending_delete {
            let mut should_quit = false;
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
                    should_quit = self
                        .dispatch_attach_menu_action(
                            parse_attach_menu_input(bytes),
                            write_half,
                            reader,
                        )
                        .await?;
                }
            }
            return Ok(should_quit);
        }

        // A group's own "+ spawn new in <dir>" row gets its own
        // dispatch, distinct from every other row's: only Up/Down
        // (arrows or `j`/`k`) still navigate away from it; Enter/Esc
        // keep their usual meaning (spawn now with no typed command /
        // close the menu); a literal space opens this row's inline
        // field, empty, ready to type a command into. Everything else
        // -- `x`/`r`/`p`/`a`/`f`/`g`/`d`/`q` included -- falls through to
        // the normal action dispatch below rather than being swallowed
        // as the first character of a typed command: those are the
        // menu's filter/pin/detach toggles, and a user pressing one of
        // them while a spawn row happens to be selected (easy to land
        // on, e.g. a lone-pane group's only other row) expects it to
        // toggle the filter, not silently open a text field instead.
        if let Some(AttachMenuRow::SpawnNewInGroup(server_index)) = self.selected_attach_menu_row()
        {
            match bytes {
                b"\x1b[A" | b"k" => self.move_attach_menu_selection(false),
                b"\x1b[B" | b"j" => self.move_attach_menu_selection(true),
                b"\r" | b"\n" => self.confirm_attach_menu(write_half, reader).await?,
                b"\x1b" => self.attach_menu = None,
                b" " => {
                    if let Some(menu) = &mut self.attach_menu {
                        menu.spawn_in_group = Some(SpawnInGroupState {
                            group_server_index: server_index,
                            text: String::new(),
                            cursor: 0,
                            error: None,
                        });
                    }
                }
                _ => {
                    return self
                        .dispatch_attach_menu_action(
                            parse_attach_menu_input(bytes),
                            write_half,
                            reader,
                        )
                        .await;
                }
            }
            return Ok(false);
        }

        self.dispatch_attach_menu_action(parse_attach_menu_input(bytes), write_half, reader)
            .await
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
    ) -> anyhow::Result<bool> {
        match action {
            AttachMenuAction::Up => self.move_attach_menu_selection(false),
            AttachMenuAction::Down => self.move_attach_menu_selection(true),
            AttachMenuAction::Cancel => {
                self.cancel_and_restore_previous_binding(write_half, reader)
                    .await?
            }
            AttachMenuAction::Confirm => {
                self.confirm_or_toggle_attach_menu(write_half, reader)
                    .await?
            }
            AttachMenuAction::Delete => self.arm_delete(),
            AttachMenuAction::StartRename => self.start_rename(),
            AttachMenuAction::TogglePin => self.toggle_directory_pin(write_half, reader).await?,
            AttachMenuAction::ToggleAgentsOnly => {
                self.toggle_agents_only(write_half, reader).await?
            }
            AttachMenuAction::ToggleGrouping => self.toggle_grouping(),
            AttachMenuAction::DetachAll => {
                self.detach_all_and_close_menu(write_half, reader).await?
            }
            AttachMenuAction::Quit => return Ok(true),
            AttachMenuAction::Ignore => {}
        }
        Ok(false)
    }

    /// The row `menu.selected` currently names, per
    /// `visible_attach_menu_rows` -- `None` only if the menu has become
    /// empty of rows entirely, which can't happen (there's always at
    /// least the trailing `SpawnNew` row).
    fn selected_attach_menu_row(&self) -> Option<AttachMenuRow> {
        let menu = self.attach_menu.as_ref()?;
        let rows =
            visible_attach_menu_rows(&menu.servers, &self.collapsed_groups, self.grouped_view);
        rows.get(menu.selected).copied()
    }

    /// Keep `attach_menu_preview` in sync with whatever server-pane row
    /// is currently selected: a no-op if the menu is closed or the
    /// selection isn't on a `Server` row (clears any stale preview from
    /// a previous selection in that case, so a header/spawn row never
    /// shows the last real pane's leftover content); otherwise fetches
    /// that pane's current screen via `Request::ServerRead` -- the same
    /// wire request `dimax server read` uses, chosen because it needs
    /// no workspace/client-pane binding at all, matching how little the
    /// rest of the attach menu (`ServerRead`'s siblings `ServerKill`/
    /// `ServerRename`) needs to reach a pane.
    ///
    /// Called from `run`'s loop on every iteration the menu is open
    /// (not just when the selection changes), so the preview tracks a
    /// pane's *live* output rather than freezing at the moment it was
    /// selected -- e.g. watching a build's output scroll by without
    /// having to re-select the row after every line.
    async fn refresh_attach_menu_preview(
        &mut self,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        let Some(AttachMenuRow::Server(server_index)) = self.selected_attach_menu_row() else {
            self.attach_menu_preview = None;
            return Ok(());
        };
        let Some(menu) = &self.attach_menu else {
            return Ok(());
        };
        let target = menu.servers[server_index].1.id;
        let req = Request::ServerRead {
            target: target.to_string(),
        };
        if let Response::ServerReadOutput { text } = self.request(write_half, reader, req).await? {
            self.attach_menu_preview = Some((target, text));
        }
        Ok(())
    }

    /// Refresh `server_names` from a full `ServerList` fetch -- called
    /// on the same tick as `refresh_attach_menu_preview` so every grid
    /// leaf's title bar (see `render::draw_leaf`) tracks a bound
    /// server-pane's current name shortly after a rename, whether or
    /// not the attach menu happens to be open.
    async fn refresh_server_names(
        &mut self,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        if let Response::ServerPaneList(servers) = self
            .request(write_half, reader, Request::ServerList)
            .await?
        {
            self.server_names = servers.into_iter().map(|s| (s.id, s)).collect();
        }
        Ok(())
    }

    fn move_attach_menu_selection(&mut self, forward: bool) {
        let collapsed = self.collapsed_groups.clone();
        let Some(menu) = &mut self.attach_menu else {
            return;
        };
        let len = visible_attach_menu_rows(&menu.servers, &collapsed, self.grouped_view).len();
        menu.selected = if forward {
            (menu.selected + 1) % len
        } else {
            (menu.selected + len - 1) % len
        };
    }

    /// Enter/`\r` on a group header toggles that group's collapse; on a
    /// real server-pane row or the trailing "spawn new" row it binds, via
    /// `confirm_attach_menu`, unchanged from before headers became
    /// selectable.
    async fn confirm_or_toggle_attach_menu(
        &mut self,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        match self.selected_attach_menu_row() {
            Some(AttachMenuRow::GroupHeader(server_index)) => {
                self.toggle_group_collapse(server_index);
                Ok(())
            }
            _ => self.confirm_attach_menu(write_half, reader).await,
        }
    }

    /// Flip the collapsed state of the group whose first member sits at
    /// `menu.servers[server_index]`. A no-op if `attach_menu` is `None`.
    fn toggle_group_collapse(&mut self, server_index: usize) {
        let Some(menu) = &self.attach_menu else {
            return;
        };
        let key = menu.servers[server_index].0.clone();
        if !self.collapsed_groups.remove(&key) {
            self.collapsed_groups.insert(key);
        }
        // Collapsing can shrink the visible row list out from under
        // `selected` (e.g. the header itself was the last visible row
        // before other groups' rows disappeared below it) -- clamp back
        // onto the new last row rather than leaving an out-of-bounds
        // index for the next lookup/render to trip over.
        let collapsed = self.collapsed_groups.clone();
        let Some(menu) = &mut self.attach_menu else {
            return;
        };
        let len = visible_attach_menu_rows(&menu.servers, &collapsed, self.grouped_view).len();
        if menu.selected >= len {
            menu.selected = len - 1;
        }
    }

    /// `g` on any row: flip between grouped-by-cwd and a flat row list.
    /// Purely local -- `menu.servers` is already sorted/grouped by
    /// `group_servers_by_cwd`; this only changes whether
    /// `visible_attach_menu_rows` emits header/spawn-in-group rows on top
    /// of that existing order, so no re-fetch is needed. Clamps
    /// `selected` the same way `toggle_group_collapse` does, since
    /// flattening removes every `GroupHeader`/`SpawnNewInGroup` row and
    /// can shrink the list out from under the cursor.
    fn toggle_grouping(&mut self) {
        self.grouped_view = !self.grouped_view;
        let collapsed = self.collapsed_groups.clone();
        let Some(menu) = &mut self.attach_menu else {
            return;
        };
        let len = visible_attach_menu_rows(&menu.servers, &collapsed, self.grouped_view).len();
        if menu.selected >= len {
            menu.selected = len - 1;
        }
    }

    /// `p` on a group header, or on a server row: pin that row's
    /// directory if it isn't pinned yet, unpin it if it is (see
    /// `state::State::toggle_pinned_dir`'s doc comment for the exact
    /// ordering rule). Both row kinds carry the same group-key index
    /// into `menu.servers` (see `AttachMenuRow::Server`/`GroupHeader`'s
    /// own doc comments), so a server row works identically to its
    /// group's header -- the only way to pin at all in the flat/
    /// ungrouped view (`App.grouped_view`), which has no header rows.
    /// A no-op on a spawn/spawn-new row -- neither carries a directory
    /// of its own to pin (`SpawnNewInGroup` carries one, but that row's
    /// own dispatch never reaches here -- see `handle_attach_menu_input`).
    /// Re-fetches and re-groups the server list afterward since pin
    /// order can move every group's position, not just the toggled
    /// one's, then clamps `selected` the same way `toggle_group_collapse`
    /// does in case the row list's length or the header's own position
    /// shifted out from under the cursor.
    async fn toggle_directory_pin(
        &mut self,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        let server_index = match self.selected_attach_menu_row() {
            Some(
                AttachMenuRow::GroupHeader(server_index) | AttachMenuRow::Server(server_index),
            ) => server_index,
            _ => return Ok(()),
        };
        let Some(menu) = &self.attach_menu else {
            return Ok(());
        };
        let dir = menu.servers[server_index].0.clone();

        if let Response::PinnedDirsList(pinned) = self
            .request(write_half, reader, Request::ToggleDirectoryPin { dir })
            .await?
        {
            self.pinned_dirs = pinned;
        }
        if let Response::ServerPaneList(servers) = self
            .request(write_half, reader, Request::ServerList)
            .await?
        {
            let grouped = group_servers_by_cwd(
                filter_servers_for_menu(servers, self.agents_only_view),
                &self.pinned_dirs,
            );
            let collapsed = self.collapsed_groups.clone();
            let Some(menu) = &mut self.attach_menu else {
                return Ok(());
            };
            menu.servers = grouped;
            let len = visible_attach_menu_rows(&menu.servers, &collapsed, self.grouped_view).len();
            if menu.selected >= len {
                menu.selected = len - 1;
            }
        }
        Ok(())
    }

    /// `f` on any row: flip `App.agents_only_view` and re-fetch/re-
    /// filter so the toggle takes effect immediately. Called out here
    /// from a separate method rather than folding it into
    /// `toggle_directory_pin`/`toggle_grouping` because it needs its own
    /// network round-trip and its own clamp step even though the
    /// mutation is different -- keeping it separate is clearer than a
    /// multi-flag "which one changed" parameter.
    async fn toggle_agents_only(
        &mut self,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        self.agents_only_view = !self.agents_only_view;
        if let Response::ServerPaneList(servers) = self
            .request(write_half, reader, Request::ServerList)
            .await?
        {
            let grouped = group_servers_by_cwd(
                filter_servers_for_menu(servers, self.agents_only_view),
                &self.pinned_dirs,
            );
            let collapsed = self.collapsed_groups.clone();
            let Some(menu) = &mut self.attach_menu else {
                return Ok(());
            };
            menu.servers = grouped;
            let len = visible_attach_menu_rows(&menu.servers, &collapsed, self.grouped_view).len();
            if menu.selected >= len {
                menu.selected = len.saturating_sub(1);
            }
        }
        Ok(())
    }

    /// `Esc`: cancel the pick and close the menu. If the leaf that
    /// opened this menu was actually detached to get here (the
    /// single-tab `cmd-shift-z` case -- see `AttachMenu.previously_bound`'s
    /// doc comment), rebind it so cancelling is a true no-op rather than
    /// leaving the leaf unbound. `previously_bound` is `None` for every
    /// other way the menu can open (a multi-tab `cmd-shift-z`, which
    /// deliberately skips the unbind; `cmd-t`'s add-tab mode, which never
    /// unbinds at all; or the startup picker on an empty workspace, which
    /// has no leaf to restore) -- in all of those, this is just a plain
    /// close.
    async fn cancel_and_restore_previous_binding(
        &mut self,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        let previously_bound = self
            .attach_menu
            .take()
            .and_then(|menu| menu.previously_bound);
        let (Some(pane), Some(target)) = (self.focused, previously_bound) else {
            return Ok(());
        };
        let req = Request::ClientBind {
            workspace: self.workspace.id.to_string(),
            pane,
            target: target.to_string(),
        };
        let _ = self.request(write_half, reader, req).await?;
        Ok(())
    }

    /// `d`: detach every tab from the leaf that opened this menu, then
    /// close the menu -- the escape hatch for "I don't want anything
    /// bound here anymore", distinct from `Esc`'s "restore what was
    /// there". `ClientUnbind` only ever removes the *active* tab, so
    /// this repeats it until the leaf reports no tabs left (or a bound
    /// number of iterations elapses, as a defensive backstop against an
    /// unexpected daemon response looping forever).
    async fn detach_all_and_close_menu(
        &mut self,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        self.attach_menu = None;
        let Some(pane) = self.focused else {
            return Ok(());
        };
        for _ in 0..64 {
            let still_bound = self
                .workspace
                .tree
                .as_ref()
                .and_then(|tree| tree.find(pane))
                .is_some_and(|leaf| !leaf.tabs.is_empty());
            if !still_bound {
                break;
            }
            let req = Request::ClientUnbind {
                workspace: self.workspace.id.to_string(),
                pane,
            };
            let _ = self.request(write_half, reader, req).await?;
        }
        Ok(())
    }

    /// Arm the selected row's deletion (first `x`) if it names a real
    /// server-pane row; a no-op on a group header or the trailing "spawn
    /// new" row (nothing to delete there).
    fn arm_delete(&mut self) {
        let Some(AttachMenuRow::Server(server_index)) = self.selected_attach_menu_row() else {
            return;
        };
        let Some(menu) = &mut self.attach_menu else {
            return;
        };
        menu.pending_delete = Some(server_index);
    }

    /// Confirm a previously-armed deletion: kill the server-pane, then
    /// re-fetch and re-group the server list so the menu reflects its
    /// removal. Clamps `selected` into the shrunk *visible* row list --
    /// if the deleted row was the last one, selection moves to the new
    /// last row; otherwise the numeric index is left as-is (which now
    /// names whatever slid up into that slot, an acceptable "selection
    /// moved on" side effect of any list shrinking under the cursor).
    async fn confirm_delete(
        &mut self,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        let Some(menu) = &mut self.attach_menu else {
            return Ok(());
        };
        let Some(index) = menu.pending_delete.take() else {
            return Ok(());
        };
        let target = menu.servers[index].1.id.to_string();
        let _ = self
            .request(write_half, reader, Request::ServerKill { target })
            .await?;
        if let Response::ServerPaneList(servers) = self
            .request(write_half, reader, Request::ServerList)
            .await?
        {
            let grouped = group_servers_by_cwd(
                filter_servers_for_menu(servers, self.agents_only_view),
                &self.pinned_dirs,
            );
            let collapsed = self.collapsed_groups.clone();
            let Some(menu) = &mut self.attach_menu else {
                return Ok(());
            };
            menu.servers = grouped;
            let len = visible_attach_menu_rows(&menu.servers, &collapsed, self.grouped_view).len();
            if menu.selected >= len {
                menu.selected = len - 1;
            }
        }
        Ok(())
    }

    /// `r` on a real server-pane row: open the inline rename field,
    /// pre-filled with its current custom name (empty if it's currently
    /// falling back to the id — see `render::attach_menu_line`'s
    /// `unwrap_or_else(|| short_id(...))`), cursor starting at the end
    /// of the pre-filled text. A no-op on a group header or the trailing
    /// "spawn new" row.
    fn start_rename(&mut self) {
        let Some(AttachMenuRow::Server(server_index)) = self.selected_attach_menu_row() else {
            return;
        };
        let Some(menu) = &mut self.attach_menu else {
            return;
        };
        let text = menu.servers[server_index]
            .1
            .name
            .clone()
            .unwrap_or_default();
        let cursor = text.len();
        menu.rename = Some(RenameState {
            index: server_index,
            text,
            cursor,
            error: None,
        });
    }

    /// Submit the active rename field's current text: a no-op if empty
    /// (stays in rename mode rather than sending an empty name), sends
    /// `Request::ServerRename` otherwise. On success, re-fetches and
    /// re-groups the server list and closes rename mode. On
    /// `Response::Error` (e.g. a name collision), records the message in
    /// `rename.error` and stays open for another attempt.
    async fn confirm_rename(
        &mut self,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        let Some(menu) = &self.attach_menu else {
            return Ok(());
        };
        let Some(rename) = &menu.rename else {
            return Ok(());
        };
        if rename.text.is_empty() {
            return Ok(());
        }
        let target = menu.servers[rename.index].1.id.to_string();
        let new_name = rename.text.clone();
        let req = Request::ServerRename { target, new_name };
        match self.request(write_half, reader, req).await? {
            Response::Ack => {
                if let Response::ServerPaneList(servers) = self
                    .request(write_half, reader, Request::ServerList)
                    .await?
                {
                    let grouped = group_servers_by_cwd(
                        filter_servers_for_menu(servers, self.agents_only_view),
                        &self.pinned_dirs,
                    );
                    let Some(menu) = &mut self.attach_menu else {
                        return Ok(());
                    };
                    menu.servers = grouped;
                    menu.rename = None;
                }
            }
            Response::Error { message } => {
                if let Some(menu) = &mut self.attach_menu
                    && let Some(rename) = &mut menu.rename
                {
                    rename.error = Some(message);
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Confirm the attach menu's current selection: bind the just-detached
    /// client-pane to the selected server-pane, spawning a fresh one first
    /// if the trailing "spawn new" row (no particular cwd) or a group's
    /// own "+ spawn new here" row (spawns with that group's cwd) is
    /// selected. Only ever called (via `confirm_or_toggle_attach_menu`)
    /// when the selected row is a `Server`, `SpawnNewInGroup`, or
    /// `SpawnNew` row -- a `GroupHeader` is intercepted before this runs.
    async fn confirm_attach_menu(
        &mut self,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        let Some(row) = self.selected_attach_menu_row() else {
            return Ok(());
        };
        let Some(menu) = self.attach_menu.take() else {
            return Ok(());
        };
        let adding_tab = menu.adding_tab;
        let target = match row {
            AttachMenuRow::Server(server_index) => menu.servers[server_index].1.id.to_string(),
            AttachMenuRow::SpawnNew => match self
                .request(
                    write_half,
                    reader,
                    Request::ServerSpawn {
                        name: None,
                        cmd: None,
                        cwd: None,
                    },
                )
                .await?
            {
                Response::ServerPane(info) => info.id.to_string(),
                _ => return Ok(()),
            },
            AttachMenuRow::SpawnNewInGroup(server_index) => {
                let cwd = menu.servers[server_index].0.clone();
                match self
                    .request(
                        write_half,
                        reader,
                        Request::ServerSpawn {
                            name: None,
                            cmd: None,
                            cwd: Some(cwd),
                        },
                    )
                    .await?
                {
                    Response::ServerPane(info) => info.id.to_string(),
                    _ => return Ok(()),
                }
            }
            AttachMenuRow::GroupHeader(_) => return Ok(()),
        };
        match self.focused {
            Some(pane) if adding_tab => {
                let req = Request::ClientAddTab {
                    workspace: self.workspace.id.to_string(),
                    pane,
                    target,
                };
                let _ = self.request(write_half, reader, req).await?;
            }
            Some(pane) => {
                let req = Request::ClientBind {
                    workspace: self.workspace.id.to_string(),
                    pane,
                    target,
                };
                let _ = self.request(write_half, reader, req).await?;
            }
            None => {
                // No leaf exists yet -- this is the startup picker on an
                // empty workspace (see `bootstrap_empty_workspace`).
                // `adding_tab` is always `false` here (there's no leaf
                // to append a tab to), so no branch on it is needed.
                let req = Request::ClientSpawn {
                    workspace: self.workspace.id.to_string(),
                    split_of: None,
                    dir: None,
                    bind: Some(target),
                };
                if let Response::ClientPaneCreated { pane, .. } =
                    self.request(write_half, reader, req).await?
                {
                    self.focused = Some(pane);
                }
            }
        }
        Ok(())
    }

    /// Confirm a group's inline spawn field: spawn a new default-shell
    /// server-pane in that group's directory, then (if `bind`) bind it
    /// into the just-detached client-pane, then (if the field's text is
    /// non-empty) send that text into the new pane followed by Enter, as
    /// if typed live. `bind = true` is plain Enter, which closes the
    /// attach menu on success (same as `confirm_attach_menu` -- binding
    /// is "commit and go"). `bind = false` is Shift+Enter, which spawns
    /// and sends but deliberately leaves the pane unbound -- per the
    /// design intent that this row not force a bind just to run a
    /// one-off command in that directory -- and correspondingly leaves
    /// the menu *open* rather than closing it: Shift+Enter's whole point
    /// is firing off a quick command without disturbing the current
    /// pane, so staying in the menu to spawn/inspect more (rather than
    /// having to reopen it) matches that same "don't disturb the
    /// current flow" intent. The just-confirmed field is cleared and
    /// the server list re-fetched either way, so the new pane appears
    /// in the row list immediately when the menu stays open.
    async fn confirm_spawn_in_group(
        &mut self,
        bind: bool,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        let Some(menu) = &self.attach_menu else {
            return Ok(());
        };
        let Some(spawn) = &menu.spawn_in_group else {
            return Ok(());
        };
        let cwd = menu.servers[spawn.group_server_index].0.clone();
        let text = spawn.text.clone();

        let server_pane = match self
            .request(
                write_half,
                reader,
                Request::ServerSpawn {
                    name: None,
                    cmd: None,
                    cwd: Some(cwd),
                },
            )
            .await?
        {
            Response::ServerPane(info) => info.id,
            _ => return Ok(()),
        };

        if bind && let Some(pane) = self.focused {
            let adding_tab = self.attach_menu.as_ref().is_some_and(|m| m.adding_tab);
            let req = if adding_tab {
                Request::ClientAddTab {
                    workspace: self.workspace.id.to_string(),
                    pane,
                    target: server_pane.to_string(),
                }
            } else {
                Request::ClientBind {
                    workspace: self.workspace.id.to_string(),
                    pane,
                    target: server_pane.to_string(),
                }
            };
            let _ = self.request(write_half, reader, req).await?;
        }

        if !text.is_empty() {
            let req = Request::ServerSend {
                target: server_pane.to_string(),
                text,
                enter: true,
            };
            let _ = self.request(write_half, reader, req).await?;
        }

        if !bind {
            if let Response::ServerPaneList(servers) = self
                .request(write_half, reader, Request::ServerList)
                .await?
            {
                let grouped = group_servers_by_cwd(
                    filter_servers_for_menu(servers, self.agents_only_view),
                    &self.pinned_dirs,
                );
                let collapsed = self.collapsed_groups.clone();
                if let Some(menu) = &mut self.attach_menu {
                    menu.servers = grouped;
                    menu.spawn_in_group = None;
                    let len =
                        visible_attach_menu_rows(&menu.servers, &collapsed, self.grouped_view)
                            .len();
                    if menu.selected >= len {
                        menu.selected = len - 1;
                    }
                }
            }
            return Ok(());
        }

        self.attach_menu = None;
        Ok(())
    }

    fn move_focus(&mut self, direction: Direction) {
        if let Some(tree) = &self.workspace.tree
            && let Some(next) =
                nearest_leaf_in_direction(tree, self.frame_area, self.focused, direction)
        {
            self.focused = Some(next);
        }
    }

    /// Route one parsed [`mouse::MouseEvent`] — divider drag-resizing,
    /// click-to-focus, and pane-local text selection. `Down` hit-tests
    /// against the current layout's divider grab zones first (recomputed
    /// fresh every call via `render::divider_rects`, since the layout can
    /// change between mouse events); if that misses, it focuses the leaf
    /// under the pointer and starts a selection when the pointer is over
    /// that leaf's terminal content. `Drag` updates either
    /// `dragging_split`'s live ratio or the active text range purely
    /// locally — no request is sent — so both interactions render
    /// smoothly without flooding the daemon (see the field doc on
    /// `dragging_split` for why this replaced the original "send on every
    /// move" design). `Up`
    /// commits a divider position with exactly one `Request::ResizeSplit`,
    /// or returns the completed selection's text for the event loop to
    /// copy through OSC 52. A plain click returns no text.
    async fn handle_mouse(
        &mut self,
        event: mouse::MouseEvent,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<Option<String>> {
        match event {
            mouse::MouseEvent::Down { col, row } => {
                self.dragging_split = None;
                self.text_selection = None;
                let Some(tree) = &self.workspace.tree else {
                    return Ok(None);
                };
                let hits = render::divider_rects(tree, self.frame_area);
                if let Some(split) = hit_test_dividers(&hits, col, row) {
                    let hit = hits.iter().find(|h| h.split == split).expect(
                        "hit_test_dividers only returns a split id that came from this exact `hits` list",
                    );
                    self.dragging_split = Some((split, render::ratio_at(hit, col, row)));
                } else if let Some((pane, rect)) = render::leaf_rects(tree, self.frame_area)
                    .into_iter()
                    .find(|(_, rect)| {
                        col >= rect.x
                            && col < rect.x + rect.width
                            && row >= rect.y
                            && row < rect.y + rect.height
                    })
                {
                    self.focused = Some(pane);
                    let server_pane = tree.find(pane).and_then(|pane| pane.active_bound());
                    if let Some(server_pane) = server_pane
                        && let Some(snapshot) = self.grids.get(&server_pane)
                        && let Some(position) = selection::position_at(rect, snapshot, col, row)
                    {
                        self.text_selection =
                            Some(selection::TextSelection::new(pane, server_pane, position));
                    }
                }
            }
            mouse::MouseEvent::ScrollUp { col, row } => {
                self.text_selection = None;
                self.scroll_pane_under(col, row, SCROLL_ROWS_PER_TICK, write_half, reader)
                    .await?;
            }
            mouse::MouseEvent::ScrollDown { col, row } => {
                self.text_selection = None;
                self.scroll_pane_under(col, row, -SCROLL_ROWS_PER_TICK, write_half, reader)
                    .await?;
            }
            mouse::MouseEvent::Drag { col, row } => {
                if let Some((split, _)) = self.dragging_split {
                    let Some(tree) = &self.workspace.tree else {
                        return Ok(None);
                    };
                    let Some(hit) = render::divider_rects(tree, self.frame_area)
                        .into_iter()
                        .find(|hit| hit.split == split)
                    else {
                        // The split disappeared from under the drag
                        // (e.g. closed by another frontend mid-drag).
                        self.dragging_split = None;
                        return Ok(None);
                    };
                    self.dragging_split = Some((split, render::ratio_at(&hit, col, row)));
                } else if let Some((pane, server_pane)) = self
                    .text_selection
                    .as_ref()
                    .filter(|selection| selection.is_dragging())
                    .map(|selection| (selection.pane(), selection.server_pane()))
                    && let Some(position) = self.selection_position(pane, server_pane, col, row)
                    && let Some(selection) = &mut self.text_selection
                {
                    selection.update(position);
                }
            }
            mouse::MouseEvent::Up { col, row } => {
                if let Some((split, ratio)) = self.dragging_split.take() {
                    let req = Request::ResizeSplit {
                        workspace: self.workspace.id.to_string(),
                        split,
                        new_ratio: ratio,
                    };
                    let _ = self.request(write_half, reader, req).await?;
                    return Ok(None);
                }

                let Some((pane, server_pane)) = self
                    .text_selection
                    .as_ref()
                    .filter(|selection| selection.is_dragging())
                    .map(|selection| (selection.pane(), selection.server_pane()))
                else {
                    return Ok(None);
                };
                let Some(position) = self.selection_position(pane, server_pane, col, row) else {
                    self.text_selection = None;
                    return Ok(None);
                };
                let selection = self
                    .text_selection
                    .as_mut()
                    .expect("the active selection was read immediately above and cannot disappear");
                if !selection.finish(position) {
                    self.text_selection = None;
                    return Ok(None);
                }
                let text = self
                    .grids
                    .get(&server_pane)
                    .and_then(|snapshot| selection::selected_text(snapshot, selection))
                    .filter(|text| !text.is_empty());
                return Ok(text);
            }
        }
        Ok(None)
    }

    fn selection_position(
        &self,
        pane: ClientPaneId,
        server_pane: ServerPaneId,
        col: u16,
        row: u16,
    ) -> Option<selection::GridPosition> {
        let tree = self.workspace.tree.as_ref()?;
        let (_, rect) = render::leaf_rects(tree, self.frame_area)
            .into_iter()
            .find(|(candidate, _)| *candidate == pane)?;
        let snapshot = self.grids.get(&server_pane)?;
        selection::clamped_position(rect, snapshot, col, row)
    }

    /// Hit-test `(col, row)` against the current workspace's leaves and
    /// send `Request::ScrollClientPane` for whichever one it landed
    /// over, if any -- not necessarily the focused pane (the wheel
    /// scrolls whatever's under the cursor). A hit over empty space is
    /// a no-op.
    async fn scroll_pane_under(
        &mut self,
        col: u16,
        row: u16,
        delta: i32,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        let Some(tree) = &self.workspace.tree else {
            return Ok(());
        };
        let hit = render::leaf_rects(tree, self.frame_area)
            .into_iter()
            .find(|(_, rect)| {
                col >= rect.x
                    && col < rect.x + rect.width
                    && row >= rect.y
                    && row < rect.y + rect.height
            });
        let Some((pane, _)) = hit else { return Ok(()) };
        let req = Request::ScrollClientPane { pane, delta };
        let _ = self.request(write_half, reader, req).await?;
        Ok(())
    }

    /// Route one resolved `Action` to its effect.
    /// Returns `true` when `action` was `Action::Quit` -- the caller's
    /// read loop must break out and exit `dimax attach` in that case,
    /// which this method (an ordinary `&mut self` action handler) has no
    /// way to do on its own.
    async fn handle_action(
        &mut self,
        action: Action,
        raw: &[u8],
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<bool> {
        self.text_selection = None;
        match action {
            Action::SwitchWorkspace(n) => self.switch_workspace(n, write_half, reader).await?,
            Action::JumpSession(n) => self.jump_session(n, write_half, reader).await?,
            Action::SplitVertical => self.split(SplitDir::Vertical, write_half, reader).await?,
            Action::SplitHorizontal => self.split(SplitDir::Horizontal, write_half, reader).await?,
            Action::CloseFocusedPane => self.close_tab(write_half, reader).await?,
            Action::KillFocusedServerPane => {
                self.kill_focused_server_pane(write_half, reader).await?
            }
            Action::DetachAndAttach => self.detach_and_open_menu(write_half, reader).await?,
            Action::AddTab => self.open_add_tab_menu(write_half, reader).await?,
            Action::CycleTabForward => self.cycle_tab(true, write_half, reader).await?,
            Action::CycleTabBackward => self.cycle_tab(false, write_half, reader).await?,
            Action::FocusLeft => self.move_focus(Direction::Left),
            Action::FocusRight => self.move_focus(Direction::Right),
            Action::FocusUp => self.move_focus(Direction::Up),
            Action::FocusDown => self.move_focus(Direction::Down),
            Action::Quit => return Ok(true),
            Action::PassThrough => {
                if let Some(pane) = self.focused {
                    let req = Request::Input {
                        pane,
                        bytes: raw.to_vec(),
                    };
                    let _ = self.request(write_half, reader, req).await?;
                }
            }
        }
        Ok(false)
    }

    /// Bind the focused client-pane to the nth server-pane in the
    /// daemon's stable, name-sorted list. Missing indices and an absent
    /// focused pane are no-ops; this action never creates a session.
    async fn jump_session(
        &mut self,
        number: u8,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        let Some(pane) = self.focused else {
            return Ok(());
        };
        let Response::ServerPaneList(servers) = self
            .request(write_half, reader, Request::ServerList)
            .await?
        else {
            return Ok(());
        };
        let Some(server) = servers.get(usize::from(number.saturating_sub(1))) else {
            return Ok(());
        };
        let req = Request::ClientBind {
            workspace: self.workspace.id.to_string(),
            pane,
            target: server.id.to_string(),
        };
        let _ = self.request(write_half, reader, req).await?;
        Ok(())
    }
}

/// First leaf (tree order) of `workspace`'s tree, or `None` if the
/// workspace is empty. Used both for the initial focus pick and whenever
/// the tree changes out from under the current focus.
fn first_leaf(workspace: &WorkspaceInfo) -> Option<ClientPaneId> {
    workspace
        .tree
        .as_ref()
        .and_then(|t| t.leaves().first().map(|p| p.id))
}

/// Find whichever leaf is nearest to `current` in the given screen
/// `direction`, using each leaf's actual on-screen `Rect` (via
/// `render::leaf_rects`) rather than tree order — see design doc
/// "Directional Focus Navigation" for why this now works where the
/// old tree-order `cycle_focus` was an intentional stand-in. `None` if
/// there is no leaf in that direction at all (no wraparound) or the
/// tree has zero leaves (cannot happen for a real `SplitTree`, but
/// handled defensively). If `current` is `None` or no longer names a
/// leaf in this tree (e.g. it was just closed elsewhere), lands on the
/// first leaf in `leaf_rects`' own order, matching `cycle_focus`'s old
/// fallback behavior for the same edge case.
fn nearest_leaf_in_direction(
    tree: &SplitTree,
    area: ratatui::layout::Rect,
    current: Option<ClientPaneId>,
    direction: Direction,
) -> Option<ClientPaneId> {
    let rects = render::leaf_rects(tree, area);
    if rects.is_empty() {
        return None;
    }
    let focused_rect =
        current.and_then(|id| rects.iter().find(|(pane, _)| *pane == id).map(|(_, r)| *r));
    let Some(focused_rect) = focused_rect else {
        return Some(rects[0].0);
    };
    rects
        .iter()
        .filter(|(_, rect)| is_in_direction(focused_rect, *rect, direction))
        .min_by_key(|(_, rect)| {
            let gap = axis_gap(focused_rect, *rect, direction);
            let overlap = perpendicular_overlap(focused_rect, *rect, direction);
            // Smallest gap wins; among equal gaps, the largest
            // perpendicular overlap wins (favors a pane actually
            // beside/above/below the focused one over a diagonal
            // neighbor that happens to be marginally closer). Negating
            // overlap turns "largest wins" into the same ascending
            // `min_by_key` comparison as the gap.
            (gap, i32::from(overlap).saturating_neg())
        })
        .map(|(pane, _)| *pane)
}

/// Whether `other` is positioned in `direction` relative to `focused`
/// -- strictly on that side, not overlapping it on the movement axis.
fn is_in_direction(
    focused: ratatui::layout::Rect,
    other: ratatui::layout::Rect,
    direction: Direction,
) -> bool {
    match direction {
        Direction::Left => other.x + other.width <= focused.x,
        Direction::Right => other.x >= focused.x + focused.width,
        Direction::Up => other.y + other.height <= focused.y,
        Direction::Down => other.y >= focused.y + focused.height,
    }
}

/// The gap between `focused` and `other` along the movement axis --
/// smaller means nearer. Only meaningful for a pair `is_in_direction`
/// already confirmed, so this never underflows.
fn axis_gap(
    focused: ratatui::layout::Rect,
    other: ratatui::layout::Rect,
    direction: Direction,
) -> u16 {
    match direction {
        Direction::Left => focused.x - (other.x + other.width),
        Direction::Right => other.x - (focused.x + focused.width),
        Direction::Up => focused.y - (other.y + other.height),
        Direction::Down => other.y - (focused.y + focused.height),
    }
}

/// How much `focused` and `other` overlap along the axis perpendicular
/// to `direction` -- larger means "more directly across from" rather
/// than a diagonal neighbor. Zero if they don't overlap on that axis
/// at all.
fn perpendicular_overlap(
    focused: ratatui::layout::Rect,
    other: ratatui::layout::Rect,
    direction: Direction,
) -> u16 {
    let (focused_start, focused_end, other_start, other_end) = match direction {
        Direction::Left | Direction::Right => (
            focused.y,
            focused.y + focused.height,
            other.y,
            other.y + other.height,
        ),
        Direction::Up | Direction::Down => (
            focused.x,
            focused.x + focused.width,
            other.x,
            other.x + other.width,
        ),
    };
    focused_end
        .min(other_end)
        .saturating_sub(focused_start.max(other_start))
}

/// Find whichever divider's grab zone contains `(col, row)`, if any. Pure
/// hit-testing logic factored out of `App::handle_mouse` so it's directly
/// unit-testable without a live connection.
fn hit_test_dividers(
    hits: &[render::DividerHit],
    col: u16,
    row: u16,
) -> Option<crate::protocol::SplitId> {
    hits.iter()
        .find(|hit| {
            col >= hit.grab_zone.x
                && col < hit.grab_zone.x + hit.grab_zone.width
                && row >= hit.grab_zone.y
                && row < hit.grab_zone.y + hit.grab_zone.height
        })
        .map(|hit| hit.split)
}

/// Diff each leaf's current on-screen `Rect` (from `leaf_rects`) against
/// `current` (a frontend's last-reported sizes), returning only the
/// panes whose size actually changed -- including a pane not yet
/// present in `current` at all (its first frame). Pure and
/// `App`-free so it's directly unit-testable; `run`'s loop calls this
/// every frame and only sends `Request::ResizeClientPane` for whatever
/// this returns, rather than unconditionally re-reporting every pane on
/// every frame.
///
/// Subtracts 1 from each leaf's rect height before comparing/returning
/// -- every leaf reserves its top row as a title-bar border (see
/// `render::draw_leaf`), so the *usable* grid area the PTY should
/// actually be sized to is one row shorter than the leaf's full
/// on-screen rect.
fn changed_pane_sizes(
    current: &HashMap<ClientPaneId, crate::protocol::Size>,
    tree: &SplitTree,
    area: ratatui::layout::Rect,
) -> Vec<(ClientPaneId, crate::protocol::Size)> {
    render::leaf_rects(tree, area)
        .into_iter()
        .filter_map(|(pane_id, rect)| {
            let size = crate::protocol::Size {
                rows: rect.height.saturating_sub(1),
                cols: rect.width,
            };
            if current.get(&pane_id) == Some(&size) {
                None
            } else {
                Some((pane_id, size))
            }
        })
        .collect()
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
/// One selectable row in the attach menu, in on-screen order. Headers are
/// selectable (Enter on one toggles that group's collapse) alongside the
/// server-pane rows and the trailing "spawn new" row -- `AttachMenu
/// .selected` indexes into whatever [`visible_attach_menu_rows`] returns,
/// not into `servers` directly, since a collapsed group's member rows are
/// omitted from that list entirely (skipped by navigation, not just
/// hidden visually).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttachMenuRow {
    /// A directory-group header. Carries the index of that group's first
    /// member in `AttachMenu.servers`, from which the group's key string
    /// (`servers[first_index].0`) and collapsed state can be looked up —
    /// cheaper than duplicating the key string into every row.
    GroupHeader(usize),
    /// A real server-pane row; the index into `AttachMenu.servers`.
    Server(usize),
    /// A group's own "+ spawn new here" row, right after its `Server`
    /// rows -- carries the same first-member index as that group's
    /// `GroupHeader`, from which its cwd key can be looked up. Omitted
    /// while that group is collapsed (see `visible_attach_menu_rows`) --
    /// a collapsed group hides all of its detail, this row included --
    /// and never emitted for the synthetic `"Unknown"` bucket at all: it
    /// aggregates panes with no resolvable directory, so there's no real
    /// cwd to spawn a new pane into.
    SpawnNewInGroup(usize),
    SpawnNew,
}

/// Build the attach menu's current row list: a `GroupHeader` before each
/// distinct cwd group's first member (matching `servers`' existing
/// group-sorted order), that group's `Server` rows and its own
/// `SpawnNewInGroup` row only if `collapsed` does not contain its key
/// (skipped entirely for the synthetic `"Unknown"` bucket regardless --
/// see `AttachMenuRow::SpawnNewInGroup`'s doc comment), then a trailing
/// `SpawnNew`. A collapsed group hides everything but its header --
/// `SpawnNewInGroup` included, so collapsing a group fully tucks it away
/// rather than leaving one row still poking out. Grouping itself stays
/// `group_servers_by_cwd`'s job; this only decides which of those
/// already-grouped rows are currently *visible* for selection/rendering.
/// Takes the raw `servers` slice rather than a whole `AttachMenu` so a
/// caller building the very first `AttachMenu` (no instance to borrow
/// from yet) can still compute an initial `selected` from it — see
/// `App::detach_and_open_menu`. When `grouped` is `false` (see
/// `App.grouped_view`), headers and per-group spawn rows are skipped
/// entirely and every server gets a plain `Server` row in `servers`'
/// existing (still pin/alpha-sorted, just unlabeled) order -- `collapsed`
/// is ignored in that case, since there are no headers to collapse.
fn visible_attach_menu_rows(
    servers: &[(String, ServerPaneInfo)],
    collapsed: &HashSet<String>,
    grouped: bool,
) -> Vec<AttachMenuRow> {
    const UNKNOWN: &str = "Unknown";
    let mut rows = Vec::with_capacity(servers.len() + 2);
    if !grouped {
        for index in 0..servers.len() {
            rows.push(AttachMenuRow::Server(index));
        }
        rows.push(AttachMenuRow::SpawnNew);
        return rows;
    }
    let mut last_group: Option<&str> = None;
    let mut group_is_collapsed = false;
    let mut group_start = 0;
    for (index, (group, _)) in servers.iter().enumerate() {
        if last_group != Some(group.as_str()) {
            if let Some(prev_group) = last_group
                && prev_group != UNKNOWN
                && !group_is_collapsed
            {
                rows.push(AttachMenuRow::SpawnNewInGroup(group_start));
            }
            rows.push(AttachMenuRow::GroupHeader(index));
            last_group = Some(group.as_str());
            group_is_collapsed = collapsed.contains(group.as_str());
            group_start = index;
        }
        if !group_is_collapsed {
            rows.push(AttachMenuRow::Server(index));
        }
    }
    if let Some(last) = last_group
        && last != UNKNOWN
        && !group_is_collapsed
    {
        rows.push(AttachMenuRow::SpawnNewInGroup(group_start));
    }
    rows.push(AttachMenuRow::SpawnNew);
    rows
}

/// Pick where the attach menu's cursor should start when it opens right
/// after detaching from `previously_bound` -- makes reattaching the same
/// pane a single Enter press instead of having to hunt for it. Finds the
/// row in `visible_attach_menu_rows(servers, collapsed)` for the
/// `Server` entry whose id matches `previously_bound`, if any; if that
/// pane's group happens to be collapsed (so its `Server` row isn't
/// currently visible at all), lands on the group's header instead --
/// one Enter to expand gets there, still closer than starting at row 0.
/// Falls back to `0` (the plan `selected: 0` default) when there's
/// nothing to preselect: `previously_bound` is `None` (the leaf was
/// already unbound) or the pane no longer exists in the fresh
/// `servers` list (e.g. killed by another connection in the gap between
/// detaching and this menu's `ServerList` fetch).
fn initial_selection_for(
    servers: &[(String, ServerPaneInfo)],
    collapsed: &HashSet<String>,
    grouped: bool,
    previously_bound: Option<ServerPaneId>,
) -> usize {
    let Some(previously_bound) = previously_bound else {
        return 0;
    };
    let Some(server_index) = servers.iter().position(|(_, s)| s.id == previously_bound) else {
        return 0;
    };
    let rows = visible_attach_menu_rows(servers, collapsed, grouped);
    if let Some(row_index) = rows
        .iter()
        .position(|row| *row == AttachMenuRow::Server(server_index))
    {
        return row_index;
    }
    // The group is collapsed -- its Server row is hidden, but its header
    // (which carries the same first-member index the group started at,
    // not necessarily `server_index` itself) is still selectable.
    let group_key = &servers[server_index].0;
    rows.iter()
        .position(|row| matches!(row, AttachMenuRow::GroupHeader(i) if &servers[*i].0 == group_key))
        .unwrap_or(0)
}

/// Filter `servers` down to what the attach menu should actually show.
/// Server-panes are never scoped to a workspace -- every pane spawned
/// anywhere is reachable from every workspace's attach menu, same as
/// `dimax server ls` and friends already saw everything regardless of
/// which client-pane spawned a pane.
///
/// `agents_only`, when set, drops every pane whose foreground process
/// isn't a recognized AI-coding CLI tool (see `protocol::SessionKind`/
/// `App::agents_only_view`'s doc comment on why this is the *default*
/// view, not an opt-in one) -- same caveat as `session_kind` itself: a
/// `Dead` pane has no foreground process to classify at all, so a
/// finished agent session drops out of this view along with everything
/// else non-agent; toggling this off still finds it.
fn filter_servers_for_menu(servers: Vec<ServerPaneInfo>, agents_only: bool) -> Vec<ServerPaneInfo> {
    if !agents_only {
        return servers;
    }
    servers
        .into_iter()
        .filter(|s| {
            s.foreground
                .as_ref()
                .is_some_and(|f| f.session_kind.is_some())
        })
        .collect()
}

/// Synthetic group key for a server-pane with no resolvable foreground
/// `cwd` (a `Dead` pane, or a live one whose lookup failed) -- always
/// sorted last, regardless of pin state: pinning is a directory-level
/// preference, and there's no real directory here to have pinned.
const UNKNOWN_GROUP: &str = "Unknown";

/// Sort `servers` into cwd-bucket order for the attach menu's grouped
/// display, then hand back `(group_key, server)` pairs (see this
/// function's return type doc below for why a flat `Vec` rather than
/// nested groups). Order: every directory in `pinned` first, in
/// `pinned`'s own order (the earliest-pinned dir sorts above a
/// later-pinned one -- `pinned` is itself already in that order, see
/// `state::State::toggle_pinned_dir`'s doc comment), then every
/// remaining real directory ascending alphabetically, then
/// [`UNKNOWN_GROUP`] last regardless of anything else. Within a bucket,
/// panes keep their relative input order (stable sort, no secondary
/// key) -- whatever order `ServerList`/`ServerPaneList` returned.
/// Returns `(group_key, server)` pairs rather than a nested
/// `Vec<Vec<_>>` so callers can walk it once and detect group
/// boundaries by comparing consecutive keys, which is exactly what
/// `render::draw_attach_menu` needs to decide where to emit a header.
fn group_servers_by_cwd(
    mut servers: Vec<ServerPaneInfo>,
    pinned: &[String],
) -> Vec<(String, ServerPaneInfo)> {
    let key_of = |s: &ServerPaneInfo| -> String {
        s.foreground
            .as_ref()
            .and_then(|f| f.cwd.clone())
            .unwrap_or_else(|| UNKNOWN_GROUP.to_string())
    };
    // `(pin_rank, key)`: `pin_rank` is the dir's index into `pinned`
    // (so earlier-pinned sorts first), `pinned.len()` for any real,
    // unpinned directory (sorts after every pinned one, then subject
    // to the plain alphabetical `key` tie-break), or `usize::MAX` for
    // `UNKNOWN_GROUP` (always last, full stop).
    let rank_of = |key: &str| -> usize {
        if key == UNKNOWN_GROUP {
            usize::MAX
        } else {
            pinned.iter().position(|p| p == key).unwrap_or(pinned.len())
        }
    };
    servers.sort_by(|a, b| {
        let (key_a, key_b) = (key_of(a), key_of(b));
        rank_of(&key_a)
            .cmp(&rank_of(&key_b))
            .then_with(|| key_a.cmp(&key_b))
    });
    servers.into_iter().map(|s| (key_of(&s), s)).collect()
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
    /// `p` on a group header row: pin/unpin that directory (see
    /// `App::toggle_directory_pin`). A no-op on any other row -- there's
    /// no directory to pin from a server/spawn/spawn-new row.
    TogglePin,
    /// `g` on any row: flip `App.grouped_view` between grouped-by-cwd and
    /// a flat list (see `App::toggle_grouping`). Purely a local
    /// re-render -- no network round-trip, unlike `ToggleAgentsOnly`.
    ToggleGrouping,
    /// `f` on any row: flip `App.agents_only_view` between "only
    /// recognized AI-coding CLI sessions" (default) and "every pane" --
    /// re-fetches and re-filters immediately so the row list updates
    /// in-place.
    ToggleAgentsOnly,
    /// `d` on any row: detach every tab from the leaf that opened this
    /// menu (see `App::detach_all_and_close_menu`), then close the
    /// menu -- distinct from `Cancel` (`Esc`), which restores whatever
    /// was bound before the menu opened rather than clearing it.
    DetachAll,
    /// `q` on any row: exit `dimax attach` entirely, same as
    /// `Action::Quit` outside the menu -- the outer `cmd-shift-q`/
    /// `Ctrl-Space q` chord can't reach the app while the menu is open
    /// (every byte routes through here instead), so browsing the
    /// server-pane selector needs its own quit binding rather than
    /// forcing `Esc` first. Only reachable from browse-mode dispatch
    /// (`parse_attach_menu_input`), never while renaming or editing a
    /// spawn-in-group field -- `q` types literally there instead, same
    /// as every other browse-mode letter.
    Quit,
    Ignore,
}

/// Map one raw input chunk to an `AttachMenuAction`. Direct byte matching:
/// arrow-key escape sequences and vi-style `j`/`k` move the selection,
/// Enter (`\r` or `\n`) confirms, a bare `Esc` cancels (leaving the pane
/// unbound — it was already detached by `detach_and_open_menu` before the
/// menu opened), `x` arms/confirms delete, `r` opens rename, `p` toggles a
/// pin. Anything else is ignored rather than passed through — the menu is
/// modal and has no server-pane to forward keystrokes to yet. Note: this
/// table is only consulted for *browsing* mode — `App::handle_attach_menu_input`
/// routes to entirely separate byte-handling while `pending_delete`/
/// `rename` are active, so `x`/Enter's *confirm* behavior when a delete is
/// already armed is handled there, not by this function returning some
/// third "confirm delete" variant.
fn parse_attach_menu_input(bytes: &[u8]) -> AttachMenuAction {
    match bytes {
        b"\r" | b"\n" => AttachMenuAction::Confirm,
        b"\x1b" => AttachMenuAction::Cancel,
        b"\x1b[A" | b"k" => AttachMenuAction::Up,
        b"\x1b[B" | b"j" => AttachMenuAction::Down,
        b"x" => AttachMenuAction::Delete,
        b"r" => AttachMenuAction::StartRename,
        b"p" => AttachMenuAction::TogglePin,
        b"f" => AttachMenuAction::ToggleAgentsOnly,
        b"g" => AttachMenuAction::ToggleGrouping,
        b"d" => AttachMenuAction::DetachAll,
        b"q" => AttachMenuAction::Quit,
        _ => AttachMenuAction::Ignore,
    }
}

/// Apply one raw input chunk to a live inline-field edit buffer. Shared
/// by the rename field (`r` on a server row) and the per-group spawn
/// field ("+ spawn new in <dir>") -- both are a plain `(text, cursor,
/// error)` triple edited identically, so this operates on those three
/// pieces directly rather than on `RenameState`/`SpawnInGroupState`
/// themselves, letting both wrap it with a one-line adapter instead of
/// duplicating the whole match. Byte-level matching in the same minimal
/// style as `parse_attach_menu_input`/`keys::parse` rather than pulling
/// in a text-input crate — these fields only ever need insert/delete/
/// cursor movement, not a general-purpose editor. Every branch that
/// mutates `text`/`cursor` also clears `*error`, so a fresh edit
/// dismisses the last rejection message rather than leaving stale text
/// stuck under the field. Enter/Esc are handled by each caller, not
/// here, since they trigger network requests or close the field
/// entirely -- concerns outside this pure buffer-editing function.
fn apply_text_edit(
    text: &mut String,
    cursor: &mut usize,
    error: &mut Option<String>,
    bytes: &[u8],
) {
    match bytes {
        b"\x7f" | b"\x08" => {
            if *cursor > 0 {
                let mut chars: Vec<char> = text.chars().collect();
                let char_idx = text[..*cursor].chars().count() - 1;
                chars.remove(char_idx);
                *text = chars.into_iter().collect();
                *cursor = text[..].chars().take(char_idx).map(|c| c.len_utf8()).sum();
            } else {
                return;
            }
        }
        b"\x1b[3~" => {
            let char_idx = text[..*cursor].chars().count();
            let mut chars: Vec<char> = text.chars().collect();
            if char_idx < chars.len() {
                chars.remove(char_idx);
                *text = chars.into_iter().collect();
            } else {
                return;
            }
        }
        b"\x1b[D" => {
            if *cursor > 0 {
                let char_idx = text[..*cursor].chars().count() - 1;
                *cursor = text.chars().take(char_idx).map(|c| c.len_utf8()).sum();
            } else {
                return;
            }
        }
        b"\x1b[C" => {
            if *cursor < text.len() {
                let char_idx = text[..*cursor].chars().count() + 1;
                *cursor = text.chars().take(char_idx).map(|c| c.len_utf8()).sum();
            } else {
                return;
            }
        }
        b"\x1b[H" | b"\x1b[1~" => {
            if *cursor == 0 {
                return;
            }
            *cursor = 0;
        }
        b"\x1b[F" | b"\x1b[4~" => {
            if *cursor == text.len() {
                return;
            }
            *cursor = text.len();
        }
        _ if bytes.first() == Some(&0x1b) => {
            // Any other escape sequence this field doesn't recognize --
            // defense in depth, same rationale as `mouse::parse`'s
            // "Ignored" case: don't let a stray sequence get inserted as
            // literal garbage text.
            return;
        }
        _ => match std::str::from_utf8(bytes) {
            Ok(inserted) => {
                text.insert_str(*cursor, inserted);
                *cursor += inserted.len();
            }
            Err(_) => return,
        },
    }
    *error = None;
}

fn apply_rename_edit(state: &mut RenameState, bytes: &[u8]) {
    apply_text_edit(&mut state.text, &mut state.cursor, &mut state.error, bytes);
}

fn apply_spawn_in_group_edit(state: &mut SpawnInGroupState, bytes: &[u8]) {
    apply_text_edit(&mut state.text, &mut state.cursor, &mut state.error, bytes);
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

/// Enable exactly the xterm mouse modes dimax's mouse interactions need:
/// normal tracking (`?1000h`, click/release) + button-event tracking (`?1002h`,
/// motion reported only *while a button is held* — i.e. drags) + SGR
/// extended-coordinate encoding (`?1006h`, what `mouse::parse` expects).
///
/// Deliberately NOT `ratatui::crossterm::event::EnableMouseCapture`: that
/// command additionally enables any-event tracking (`?1003h`), which
/// reports *every* mouse movement — including with no button held at
/// all, e.g. just passing the cursor over the window while typing
/// elsewhere in it. dimax has no use for movement-only events, and
/// `?1002h` alone never asks the terminal to report motion without a
/// button down, avoiding both raw mouse bytes leaking into a pane and
/// needless event-loop traffic.
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

fn copy_to_clipboard(text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(selection::osc52_sequence(text).as_bytes())?;
    stdout.flush()
}

/// Drive the first-run wizard's own tiny event loop -- draw, read one
/// stdin chunk, apply it -- until it reports `WizardOutcome::Done`, then
/// persist the chosen keybinding mode (and install Kitty bindings if
/// that mode needs them, same as `dimax keys install --mode kitty`
/// would) and the Claude skill if requested. Isolated from `run`'s main
/// loop entirely -- no daemon connection exists yet at this point, and
/// the wizard has no interaction with `App`/attach-menu/mouse state.
async fn run_first_run_wizard(
    terminal: &mut ratatui::DefaultTerminal,
    stdin: &mut tokio::io::Stdin,
    buf: &mut [u8; 512],
) -> anyhow::Result<()> {
    let mut wizard = first_run::Wizard::new();
    loop {
        terminal.draw(|frame| wizard.draw(frame))?;
        let n = stdin.read(buf).await?;
        if n == 0 {
            break;
        }
        if let first_run::WizardOutcome::Done {
            mode,
            install_skill,
        } = wizard.handle_input(&buf[..n])
        {
            keys::save_mode(mode)?;
            if mode.kitty_enabled() {
                let _ = kitty_setup::install();
            }
            if install_skill {
                let _ = crate::skills_setup::install();
            }
            break;
        }
    }
    Ok(())
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

    let mut stdin = tokio::io::stdin();
    let mut buf = [0u8; 512];

    // One-time first-run wizard (keybinding mode + Claude skill install),
    // gated by `keys::consume_first_run` so it only ever shows once per
    // `keybindings.json` -- see `first_run` module doc. Runs before
    // `bootstrap` since it's purely local (no daemon requests) and its
    // choice of `BindingMode` needs to land on disk before `load_mode`
    // is read just below.
    if keys::consume_first_run() {
        run_first_run_wizard(&mut terminal, &mut stdin, &mut buf).await?;
    }

    let mut app = App::bootstrap(&mut write_half, &mut reader, "1").await?;

    let mut key_parser = keys::PortableParser::from_config();
    let binding_mode = keys::load_mode();
    // Keeps the attach menu's preview panel showing a selected pane's
    // *live* output (see `refresh_attach_menu_preview`'s doc comment)
    // rather than only updating it right after a selection change --
    // ticking unconditionally is deliberate: the refresh call itself is
    // a cheap no-op whenever the menu is closed or the selection isn't
    // on a server-pane row, so there's no need to start/stop this timer
    // as the menu opens and closes.
    let mut preview_tick = tokio::time::interval(std::time::Duration::from_millis(300));
    // A `\x1b[<...` mouse sequence whose terminator (`M`/`m`) hadn't
    // arrived yet by the end of the previous read -- carried forward
    // and prepended to the next one so `mouse::parse_all` sees the whole
    // sequence in one piece instead of a cut-off fragment. See mouse.rs
    // module doc "Split sequences": at real scroll speeds, `read()`'s
    // fixed-size buffer routinely fills up mid-sequence, splitting one
    // `ESC [ < ... M` chord across two reads -- without this carry-over,
    // the orphaned tail used to be typed into the focused pane as
    // literal garbage.
    let mut pending_mouse: Vec<u8> = Vec::new();

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
                    render::draw_with_selection(
                        frame,
                        &preview,
                        &app.grids,
                        &app.server_names,
                        app.focused,
                        app.text_selection.as_ref(),
                    );
                }
                None => render::draw_with_selection(
                    frame,
                    &app.workspace,
                    &app.grids,
                    &app.server_names,
                    app.focused,
                    app.text_selection.as_ref(),
                ),
            }
            if let Some(menu) = &app.attach_menu {
                render::draw_attach_menu(
                    frame,
                    menu,
                    &app.collapsed_groups,
                    app.grouped_view,
                    app.attach_menu_preview.as_ref(),
                    &app.pinned_dirs,
                );
            }
        })?;

        // Report any leaf whose on-screen size changed since the last
        // frame -- see `changed_pane_sizes`'s doc comment for why this
        // piggybacks on the render loop's own cadence instead of a
        // SIGWINCH handler. This is the fix for the "bash renders in the
        // top half" bug: without this, `Request::ResizeClientPane` is
        // never sent at all, and every PTY stays pinned at the daemon's
        // 24x80 default regardless of the pane's real size.
        if let Some(tree) = &app.workspace.tree {
            let changed = changed_pane_sizes(&app.pane_sizes, tree, app.frame_area);
            for (pane, size) in changed {
                app.pane_sizes.insert(pane, size);
                let req = Request::ResizeClientPane { pane, size };
                let _ = app.request(&mut write_half, &mut reader, req).await?;
            }
        }

        tokio::select! {
            result = stdin.read(&mut buf) => {
                let n = result?;
                if n == 0 {
                    // stdin closed (e.g. piped input exhausted) -- nothing
                    // further to read, so there is no way to drive the UI;
                    // exit cleanly rather than spin.
                    break;
                }
                // Prepend any mouse-sequence fragment left over from the
                // previous read (see `pending_mouse`'s doc comment) --
                // this is what lets a sequence split across two reads
                // get reassembled instead of misrouted.
                let bytes: std::borrow::Cow<[u8]> = if pending_mouse.is_empty() {
                    std::borrow::Cow::Borrowed(&buf[..n])
                } else {
                    let mut combined = std::mem::take(&mut pending_mouse);
                    combined.extend_from_slice(&buf[..n]);
                    std::borrow::Cow::Owned(combined)
                };
                if is_quit(&bytes) {
                    break;
                }
                // `parse_all`, not `parse`: a fast scroll (trackpad, or
                // just a quick wheel flick) routinely bundles more than
                // one SGR sequence into a single `read()`, and/or a
                // sequence can be split across two reads by the fixed
                // read-buffer size -- see mouse.rs module doc "Bundled
                // sequences" / "Split sequences" for the two distinct
                // bugs this fixes (both used to leak raw escape bytes
                // into the focused pane as literal garbage keystrokes).
                let (mouse_events, incomplete, leftover) = mouse::parse_all(&bytes);
                for event in mouse_events {
                    if let Some(text) =
                        app.handle_mouse(event, &mut write_half, &mut reader).await?
                    {
                        copy_to_clipboard(&text)?;
                    }
                }
                if !incomplete.is_empty() {
                    // Cap how large a carried-over fragment can grow --
                    // a genuine SGR sequence is at most a few dozen bytes
                    // (`ESC [ < ` + up to three small integers + `;` x2 +
                    // terminator), so anything this large is not a real
                    // sequence patiently waiting for its terminator; it's
                    // either a corrupted stream or something actively
                    // hostile. Drop it rather than let `pending_mouse`
                    // grow without bound across many reads.
                    const MAX_PENDING_MOUSE_FRAGMENT: usize = 64;
                    if incomplete.len() <= MAX_PENDING_MOUSE_FRAGMENT {
                        pending_mouse = incomplete.to_vec();
                    }
                }
                if !leftover.is_empty() {
                    if app.attach_menu.is_some() {
                        let should_quit = app
                            .handle_attach_menu_input(leftover, &mut write_half, &mut reader)
                            .await?;
                        if should_quit {
                            break;
                        }
                        // Refresh right away rather than waiting for
                        // `preview_tick` -- otherwise opening the menu
                        // or moving the selection shows a blank/stale
                        // panel for up to one tick interval.
                        app.refresh_attach_menu_preview(&mut write_half, &mut reader).await?;
                    } else {
                        match key_parser.parse(leftover, binding_mode) {
                            keys::ParsedInput::Action(action) => {
                                let should_quit = app
                                    .handle_action(action, &[], &mut write_half, &mut reader)
                                    .await?;
                                if should_quit {
                                    break;
                                }
                            }
                            keys::ParsedInput::PassThrough(raw) => {
                                app.handle_action(
                                    Action::PassThrough,
                                    &raw,
                                    &mut write_half,
                                    &mut reader,
                                )
                                .await?;
                            }
                            keys::ParsedInput::Pending => {}
                        }
                        // Catches the chord that just opened the menu
                        // (`Action::DetachAndAttach`) -- `attach_menu`
                        // was still `None` when this branch started, so
                        // the `is_some()` check above routed here
                        // instead of through the other immediate-refresh
                        // call.
                        app.refresh_attach_menu_preview(&mut write_half, &mut reader).await?;
                    }
                }
            }
            _ = preview_tick.tick() => {
                app.refresh_attach_menu_preview(&mut write_half, &mut reader).await?;
                app.refresh_server_names(&mut write_half, &mut reader).await?;
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
                    // Connection lost -- most likely a hot reload on the
                    // daemon side (see `daemon::mod`'s "Hot reload"
                    // module doc): the accepted-connection fd this
                    // socket was using doesn't survive the daemon's
                    // `execve` even though every server-pane does, so
                    // every attached client sees exactly this. Reconnect
                    // and re-bootstrap rather than dying -- a real,
                    // unrecoverable daemon loss surfaces as an `Err`
                    // from `reconnect` itself (its own last-resort
                    // fallback already auto-spawns a fresh daemon if
                    // nothing answers at all).
                    Err(_) => {
                        let (new_write_half, new_reader) = reconnect().await?;
                        write_half = new_write_half;
                        reader = new_reader;
                        app = App::bootstrap(&mut write_half, &mut reader, "1").await?;
                        // `ratatui::Terminal::draw` only ever emits escapes
                        // for cells that changed since its *own* cached
                        // buffer, not since what's really on screen -- fine
                        // normally, but the reconnect gap just replaced
                        // `app` wholesale (fresh grids, re-derived pane
                        // sizes) while the terminal's actual visible
                        // content is still whatever the pre-reload frame
                        // last drew. Any cell whose new content happens to
                        // equal ratatui's stale cached cell (common with
                        // idle shell prompts) never gets rewritten, leaving
                        // real garbage on screen until something else
                        // forces a full repaint -- a terminal resize does
                        // this today, which is the "fiddling" a user
                        // shouldn't have to do.
                        //
                        // Deliberately `resize(size)`, NOT `terminal.
                        // clear()`: `Terminal::clear()` snapshots and
                        // restores the cursor position, which for a
                        // `CrosstermBackend` means calling `crossterm::
                        // cursor::position()` -- that writes `ESC[6n` and
                        // then blocks on *crossterm's own* internal event
                        // reader for the terminal's reply. This module
                        // deliberately reads raw stdin itself instead of
                        // going through crossterm's event system (see the
                        // module doc "Raw stdin reads, not crossterm's
                        // KeyEvents"), so that reply is never delivered to
                        // crossterm's reader -- `cursor::position()` always
                        // times out after 2s and surfaces as "Error: The
                        // cursor position could not be read within a
                        // normal duration", killing this loop via the `?`
                        // below on every single reconnect. `resize` forces
                        // the same full-redraw invalidation via `clear_
                        // viewport` internally, but for a `Viewport::
                        // Fullscreen` terminal (what `try_init` sets up)
                        // it never touches cursor position at all.
                        let size = terminal.size()?;
                        terminal.resize(size.into())?;
                        continue;
                    }
                }
            }
        }
    }

    Ok(())
}

/// After the connection drops unexpectedly, reconnect and split a fresh
/// pair of halves -- see `run`'s `reader.next()` error arm for the one
/// caller and why this exists (module doc reference: `daemon::mod`'s
/// "Hot reload"). Retries a bare connect a few times first (100ms
/// apart, 2s total) rather than going straight to `Client::connect`'s
/// auto-spawn-if-unreachable fallback: a hot reload's gap between the
/// old process's `execve` and the new one finishing its listener rewrap
/// is milliseconds, and spawning a *second* daemon because this raced a
/// still-in-progress reload would be actively wrong. Only falls back to
/// `Client::connect` (which auto-spawns) once those retries are
/// exhausted -- covering the genuine "the daemon actually died" case,
/// same recovery a fresh `dimax attach` would get today.
async fn reconnect() -> anyhow::Result<(OwnedWriteHalf, FrameReader)> {
    let path = crate::protocol::socket_path();
    for _ in 0..20 {
        if let Ok(stream) = tokio::net::UnixStream::connect(&path).await {
            let (read_half, write_half) = stream.into_split();
            return Ok((write_half, FrameReader::spawn(read_half)));
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let client = Client::connect().await?;
    let (read_half, write_half) = client.into_split();
    Ok((write_half, FrameReader::spawn(read_half)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ClientPane, SessionKind};
    use uuid::Uuid;

    fn leaf(id: ClientPaneId) -> SplitTree {
        SplitTree::Leaf(ClientPane {
            id,
            name: None,
            tabs: vec![],
            active_tab: 0,
            short_id: "aa".to_string(),
        })
    }

    fn split(dir: SplitDir, a: SplitTree, b: SplitTree) -> SplitTree {
        SplitTree::Split {
            id: Uuid::new_v4(),
            dir,
            ratio: 0.5,
            a: Box::new(a),
            b: Box::new(b),
        }
    }

    fn rect(x: u16, y: u16, width: u16, height: u16) -> ratatui::layout::Rect {
        ratatui::layout::Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn nearest_leaf_single_leaf_has_no_neighbors() {
        let id = Uuid::new_v4();
        let tree = leaf(id);
        let area = rect(0, 0, 80, 24);
        for dir in [
            Direction::Left,
            Direction::Right,
            Direction::Up,
            Direction::Down,
        ] {
            assert_eq!(nearest_leaf_in_direction(&tree, area, Some(id), dir), None);
        }
    }

    #[test]
    fn nearest_leaf_no_current_focus_lands_on_first_leaf() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let tree = split(SplitDir::Vertical, leaf(a), leaf(b));
        let area = rect(0, 0, 80, 24);
        assert_eq!(
            nearest_leaf_in_direction(&tree, area, None, Direction::Right),
            Some(a)
        );
    }

    #[test]
    fn nearest_leaf_unknown_current_id_lands_on_first_leaf() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let tree = split(SplitDir::Vertical, leaf(a), leaf(b));
        let area = rect(0, 0, 80, 24);
        let stale = Uuid::new_v4();
        assert_eq!(
            nearest_leaf_in_direction(&tree, area, Some(stale), Direction::Left),
            Some(a)
        );
    }

    #[test]
    fn nearest_leaf_left_right_split_moves_horizontally_only() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        // Vertical split = side-by-side panes (see render.rs module doc
        // "SplitDir -> ratatui::Direction mapping").
        let tree = split(SplitDir::Vertical, leaf(a), leaf(b));
        let area = rect(0, 0, 80, 24);
        assert_eq!(
            nearest_leaf_in_direction(&tree, area, Some(a), Direction::Right),
            Some(b)
        );
        assert_eq!(
            nearest_leaf_in_direction(&tree, area, Some(b), Direction::Left),
            Some(a)
        );
        assert_eq!(
            nearest_leaf_in_direction(&tree, area, Some(a), Direction::Up),
            None
        );
        assert_eq!(
            nearest_leaf_in_direction(&tree, area, Some(a), Direction::Down),
            None
        );
        assert_eq!(
            nearest_leaf_in_direction(&tree, area, Some(b), Direction::Right),
            None,
            "no wraparound"
        );
    }

    #[test]
    fn nearest_leaf_top_bottom_split_moves_vertically_only() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        // Horizontal split = stacked panes.
        let tree = split(SplitDir::Horizontal, leaf(a), leaf(b));
        let area = rect(0, 0, 80, 24);
        assert_eq!(
            nearest_leaf_in_direction(&tree, area, Some(a), Direction::Down),
            Some(b)
        );
        assert_eq!(
            nearest_leaf_in_direction(&tree, area, Some(b), Direction::Up),
            Some(a)
        );
        assert_eq!(
            nearest_leaf_in_direction(&tree, area, Some(a), Direction::Left),
            None
        );
        assert_eq!(
            nearest_leaf_in_direction(&tree, area, Some(a), Direction::Right),
            None
        );
    }

    #[test]
    fn nearest_leaf_picks_the_directly_adjacent_pane_over_a_diagonal_one() {
        // Three panes: `top` spans the full width across the top half;
        // `bottom_left`/`bottom_right` split the bottom half side by side.
        // From `bottom_left`, Up must land on `top` (directly above),
        // not skip past it -- and there is only one candidate "above" here
        // by construction, so this also confirms the direction filter
        // itself (not just tie-breaking) is doing the right thing.
        let top = Uuid::new_v4();
        let bottom_left = Uuid::new_v4();
        let bottom_right = Uuid::new_v4();
        let tree = split(
            SplitDir::Horizontal,
            leaf(top),
            split(SplitDir::Vertical, leaf(bottom_left), leaf(bottom_right)),
        );
        let area = rect(0, 0, 80, 24);
        assert_eq!(
            nearest_leaf_in_direction(&tree, area, Some(bottom_left), Direction::Up),
            Some(top)
        );
        assert_eq!(
            nearest_leaf_in_direction(&tree, area, Some(bottom_right), Direction::Up),
            Some(top)
        );
        assert_eq!(
            nearest_leaf_in_direction(&tree, area, Some(bottom_left), Direction::Right),
            Some(bottom_right)
        );
    }

    fn divider_hit(
        split: crate::protocol::SplitId,
        zone: ratatui::layout::Rect,
    ) -> render::DividerHit {
        render::DividerHit {
            split,
            dir: SplitDir::Vertical,
            grab_zone: zone,
            parent_area: ratatui::layout::Rect {
                x: 0,
                y: 0,
                width: 40,
                height: 10,
            },
        }
    }

    #[test]
    fn hit_test_dividers_finds_the_containing_zone() {
        let split_id = Uuid::new_v4();
        let hits = vec![divider_hit(
            split_id,
            ratatui::layout::Rect {
                x: 20,
                y: 0,
                width: 1,
                height: 10,
            },
        )];
        assert_eq!(hit_test_dividers(&hits, 20, 5), Some(split_id));
    }

    #[test]
    fn hit_test_dividers_misses_outside_the_zone() {
        let split_id = Uuid::new_v4();
        let hits = vec![divider_hit(
            split_id,
            ratatui::layout::Rect {
                x: 20,
                y: 0,
                width: 1,
                height: 10,
            },
        )];
        assert_eq!(hit_test_dividers(&hits, 19, 5), None);
        assert_eq!(hit_test_dividers(&hits, 21, 5), None);
        assert_eq!(hit_test_dividers(&hits, 20, 10), None); // one past the bottom edge
    }

    #[test]
    fn hit_test_dividers_picks_the_first_match_among_several() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let hits = vec![
            divider_hit(
                a,
                ratatui::layout::Rect {
                    x: 10,
                    y: 0,
                    width: 1,
                    height: 10,
                },
            ),
            divider_hit(
                b,
                ratatui::layout::Rect {
                    x: 30,
                    y: 0,
                    width: 1,
                    height: 10,
                },
            ),
        ];
        assert_eq!(hit_test_dividers(&hits, 10, 0), Some(a));
        assert_eq!(hit_test_dividers(&hits, 30, 0), Some(b));
        assert_eq!(hit_test_dividers(&hits, 20, 0), None);
    }

    #[test]
    fn changed_pane_sizes_reports_a_never_seen_pane() {
        let id = Uuid::new_v4();
        let tree = leaf(id);
        let area = ratatui::layout::Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };
        let current: HashMap<ClientPaneId, crate::protocol::Size> = HashMap::new();
        let changed = changed_pane_sizes(&current, &tree, area);
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].0, id);
        // -1 row for the title-bar border every leaf reserves (see
        // `render::draw_leaf`) -- the usable grid area is one row shorter
        // than the leaf's full on-screen rect.
        assert_eq!(changed[0].1, crate::protocol::Size { rows: 23, cols: 80 });
    }

    #[test]
    fn changed_pane_sizes_reports_nothing_when_unchanged() {
        let id = Uuid::new_v4();
        let tree = leaf(id);
        let area = ratatui::layout::Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };
        let mut current = HashMap::new();
        current.insert(id, crate::protocol::Size { rows: 23, cols: 80 });
        let changed = changed_pane_sizes(&current, &tree, area);
        assert_eq!(changed.len(), 0);
    }

    #[test]
    fn changed_pane_sizes_reports_a_genuinely_resized_pane() {
        let id = Uuid::new_v4();
        let tree = leaf(id);
        let area = ratatui::layout::Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 30,
        };
        let mut current = HashMap::new();
        current.insert(id, crate::protocol::Size { rows: 23, cols: 80 });
        let changed = changed_pane_sizes(&current, &tree, area);
        assert_eq!(changed.len(), 1);
        assert_eq!(
            changed[0],
            (
                id,
                crate::protocol::Size {
                    rows: 29,
                    cols: 100
                }
            )
        );
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
        assert_eq!(parse_attach_menu_input(b"z"), AttachMenuAction::Ignore);
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
    fn parse_attach_menu_input_p_is_toggle_pin() {
        assert_eq!(parse_attach_menu_input(b"p"), AttachMenuAction::TogglePin);
    }

    #[test]
    fn parse_attach_menu_input_g_is_toggle_grouping() {
        assert_eq!(
            parse_attach_menu_input(b"g"),
            AttachMenuAction::ToggleGrouping
        );
    }

    #[test]
    fn parse_attach_menu_input_f_is_toggle_agents_only() {
        assert_eq!(
            parse_attach_menu_input(b"f"),
            AttachMenuAction::ToggleAgentsOnly
        );
    }

    #[test]
    fn parse_attach_menu_input_d_is_detach_all() {
        assert_eq!(parse_attach_menu_input(b"d"), AttachMenuAction::DetachAll);
    }

    #[test]
    fn parse_attach_menu_input_q_is_quit() {
        assert_eq!(parse_attach_menu_input(b"q"), AttachMenuAction::Quit);
    }

    #[test]
    fn quit_byte_is_ctrl_q_not_ctrl_c() {
        assert!(is_quit(&[0x11]));
        assert!(
            !is_quit(&[0x03]),
            "Ctrl-C must remain a pass-through byte, not the quit key"
        );
    }

    /// `Action::Quit` (reachable via `cmd-shift-q`/`Ctrl-Space q`, see
    /// `keys::BINDINGS`) must signal the caller's read loop to break --
    /// unlike every other action, `handle_action` has no way to do that
    /// on its own, so it reports back via its return value instead.
    #[tokio::test]
    async fn handle_action_quit_signals_the_caller_to_break() {
        let (mut app, mut write_half, mut reader) = app_against_real_daemon().await;
        let should_quit = app
            .handle_action(Action::Quit, &[], &mut write_half, &mut reader)
            .await
            .unwrap();
        assert!(should_quit);
    }

    /// Every other action must leave the read loop running.
    #[tokio::test]
    async fn handle_action_non_quit_action_does_not_signal_a_break() {
        let (mut app, mut write_half, mut reader) = app_against_real_daemon().await;
        let should_quit = app
            .handle_action(Action::PassThrough, b"x", &mut write_half, &mut reader)
            .await
            .unwrap();
        assert!(!should_quit);
    }

    /// `q` while browsing the server-pane selector must quit too -- the
    /// outer `cmd-shift-q`/`Ctrl-Space q` chord can't reach `handle_action`
    /// while the menu is open (every byte routes through
    /// `dispatch_attach_menu_action` instead), so it needs its own signal
    /// back to the caller's read loop, mirroring `handle_action`'s.
    #[tokio::test]
    async fn dispatch_attach_menu_action_quit_signals_the_caller_to_break() {
        let (mut app, mut write_half, mut reader) = app_against_real_daemon().await;
        let should_quit = app
            .dispatch_attach_menu_action(AttachMenuAction::Quit, &mut write_half, &mut reader)
            .await
            .unwrap();
        assert!(should_quit);
    }

    #[tokio::test]
    async fn dispatch_attach_menu_action_non_quit_action_does_not_signal_a_break() {
        let (mut app, mut write_half, mut reader) = app_against_real_daemon().await;
        let should_quit = app
            .dispatch_attach_menu_action(AttachMenuAction::Ignore, &mut write_half, &mut reader)
            .await
            .unwrap();
        assert!(!should_quit);
    }

    #[test]
    fn first_leaf_of_empty_workspace_is_none() {
        let workspace = WorkspaceInfo {
            id: Uuid::new_v4(),
            number: 1,
            name: None,
            tree: None,
        };
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
                session_kind: None,
            }),
            short_id: "aa".to_string(),
            attached_to: Vec::new(),
        }
    }

    #[test]
    fn group_servers_by_cwd_groups_matching_dirs_together() {
        let servers = vec![
            server_with_cwd("a", Some("/home/dev/api")),
            server_with_cwd("b", Some("/home/dev/web")),
            server_with_cwd("c", Some("/home/dev/api")),
        ];
        let grouped = group_servers_by_cwd(servers, &[]);
        let names: Vec<&str> = grouped
            .iter()
            .map(|(_, s)| s.name.as_deref().unwrap())
            .collect();
        // Ascending by cwd: api's two panes (in original relative order),
        // then web's one pane.
        assert_eq!(names, vec!["a", "c", "b"]);
        let keys: Vec<&str> = grouped.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            vec!["/home/dev/api", "/home/dev/api", "/home/dev/web"]
        );
    }

    #[test]
    fn group_servers_by_cwd_sorts_unknown_group_last() {
        let servers = vec![
            server_with_cwd("no-cwd", None),
            server_with_cwd("has-cwd", Some("/zzz/last-alphabetically")),
        ];
        let grouped = group_servers_by_cwd(servers, &[]);
        let names: Vec<&str> = grouped
            .iter()
            .map(|(_, s)| s.name.as_deref().unwrap())
            .collect();
        // "/zzz/..." sorts after "Unknown" alphabetically, but Unknown is
        // forced last regardless.
        assert_eq!(names, vec!["has-cwd", "no-cwd"]);
        assert_eq!(grouped[1].0, "Unknown");
    }

    #[test]
    fn group_servers_by_cwd_empty_list_is_empty() {
        // `ServerPaneInfo` doesn't derive `PartialEq`, so compare lengths
        // rather than the whole `Vec` via `assert_eq!`.
        assert_eq!(group_servers_by_cwd(vec![], &[]).len(), 0);
    }

    fn server_with_kind(name: &str, kind: Option<SessionKind>) -> ServerPaneInfo {
        let mut s = server_with_cwd(name, Some("/tmp"));
        if let Some(fg) = s.foreground.as_mut() {
            fg.session_kind = kind;
        }
        s
    }

    #[test]
    fn filter_servers_for_menu_agents_only_keeps_only_recognized_sessions() {
        let servers = vec![
            server_with_kind("shell", None),
            server_with_kind("editor", None),
            server_with_kind("agent-a", Some(SessionKind::Claude)),
            server_with_kind("agent-b", Some(SessionKind::Codex)),
        ];
        let filtered = filter_servers_for_menu(servers, true);
        let names: Vec<&str> = filtered
            .iter()
            .map(|s| s.name.as_deref().unwrap())
            .collect();
        assert_eq!(
            names,
            vec!["agent-a", "agent-b"],
            "agents_only must drop every pane whose session_kind is None"
        );
    }

    #[test]
    fn filter_servers_for_menu_agents_only_drops_dead_panes_with_no_foreground() {
        let mut dead = server_with_kind("dead-agent", None);
        dead.status = crate::protocol::ServerPaneStatus::Dead;
        dead.foreground = None;
        let servers = vec![
            dead,
            server_with_kind("live-agent", Some(SessionKind::Claude)),
        ];
        let filtered = filter_servers_for_menu(servers, true);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name.as_deref(), Some("live-agent"));
    }

    #[test]
    fn filter_servers_for_menu_not_agents_only_keeps_everything() {
        let servers = vec![
            server_with_kind("shell", None),
            server_with_kind("agent-a", Some(SessionKind::Claude)),
        ];
        let filtered = filter_servers_for_menu(servers, false);
        assert_eq!(
            filtered.len(),
            2,
            "agents_only: false must pass everything through"
        );
    }

    #[test]
    fn group_servers_by_cwd_sorts_pinned_dirs_before_unpinned_ones() {
        let servers = vec![
            server_with_cwd("a", Some("/home/dev/api")), // alphabetically first, but unpinned
            server_with_cwd("b", Some("/home/dev/web")),
            server_with_cwd("c", Some("/home/dev/zzz")),
        ];
        // "/home/dev/zzz" is pinned despite sorting last alphabetically --
        // it must still come out first.
        let pinned = vec!["/home/dev/zzz".to_string()];
        let grouped = group_servers_by_cwd(servers, &pinned);
        let keys: Vec<&str> = grouped.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            vec!["/home/dev/zzz", "/home/dev/api", "/home/dev/web"]
        );
    }

    #[test]
    fn group_servers_by_cwd_orders_multiple_pinned_dirs_by_pin_order_not_alphabetically() {
        let servers = vec![
            server_with_cwd("a", Some("/aaa")),
            server_with_cwd("b", Some("/bbb")),
            server_with_cwd("c", Some("/ccc")),
        ];
        // Pinned in this exact order -- "/ccc" pinned first, so it must
        // sort first despite being alphabetically last.
        let pinned = vec!["/ccc".to_string(), "/aaa".to_string()];
        let grouped = group_servers_by_cwd(servers, &pinned);
        let keys: Vec<&str> = grouped.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["/ccc", "/aaa", "/bbb"]);
    }

    #[test]
    fn group_servers_by_cwd_unknown_group_stays_last_even_when_pinned_dirs_exist() {
        let servers = vec![
            server_with_cwd("no-cwd", None),
            server_with_cwd("has-cwd", Some("/pinned")),
        ];
        let pinned = vec!["/pinned".to_string()];
        let grouped = group_servers_by_cwd(servers, &pinned);
        let keys: Vec<&str> = grouped.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["/pinned", "Unknown"]);
    }

    #[test]
    fn group_servers_by_cwd_a_pinned_dir_with_no_current_servers_has_no_effect() {
        // Pinning is a plain string preference, independent of whether
        // any server-pane currently has that cwd -- a stale/no-longer-
        // relevant pin must not panic or otherwise affect grouping.
        let servers = vec![server_with_cwd("a", Some("/real"))];
        let pinned = vec!["/nonexistent".to_string()];
        let grouped = group_servers_by_cwd(servers, &pinned);
        let keys: Vec<&str> = grouped.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["/real"]);
    }

    #[test]
    fn visible_attach_menu_rows_lists_every_row_when_nothing_collapsed() {
        let servers = vec![
            ("/a".to_string(), server_with_cwd("x", Some("/a"))),
            ("/a".to_string(), server_with_cwd("y", Some("/a"))),
            ("/b".to_string(), server_with_cwd("z", Some("/b"))),
        ];
        let rows = visible_attach_menu_rows(&servers, &HashSet::new(), true);
        assert_eq!(
            rows,
            vec![
                AttachMenuRow::GroupHeader(0),
                AttachMenuRow::Server(0),
                AttachMenuRow::Server(1),
                AttachMenuRow::SpawnNewInGroup(0),
                AttachMenuRow::GroupHeader(2),
                AttachMenuRow::Server(2),
                AttachMenuRow::SpawnNewInGroup(2),
                AttachMenuRow::SpawnNew,
            ]
        );
    }

    #[test]
    fn visible_attach_menu_rows_grouped_false_emits_no_headers_or_spawn_in_group_rows() {
        // Same three-server, two-group input as the test above, but with
        // `grouped: false` -- every server gets a plain `Server` row in
        // its existing order, no `GroupHeader`/`SpawnNewInGroup` at all,
        // `collapsed` is ignored entirely, and the trailing `SpawnNew`
        // still closes the list.
        let servers = vec![
            ("/a".to_string(), server_with_cwd("x", Some("/a"))),
            ("/a".to_string(), server_with_cwd("y", Some("/a"))),
            ("/b".to_string(), server_with_cwd("z", Some("/b"))),
        ];
        let mut collapsed = HashSet::new();
        collapsed.insert("/a".to_string());
        let rows = visible_attach_menu_rows(&servers, &collapsed, false);
        assert_eq!(
            rows,
            vec![
                AttachMenuRow::Server(0),
                AttachMenuRow::Server(1),
                AttachMenuRow::Server(2),
                AttachMenuRow::SpawnNew,
            ]
        );
    }

    #[test]
    fn visible_attach_menu_rows_omits_all_of_a_collapsed_groups_rows_except_its_header() {
        let servers = vec![
            ("/a".to_string(), server_with_cwd("x", Some("/a"))),
            ("/a".to_string(), server_with_cwd("y", Some("/a"))),
            ("/b".to_string(), server_with_cwd("z", Some("/b"))),
        ];
        let mut collapsed = HashSet::new();
        collapsed.insert("/a".to_string());
        let rows = visible_attach_menu_rows(&servers, &collapsed, true);
        // Both of "/a"'s Server rows AND its own "spawn new here" row
        // disappear -- only its header stays, so the group can still be
        // found and re-expanded. The uncollapsed "/b" group is
        // unaffected, spawn row included.
        assert_eq!(
            rows,
            vec![
                AttachMenuRow::GroupHeader(0),
                AttachMenuRow::GroupHeader(2),
                AttachMenuRow::Server(2),
                AttachMenuRow::SpawnNewInGroup(2),
                AttachMenuRow::SpawnNew,
            ]
        );
    }

    #[test]
    fn visible_attach_menu_rows_omits_spawn_in_group_for_the_unknown_bucket() {
        let servers = vec![server_with_cwd("no-cwd", None)]
            .into_iter()
            .map(|s| ("Unknown".to_string(), s))
            .collect::<Vec<_>>();
        let rows = visible_attach_menu_rows(&servers, &HashSet::new(), true);
        // The synthetic "Unknown" bucket has no real directory to spawn
        // a new pane into, so it gets no SpawnNewInGroup row at all --
        // only the real per-group rows do.
        assert_eq!(
            rows,
            vec![
                AttachMenuRow::GroupHeader(0),
                AttachMenuRow::Server(0),
                AttachMenuRow::SpawnNew
            ]
        );
    }

    #[test]
    fn initial_selection_for_lands_on_the_previously_bound_servers_row() {
        let a = server_with_cwd("a", Some("/x"));
        let b = server_with_cwd("b", Some("/x"));
        let b_id = b.id;
        let servers = vec![("/x".to_string(), a), ("/x".to_string(), b)];
        // Rows: 0 = header, 1 = a's Server row, 2 = b's Server row, 3 = spawn-in-group.
        assert_eq!(
            initial_selection_for(&servers, &HashSet::new(), true, Some(b_id)),
            2
        );
    }

    #[test]
    fn initial_selection_for_falls_back_to_zero_when_nothing_was_previously_bound() {
        let servers = vec![("/x".to_string(), server_with_cwd("a", Some("/x")))];
        assert_eq!(
            initial_selection_for(&servers, &HashSet::new(), true, None),
            0
        );
    }

    #[test]
    fn initial_selection_for_falls_back_to_zero_when_the_pane_no_longer_exists() {
        let servers = vec![("/x".to_string(), server_with_cwd("a", Some("/x")))];
        assert_eq!(
            initial_selection_for(&servers, &HashSet::new(), true, Some(Uuid::new_v4())),
            0
        );
    }

    #[test]
    fn initial_selection_for_lands_on_the_group_header_when_that_group_is_collapsed() {
        let a = server_with_cwd("a", Some("/x"));
        let a_id = a.id;
        let servers = vec![("/x".to_string(), a)];
        let mut collapsed = HashSet::new();
        collapsed.insert("/x".to_string());
        // Rows when collapsed: 0 = header only (Server(0) is hidden).
        assert_eq!(
            initial_selection_for(&servers, &collapsed, true, Some(a_id)),
            0
        );
    }

    #[test]
    fn toggle_group_collapse_collapses_then_expands() {
        let servers = vec![("/a".to_string(), server_with_cwd("x", Some("/a")))];
        let mut app = App {
            workspace: WorkspaceInfo {
                id: Uuid::new_v4(),
                number: 1,
                name: None,
                tree: None,
            },
            grids: HashMap::new(),
            pane_sizes: HashMap::new(),
            focused: None,
            attach_menu: Some(AttachMenu {
                servers,
                selected: 0,
                pending_delete: None,
                rename: None,
                previously_bound: None,
                spawn_in_group: None,
                adding_tab: false,
            }),
            frame_area: ratatui::layout::Rect::default(),
            dragging_split: None,
            text_selection: None,
            collapsed_groups: HashSet::new(),
            grouped_view: true,
            agents_only_view: true,
            server_names: HashMap::new(),
            attach_menu_preview: None,
            pinned_dirs: Vec::new(),
        };
        app.toggle_group_collapse(0);
        assert!(app.collapsed_groups.contains("/a"));
        app.toggle_group_collapse(0);
        assert!(!app.collapsed_groups.contains("/a"));
    }

    #[test]
    fn toggle_group_collapse_clamps_selection_into_the_shrunk_row_list() {
        let servers = vec![("/a".to_string(), server_with_cwd("x", Some("/a")))];
        // Rows before collapse: 0 = header, 1 = server, 2 = "spawn new
        // here", 3 = global spawn-new. Collapsing "/a" hides everything
        // but its header (server row AND its own spawn-here row --
        // see `visible_attach_menu_rows`), shrinking the list to 2 rows
        // (0..=1). Selecting the last row beforehand must not leave
        // `selected` pointing past that shrunk list.
        let mut app = App {
            workspace: WorkspaceInfo {
                id: Uuid::new_v4(),
                number: 1,
                name: None,
                tree: None,
            },
            grids: HashMap::new(),
            pane_sizes: HashMap::new(),
            focused: None,
            attach_menu: Some(AttachMenu {
                servers,
                selected: 3,
                pending_delete: None,
                rename: None,
                previously_bound: None,
                spawn_in_group: None,
                adding_tab: false,
            }),
            frame_area: ratatui::layout::Rect::default(),
            dragging_split: None,
            text_selection: None,
            collapsed_groups: HashSet::new(),
            grouped_view: true,
            agents_only_view: true,
            server_names: HashMap::new(),
            attach_menu_preview: None,
            pinned_dirs: Vec::new(),
        };
        app.toggle_group_collapse(0);
        assert_eq!(
            app.attach_menu.unwrap().selected,
            1,
            "should clamp onto the new last row (global spawn-new)"
        );
    }

    #[test]
    fn toggle_grouping_flips_view_and_clamps_selection() {
        let servers = vec![("/a".to_string(), server_with_cwd("x", Some("/a")))];
        // Grouped rows: 0 = header, 1 = server, 2 = "spawn new here",
        // 3 = global spawn-new -- len 4. Flat rows (grouped_view: false)
        // drop the header and per-group spawn row entirely: 0 = server,
        // 1 = global spawn-new -- len 2. Selecting the last grouped row
        // beforehand must not leave `selected` pointing past the
        // shrunk flat list.
        let mut app = App {
            workspace: WorkspaceInfo {
                id: Uuid::new_v4(),
                number: 1,
                name: None,
                tree: None,
            },
            grids: HashMap::new(),
            pane_sizes: HashMap::new(),
            focused: None,
            attach_menu: Some(AttachMenu {
                servers,
                selected: 3,
                pending_delete: None,
                rename: None,
                previously_bound: None,
                spawn_in_group: None,
                adding_tab: false,
            }),
            frame_area: ratatui::layout::Rect::default(),
            dragging_split: None,
            text_selection: None,
            collapsed_groups: HashSet::new(),
            grouped_view: true,
            agents_only_view: true,
            server_names: HashMap::new(),
            attach_menu_preview: None,
            pinned_dirs: Vec::new(),
        };
        app.toggle_grouping();
        assert!(!app.grouped_view);
        assert_eq!(
            app.attach_menu.as_ref().unwrap().selected,
            1,
            "should clamp onto the new last row (global spawn-new) of the flat list"
        );
        app.toggle_grouping();
        assert!(app.grouped_view);
    }

    #[test]
    fn move_attach_menu_selection_wraps_across_headers_and_spawn_new() {
        let servers = vec![("/a".to_string(), server_with_cwd("x", Some("/a")))];
        // Rows: 0 = header, 1 = server, 2 = "spawn new here", 3 = global
        // spawn-new -- len 4, so moving down from the last row wraps
        // back to 0, and up from 0 wraps to the last row.
        let mut app = App {
            workspace: WorkspaceInfo {
                id: Uuid::new_v4(),
                number: 1,
                name: None,
                tree: None,
            },
            grids: HashMap::new(),
            pane_sizes: HashMap::new(),
            focused: None,
            attach_menu: Some(AttachMenu {
                servers,
                selected: 3,
                pending_delete: None,
                rename: None,
                previously_bound: None,
                spawn_in_group: None,
                adding_tab: false,
            }),
            frame_area: ratatui::layout::Rect::default(),
            dragging_split: None,
            text_selection: None,
            collapsed_groups: HashSet::new(),
            grouped_view: true,
            agents_only_view: true,
            server_names: HashMap::new(),
            attach_menu_preview: None,
            pinned_dirs: Vec::new(),
        };
        app.move_attach_menu_selection(true);
        assert_eq!(app.attach_menu.as_ref().unwrap().selected, 0);
        app.move_attach_menu_selection(false);
        assert_eq!(app.attach_menu.as_ref().unwrap().selected, 3);
    }

    #[test]
    fn arm_delete_sets_pending_delete_on_a_real_row() {
        let servers = vec![("Unknown".to_string(), server_with_cwd("a", None))];
        // Rows: 0 = "Unknown" header, 1 = the server row, 2 = spawn-new.
        let mut app = App {
            workspace: WorkspaceInfo {
                id: Uuid::new_v4(),
                number: 1,
                name: None,
                tree: None,
            },
            grids: HashMap::new(),
            pane_sizes: HashMap::new(),
            focused: None,
            attach_menu: Some(AttachMenu {
                servers,
                selected: 1,
                pending_delete: None,
                rename: None,
                previously_bound: None,
                spawn_in_group: None,
                adding_tab: false,
            }),
            frame_area: ratatui::layout::Rect::default(),
            dragging_split: None,
            text_selection: None,
            collapsed_groups: HashSet::new(),
            grouped_view: true,
            agents_only_view: true,
            server_names: HashMap::new(),
            attach_menu_preview: None,
            pinned_dirs: Vec::new(),
        };
        app.arm_delete();
        assert_eq!(app.attach_menu.unwrap().pending_delete, Some(0));
    }

    #[test]
    fn arm_delete_on_group_header_row_is_a_no_op() {
        let servers = vec![("Unknown".to_string(), server_with_cwd("a", None))];
        let mut app = App {
            workspace: WorkspaceInfo {
                id: Uuid::new_v4(),
                number: 1,
                name: None,
                tree: None,
            },
            grids: HashMap::new(),
            pane_sizes: HashMap::new(),
            focused: None,
            attach_menu: Some(AttachMenu {
                servers,
                selected: 0,
                pending_delete: None,
                rename: None,
                previously_bound: None,
                spawn_in_group: None,
                adding_tab: false,
            }),
            frame_area: ratatui::layout::Rect::default(),
            dragging_split: None,
            text_selection: None,
            collapsed_groups: HashSet::new(),
            grouped_view: true,
            agents_only_view: true,
            server_names: HashMap::new(),
            attach_menu_preview: None,
            pinned_dirs: Vec::new(),
        };
        app.arm_delete();
        assert_eq!(app.attach_menu.unwrap().pending_delete, None);
    }

    #[test]
    fn arm_delete_on_spawn_new_row_is_a_no_op() {
        let servers = vec![("Unknown".to_string(), server_with_cwd("a", None))];
        // Rows: 0 = "Unknown" header, 1 = the server row, 2 = spawn-new.
        let spawn_index = 2;
        let mut app = App {
            workspace: WorkspaceInfo {
                id: Uuid::new_v4(),
                number: 1,
                name: None,
                tree: None,
            },
            grids: HashMap::new(),
            pane_sizes: HashMap::new(),
            focused: None,
            attach_menu: Some(AttachMenu {
                servers,
                selected: spawn_index,
                pending_delete: None,
                rename: None,
                previously_bound: None,
                spawn_in_group: None,
                adding_tab: false,
            }),
            frame_area: ratatui::layout::Rect::default(),
            dragging_split: None,
            text_selection: None,
            collapsed_groups: HashSet::new(),
            grouped_view: true,
            agents_only_view: true,
            server_names: HashMap::new(),
            attach_menu_preview: None,
            pinned_dirs: Vec::new(),
        };
        app.arm_delete();
        assert_eq!(app.attach_menu.unwrap().pending_delete, None);
    }

    fn rename_state(text: &str, cursor: usize) -> RenameState {
        RenameState {
            index: 0,
            text: text.to_string(),
            cursor,
            error: None,
        }
    }

    #[test]
    fn rename_edit_inserts_printable_bytes_at_cursor() {
        let mut state = rename_state("ab", 1);
        apply_rename_edit(&mut state, b"X");
        assert_eq!(state.text, "aXb");
        assert_eq!(state.cursor, 2);
    }

    #[test]
    fn rename_edit_backspace_removes_char_before_cursor() {
        let mut state = rename_state("abc", 2);
        apply_rename_edit(&mut state, b"\x7f");
        assert_eq!(state.text, "ac");
        assert_eq!(state.cursor, 1);
    }

    #[test]
    fn rename_edit_backspace_at_start_is_a_no_op() {
        let mut state = rename_state("abc", 0);
        apply_rename_edit(&mut state, b"\x7f");
        assert_eq!(state.text, "abc");
        assert_eq!(state.cursor, 0);
    }

    #[test]
    fn rename_edit_delete_removes_char_at_cursor() {
        let mut state = rename_state("abc", 1);
        apply_rename_edit(&mut state, b"\x1b[3~");
        assert_eq!(state.text, "ac");
        assert_eq!(state.cursor, 1);
    }

    #[test]
    fn rename_edit_left_and_right_move_cursor() {
        let mut state = rename_state("abc", 1);
        apply_rename_edit(&mut state, b"\x1b[C");
        assert_eq!(state.cursor, 2);
        apply_rename_edit(&mut state, b"\x1b[D");
        assert_eq!(state.cursor, 1);
    }

    #[test]
    fn rename_edit_left_at_start_and_right_at_end_are_no_ops() {
        let mut state = rename_state("abc", 0);
        apply_rename_edit(&mut state, b"\x1b[D");
        assert_eq!(state.cursor, 0);
        let mut state = rename_state("abc", 3);
        apply_rename_edit(&mut state, b"\x1b[C");
        assert_eq!(state.cursor, 3);
    }

    #[test]
    fn rename_edit_home_and_end_jump_cursor() {
        let mut state = rename_state("abc", 1);
        apply_rename_edit(&mut state, b"\x1b[H");
        assert_eq!(state.cursor, 0);
        apply_rename_edit(&mut state, b"\x1b[F");
        assert_eq!(state.cursor, 3);
    }

    #[test]
    fn rename_edit_clears_stale_error_on_any_edit() {
        let mut state = rename_state("abc", 1);
        state.error = Some("name taken".to_string());
        apply_rename_edit(&mut state, b"X");
        assert_eq!(state.error, None);
    }

    #[test]
    fn rename_edit_unrecognized_escape_is_ignored() {
        let mut state = rename_state("abc", 1);
        apply_rename_edit(&mut state, b"\x1b[99~");
        assert_eq!(state.text, "abc");
        assert_eq!(state.cursor, 1);
    }

    #[test]
    fn start_rename_prefills_current_name_with_cursor_at_end() {
        let servers = vec![("Unknown".to_string(), server_with_cwd("my-name", None))];
        // Rows: 0 = "Unknown" header, 1 = the server row.
        let mut app = App {
            workspace: WorkspaceInfo {
                id: Uuid::new_v4(),
                number: 1,
                name: None,
                tree: None,
            },
            grids: HashMap::new(),
            pane_sizes: HashMap::new(),
            focused: None,
            attach_menu: Some(AttachMenu {
                servers,
                selected: 1,
                pending_delete: None,
                rename: None,
                previously_bound: None,
                spawn_in_group: None,
                adding_tab: false,
            }),
            frame_area: ratatui::layout::Rect::default(),
            dragging_split: None,
            text_selection: None,
            collapsed_groups: HashSet::new(),
            grouped_view: true,
            agents_only_view: true,
            server_names: HashMap::new(),
            attach_menu_preview: None,
            pinned_dirs: Vec::new(),
        };
        app.start_rename();
        let rename = app.attach_menu.unwrap().rename.unwrap();
        assert_eq!(rename.text, "my-name");
        assert_eq!(rename.cursor, "my-name".len());
        assert_eq!(rename.index, 0);
    }

    #[test]
    fn start_rename_on_group_header_row_is_a_no_op() {
        let servers = vec![("Unknown".to_string(), server_with_cwd("a", None))];
        let mut app = App {
            workspace: WorkspaceInfo {
                id: Uuid::new_v4(),
                number: 1,
                name: None,
                tree: None,
            },
            grids: HashMap::new(),
            pane_sizes: HashMap::new(),
            focused: None,
            attach_menu: Some(AttachMenu {
                servers,
                selected: 0,
                pending_delete: None,
                rename: None,
                previously_bound: None,
                spawn_in_group: None,
                adding_tab: false,
            }),
            frame_area: ratatui::layout::Rect::default(),
            dragging_split: None,
            text_selection: None,
            collapsed_groups: HashSet::new(),
            grouped_view: true,
            agents_only_view: true,
            server_names: HashMap::new(),
            attach_menu_preview: None,
            pinned_dirs: Vec::new(),
        };
        app.start_rename();
        assert!(app.attach_menu.unwrap().rename.is_none());
    }

    #[test]
    fn start_rename_on_spawn_new_row_is_a_no_op() {
        let servers = vec![("Unknown".to_string(), server_with_cwd("a", None))];
        // Rows: 0 = "Unknown" header, 1 = the server row, 2 = spawn-new.
        let spawn_index = 2;
        let mut app = App {
            workspace: WorkspaceInfo {
                id: Uuid::new_v4(),
                number: 1,
                name: None,
                tree: None,
            },
            grids: HashMap::new(),
            pane_sizes: HashMap::new(),
            focused: None,
            attach_menu: Some(AttachMenu {
                servers,
                selected: spawn_index,
                pending_delete: None,
                rename: None,
                previously_bound: None,
                spawn_in_group: None,
                adding_tab: false,
            }),
            frame_area: ratatui::layout::Rect::default(),
            dragging_split: None,
            text_selection: None,
            collapsed_groups: HashSet::new(),
            grouped_view: true,
            agents_only_view: true,
            server_names: HashMap::new(),
            attach_menu_preview: None,
            pinned_dirs: Vec::new(),
        };
        app.start_rename();
        assert!(app.attach_menu.unwrap().rename.is_none());
    }

    /// Regression test for the "dimax hangs often" bug: racing a frame
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

        assert_eq!(
            received, 8,
            "expected all 8 frames to survive being raced against a faster branch"
        );
        writer.await.unwrap();
        let _ = std::fs::remove_file(&path);
    }

    /// Connect a fresh `App` (via `bootstrap`) to a real, freshly started
    /// daemon at workspace `"1"`, for the attach-menu-against-a-live-
    /// daemon tests below. Returns the `App` plus the split connection
    /// halves every `App` method needs.
    ///
    /// `bootstrap` now spawns a free default shell into a truly empty
    /// workspace (the shell fallback -- see `bootstrap_empty_workspace`),
    /// which every test in this module that predates that feature
    /// assumes does NOT exist. Two distinct assumptions break otherwise,
    /// so this undoes *both* halves of the fallback: the client-pane
    /// (callers send their own `ClientSpawn { workspace: "1", split_of:
    /// None, .. }`, which errors against a workspace that already has a
    /// pane) *and* the server-pane it spawned, which `ClientClose` leaves
    /// alive in the pool where it would show up as an extra
    /// `ServerList` row -- with the daemon's own cwd as its group key,
    /// which is enough to change attach-menu group ordering.
    ///
    /// `tree`/`focused` are then reset explicitly: the `LayoutDelta`s for
    /// the spawn and close are pushed asynchronously, so which of them
    /// this connection has read by now is scheduler-dependent, and only
    /// an explicit reset makes the returned state deterministic.
    async fn app_against_real_daemon() -> (App, OwnedWriteHalf, FrameReader) {
        // Short filename -- `dimax-test-<full-uuid>.sock` under a long
        // macOS temp dir can exceed `SUN_LEN`; this stays well under it.
        static NEXT_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let socket_path =
            std::env::temp_dir().join(format!("dmx-am-{}-{id}.sock", std::process::id()));
        crate::daemon::run(socket_path.clone())
            .await
            .expect("daemon should bind and start");
        let stream = tokio::net::UnixStream::connect(&socket_path)
            .await
            .expect("connect to test daemon");
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = FrameReader::spawn(read_half);
        let mut app = App::bootstrap(&mut write_half, &mut reader, "1")
            .await
            .expect("bootstrap workspace 1");
        if let Some(pane) = app.focused {
            let req = Request::ClientClose {
                workspace: app.workspace.id.to_string(),
                pane,
            };
            let _ = app
                .request(&mut write_half, &mut reader, req)
                .await
                .expect("close the shell-fallback leaf");
        }
        let Response::ServerPaneList(servers) = app
            .request(&mut write_half, &mut reader, Request::ServerList)
            .await
            .expect("list server-panes")
        else {
            panic!("expected ServerPaneList");
        };
        for server in servers {
            let req = Request::ServerKill {
                target: server.id.to_string(),
            };
            let _ = app
                .request(&mut write_half, &mut reader, req)
                .await
                .expect("kill the shell-fallback server-pane");
        }
        app.workspace.tree = None;
        app.focused = None;
        // Tests spawn plain `cat`/`sh` server-panes -- none of them are
        // recognized as agent sessions, so the default agents-only
        // filter (`agents_only_view: true` from `App::bootstrap`) would
        // hide every test pane from `filter_servers_for_menu`. Flip it
        // off for the test harness so existing coverage still exercises
        // the full attach-menu logic; a dedicated test below covers the
        // filter's *own* behavior against a mixed set of agent and
        // non-agent panes.
        app.agents_only_view = false;
        (app, write_half, reader)
    }

    #[tokio::test]
    async fn jump_session_binds_the_numbered_existing_session_without_creating_one() {
        let (mut app, mut write_half, mut reader) = app_against_real_daemon().await;
        let mut spawned = Vec::new();
        for name in ["alpha", "beta"] {
            let Response::ServerPane(server) = app
                .request(
                    &mut write_half,
                    &mut reader,
                    Request::ServerSpawn {
                        name: Some(name.to_string()),
                        cmd: None,
                        cwd: None,
                    },
                )
                .await
                .unwrap()
            else {
                panic!("expected ServerPane");
            };
            spawned.push(server.id);
        }
        let Response::ClientPaneCreated { pane, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ClientSpawn {
                    workspace: "1".to_string(),
                    split_of: None,
                    dir: None,
                    bind: Some(spawned[0].to_string()),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ClientPaneCreated");
        };
        app.focused = Some(pane);

        app.jump_session(2, &mut write_half, &mut reader)
            .await
            .unwrap();

        let Response::ClientPaneList { panes, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ClientList {
                    workspace: Some("1".to_string()),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ClientPaneList");
        };
        assert_eq!(panes[0].active_bound(), Some(spawned[1]));

        app.jump_session(9, &mut write_half, &mut reader)
            .await
            .unwrap();
        let Response::ServerPaneList(servers) = app
            .request(&mut write_half, &mut reader, Request::ServerList)
            .await
            .unwrap()
        else {
            panic!("expected ServerPaneList");
        };
        assert_eq!(
            servers.len(),
            2,
            "a missing number must not create a session"
        );
    }

    /// Deliberately does NOT use `app_against_real_daemon` -- that helper
    /// exists to *undo* the shell fallback (see its doc comment), which
    /// is the very thing under test here.
    #[tokio::test]
    async fn bootstrap_on_a_fresh_daemon_with_an_empty_workspace_spawns_a_default_shell() {
        // Short filename -- see `app_against_real_daemon` for why.
        let socket_path =
            std::env::temp_dir().join(format!("dmx-boot1-{}.sock", std::process::id()));
        crate::daemon::run(socket_path.clone())
            .await
            .expect("daemon should bind and start");
        let stream = tokio::net::UnixStream::connect(&socket_path)
            .await
            .expect("connect");
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = FrameReader::spawn(read_half);
        let mut app = App::bootstrap(&mut write_half, &mut reader, "1")
            .await
            .expect("bootstrap workspace 1");

        assert!(
            app.attach_menu.is_none(),
            "the very first attach should not open the picker"
        );
        assert!(
            app.focused.is_some(),
            "a default shell should already be spawned and focused"
        );

        // Assert the *daemon's* view rather than `app.workspace.tree`:
        // the spawn's `LayoutDelta` is pushed asynchronously, so whether
        // this connection has read it yet is scheduler-dependent, while
        // a fresh `ClientList` is authoritative either way.
        let Response::ClientPaneList { panes, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ClientList {
                    workspace: Some("1".to_string()),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ClientPaneList");
        };
        assert_eq!(
            panes.len(),
            1,
            "exactly one default-shell leaf should exist"
        );
        assert_eq!(
            Some(panes[0].id),
            app.focused,
            "that leaf should be the focused one"
        );
        assert!(
            panes[0].active_bound().is_some(),
            "the default shell should be bound to a server-pane"
        );
    }

    #[tokio::test]
    async fn bootstrap_on_a_second_empty_workspace_attach_opens_the_picker_instead() {
        // Short filename -- mirrors `app_against_real_daemon`'s own naming
        // scheme (see its doc comment for why this stays short).
        let socket_path =
            std::env::temp_dir().join(format!("dmx-boot2-{}.sock", std::process::id()));
        crate::daemon::run(socket_path.clone())
            .await
            .expect("daemon should bind and start");

        // First attach: consumes the fallback, spawns a shell in workspace "1".
        let stream = tokio::net::UnixStream::connect(&socket_path)
            .await
            .expect("connect");
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = FrameReader::spawn(read_half);
        let _first = App::bootstrap(&mut write_half, &mut reader, "1")
            .await
            .expect("bootstrap workspace 1");

        // Second attach, to a *different* (still-empty) workspace: the
        // fallback has already been consumed, so this should get the
        // picker instead, not another free shell.
        let stream2 = tokio::net::UnixStream::connect(&socket_path)
            .await
            .expect("connect");
        let (read_half2, mut write_half2) = stream2.into_split();
        let mut reader2 = FrameReader::spawn(read_half2);
        let second = App::bootstrap(&mut write_half2, &mut reader2, "2")
            .await
            .expect("bootstrap workspace 2");

        assert!(
            second.attach_menu.is_some(),
            "the second empty-workspace attach should open the picker"
        );
        assert_eq!(
            second.focused, None,
            "no leaf exists yet, so nothing should be focused"
        );
        assert!(
            second.workspace.tree.is_none(),
            "the picker path must not itself create a leaf"
        );
    }

    #[tokio::test]
    async fn confirm_attach_menu_with_no_focused_pane_spawns_the_first_leaf() {
        // `app_against_real_daemon` already consumed and undid the shell
        // fallback, leaving exactly the empty, unfocused workspace this
        // test needs -- no second daemon required.
        let (mut app, mut write_half, mut reader) = app_against_real_daemon().await;
        assert_eq!(
            app.focused, None,
            "helper should hand back a leaf-less workspace"
        );
        let Response::ServerPane(existing) = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ServerSpawn {
                    name: None,
                    cmd: Some("cat".to_string()),
                    cwd: None,
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ServerPane");
        };
        let existing_id = existing.id;
        app.attach_menu = Some(AttachMenu {
            servers: vec![("Unknown".to_string(), existing)],
            // Rows: 0 = "Unknown" header, 1 = the server row. Row 0 would
            // be intercepted as a group-header toggle, never reaching the
            // no-`focused` branch this test is about.
            selected: 1,
            pending_delete: None,
            rename: None,
            previously_bound: None,
            spawn_in_group: None,
            adding_tab: false,
        });
        assert_eq!(
            app.selected_attach_menu_row(),
            Some(AttachMenuRow::Server(0)),
            "row 1 should be the existing server-pane's own row"
        );

        app.confirm_attach_menu(&mut write_half, &mut reader)
            .await
            .unwrap();

        assert!(
            app.focused.is_some(),
            "confirming on a leaf-less workspace should focus the newly created leaf"
        );
        let Response::ClientPaneList { panes, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ClientList {
                    workspace: Some("1".to_string()),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ClientPaneList");
        };
        let pane = panes
            .iter()
            .find(|p| Some(p.id) == app.focused)
            .expect("focused pane should exist");
        assert_eq!(pane.active_bound(), Some(existing_id));
    }

    /// Open the attach menu directly (bypassing `detach_and_open_menu`,
    /// which requires a focused client-pane to detach) with one
    /// already-spawned server-pane grouped under `cwd`, selection
    /// starting on that group's `SpawnNewInGroup` row -- row 2: 0 =
    /// header, 1 = `existing`'s own `Server` row, 2 = spawn-in-group,
    /// since the group has exactly one member and starts uncollapsed.
    fn open_menu_with_one_group(app: &mut App, cwd: &str, existing: ServerPaneInfo) {
        app.attach_menu = Some(AttachMenu {
            servers: vec![(cwd.to_string(), existing)],
            selected: 2,
            pending_delete: None,
            rename: None,
            previously_bound: None,
            spawn_in_group: None,
            adding_tab: false,
        });
    }

    /// `cmd-w` (`close_tab`) must kill the active tab's bound server-pane,
    /// not just close the tab -- unlike `ClientCloseTab` alone (which
    /// deliberately leaves the server-pane running for scripted/CLI
    /// callers), the TUI's own close chord is meant to fully clean up
    /// what it was looking at.
    #[tokio::test]
    async fn close_tab_kills_the_bound_server_pane() {
        let (mut app, mut write_half, mut reader) = app_against_real_daemon().await;

        let Response::ServerPane(server) = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ServerSpawn {
                    name: None,
                    cmd: Some("cat".to_string()),
                    cwd: None,
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ServerPane");
        };
        let Response::ClientPaneCreated { pane, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ClientSpawn {
                    workspace: "1".to_string(),
                    split_of: None,
                    dir: None,
                    bind: Some(server.id.to_string()),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ClientPaneCreated");
        };
        app.focused = Some(pane);
        if let Response::Snapshot { workspace, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::Subscribe {
                    workspace: "1".to_string(),
                },
            )
            .await
            .unwrap()
        {
            app.workspace = workspace;
        }

        app.close_tab(&mut write_half, &mut reader).await.unwrap();

        let Response::ServerPaneList(servers) = app
            .request(&mut write_half, &mut reader, Request::ServerList)
            .await
            .unwrap()
        else {
            panic!("expected ServerPaneList");
        };
        assert!(
            !servers.iter().any(|s| s.id == server.id),
            "closing the tab should have killed its bound server-pane"
        );
    }

    /// `cmd-w` on an already-unbound pane (no tabs at all) must be a
    /// complete no-op: nothing to drop, nothing to kill, and -- unlike
    /// the daemon's own `client_close_tab` semantics for a scripted
    /// `dimax client close-tab` caller -- the empty pane must stay in
    /// the layout rather than being removed from the grid.
    #[tokio::test]
    async fn close_tab_on_an_already_unbound_pane_is_a_no_op() {
        let (mut app, mut write_half, mut reader) = app_against_real_daemon().await;

        let Response::ClientPaneCreated { pane, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ClientSpawn {
                    workspace: "1".to_string(),
                    split_of: None,
                    dir: None,
                    bind: None,
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ClientPaneCreated");
        };
        app.focused = Some(pane);
        if let Response::Snapshot { workspace, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::Subscribe {
                    workspace: "1".to_string(),
                },
            )
            .await
            .unwrap()
        {
            app.workspace = workspace;
        }

        app.close_tab(&mut write_half, &mut reader).await.unwrap();

        let Response::ClientPaneList { panes, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ClientList {
                    workspace: Some("1".to_string()),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ClientPaneList");
        };
        assert!(
            panes.iter().any(|p| p.id == pane),
            "an already-unbound pane must survive cmd-w, not be removed from the layout"
        );
    }

    /// `cmd-w` on a leaf with more than one tab must drop only the active
    /// tab (killing that tab's bound server-pane) and leave the leaf, its
    /// other tab, and that other tab's server-pane untouched -- this is
    /// the multi-tab case `close_tab`'s `ClientCloseTab` call is meant to
    /// handle, as opposed to the whole-leaf-closes path already covered by
    /// `close_tab_kills_the_bound_server_pane` (single tab).
    #[tokio::test]
    async fn close_tab_on_a_multi_tab_pane_drops_only_the_active_tab() {
        let (mut app, mut write_half, mut reader) = app_against_real_daemon().await;

        let Response::ServerPane(sp1) = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ServerSpawn {
                    name: None,
                    cmd: Some("cat".to_string()),
                    cwd: None,
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ServerPane");
        };
        let Response::ServerPane(sp2) = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ServerSpawn {
                    name: None,
                    cmd: Some("cat".to_string()),
                    cwd: None,
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ServerPane");
        };
        let Response::ClientPaneCreated { pane, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ClientSpawn {
                    workspace: "1".to_string(),
                    split_of: None,
                    dir: None,
                    bind: Some(sp1.id.to_string()),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ClientPaneCreated");
        };
        // Appending activates the new tab (`client_add_tab_appends_and_
        // activates`), so `sp2` is the active tab `close_tab` will drop.
        let _ = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ClientAddTab {
                    workspace: "1".to_string(),
                    pane,
                    target: sp2.id.to_string(),
                },
            )
            .await
            .unwrap();
        app.focused = Some(pane);
        if let Response::Snapshot { workspace, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::Subscribe {
                    workspace: "1".to_string(),
                },
            )
            .await
            .unwrap()
        {
            app.workspace = workspace;
        }

        app.close_tab(&mut write_half, &mut reader).await.unwrap();

        let Response::ClientPaneList { panes, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ClientList {
                    workspace: Some("1".to_string()),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ClientPaneList");
        };
        assert!(
            panes.iter().any(|p| p.id == pane),
            "the leaf must survive closing one of its two tabs"
        );

        let Response::ServerPaneList(servers) = app
            .request(&mut write_half, &mut reader, Request::ServerList)
            .await
            .unwrap()
        else {
            panic!("expected ServerPaneList");
        };
        assert!(
            !servers.iter().any(|s| s.id == sp2.id),
            "the dropped tab's bound server-pane should have been killed"
        );
        assert!(
            servers.iter().any(|s| s.id == sp1.id),
            "the remaining tab's server-pane must survive"
        );
    }

    /// `cmd-shift-z` end to end: the picker it opens must already have
    /// the just-detached pane's own row selected, so reattaching to the
    /// same pane is a single Enter press rather than having to hunt for
    /// it -- this exercises the real `ServerSpawn`/`ClientSpawn`/
    /// `ServerList` round trip feeding `initial_selection_for`, not just
    /// the pure function in isolation.
    #[tokio::test]
    async fn detach_and_open_menu_preselects_the_just_detached_pane() {
        let (mut app, mut write_half, mut reader) = app_against_real_daemon().await;

        let Response::ServerPane(existing) = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ServerSpawn {
                    name: None,
                    cmd: Some("cat".to_string()),
                    cwd: None,
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ServerPane");
        };
        let existing_id = existing.id;
        let Response::ClientPaneCreated { pane, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ClientSpawn {
                    workspace: "1".to_string(),
                    split_of: None,
                    dir: None,
                    bind: Some(existing_id.to_string()),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ClientPaneCreated");
        };
        app.focused = Some(pane);
        if let Response::Snapshot { workspace, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::Subscribe {
                    workspace: "1".to_string(),
                },
            )
            .await
            .unwrap()
        {
            app.workspace = workspace;
        }

        app.detach_and_open_menu(&mut write_half, &mut reader)
            .await
            .unwrap();

        let menu = app.attach_menu.as_ref().unwrap();
        match app.selected_attach_menu_row() {
            Some(AttachMenuRow::Server(server_index)) => {
                assert_eq!(
                    menu.servers[server_index].1.id, existing_id,
                    "the picker should open with the just-detached pane's own row already selected"
                );
            }
            other => panic!("expected the picker to open on a Server row, got {other:?}"),
        }

        // Confirming immediately (no navigation) should reattach to the
        // exact same pane -- the whole point of preselecting it.
        app.confirm_attach_menu(&mut write_half, &mut reader)
            .await
            .unwrap();
        let Response::ClientPaneList { panes, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ClientList {
                    workspace: Some("1".to_string()),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ClientPaneList");
        };
        let leaf = panes
            .iter()
            .find(|p| p.id == pane)
            .expect("pane should still exist");
        assert_eq!(leaf.active_bound(), Some(existing_id));
    }

    /// `Esc` on the picker `cmd-shift-z` opened must restore the leaf's
    /// original binding rather than leaving it unbound -- cancelling the
    /// pick should be a true no-op, not a silent detach.
    #[tokio::test]
    async fn escape_from_the_picker_restores_the_previous_binding() {
        let (mut app, mut write_half, mut reader) = app_against_real_daemon().await;

        let Response::ServerPane(existing) = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ServerSpawn {
                    name: None,
                    cmd: Some("cat".to_string()),
                    cwd: None,
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ServerPane");
        };
        let existing_id = existing.id;
        let Response::ClientPaneCreated { pane, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ClientSpawn {
                    workspace: "1".to_string(),
                    split_of: None,
                    dir: None,
                    bind: Some(existing_id.to_string()),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ClientPaneCreated");
        };
        app.focused = Some(pane);
        if let Response::Snapshot { workspace, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::Subscribe {
                    workspace: "1".to_string(),
                },
            )
            .await
            .unwrap()
        {
            app.workspace = workspace;
        }

        app.detach_and_open_menu(&mut write_half, &mut reader)
            .await
            .unwrap();
        assert!(
            app.attach_menu.is_some(),
            "the picker should be open before Escape"
        );

        app.dispatch_attach_menu_action(AttachMenuAction::Cancel, &mut write_half, &mut reader)
            .await
            .unwrap();

        assert!(app.attach_menu.is_none(), "Escape should close the menu");
        let Response::ClientPaneList { panes, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ClientList {
                    workspace: Some("1".to_string()),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ClientPaneList");
        };
        let leaf = panes
            .iter()
            .find(|p| p.id == pane)
            .expect("pane should still exist");
        assert_eq!(
            leaf.active_bound(),
            Some(existing_id),
            "Escape should have restored the original binding rather than leaving the leaf unbound"
        );
    }

    /// `Esc` when nothing was previously bound (e.g. the leaf was already
    /// unbound before `cmd-shift-z`) must be a plain close -- no
    /// `ClientBind` with a nonsensical target should be sent.
    #[tokio::test]
    async fn escape_from_the_picker_with_nothing_previously_bound_is_a_plain_close() {
        let (mut app, mut write_half, mut reader) = app_against_real_daemon().await;

        let Response::ClientPaneCreated { pane, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ClientSpawn {
                    workspace: "1".to_string(),
                    split_of: None,
                    dir: None,
                    bind: None,
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ClientPaneCreated");
        };
        app.focused = Some(pane);
        if let Response::Snapshot { workspace, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::Subscribe {
                    workspace: "1".to_string(),
                },
            )
            .await
            .unwrap()
        {
            app.workspace = workspace;
        }

        app.detach_and_open_menu(&mut write_half, &mut reader)
            .await
            .unwrap();
        app.dispatch_attach_menu_action(AttachMenuAction::Cancel, &mut write_half, &mut reader)
            .await
            .unwrap();

        assert!(app.attach_menu.is_none());
        let Response::ClientPaneList { panes, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ClientList {
                    workspace: Some("1".to_string()),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ClientPaneList");
        };
        let leaf = panes
            .iter()
            .find(|p| p.id == pane)
            .expect("pane should still exist");
        assert_eq!(leaf.active_bound(), None);
    }

    /// `d` from within the picker detaches every tab from the leaf that
    /// opened it, leaving it unbound, and closes the menu -- exercised
    /// on a multi-tab leaf specifically, since that's the case a single
    /// `ClientUnbind` can't finish in one call.
    #[tokio::test]
    async fn detach_all_clears_every_tab_on_the_leaf() {
        let (mut app, mut write_half, mut reader) = app_against_real_daemon().await;

        let Response::ServerPane(sp1) = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ServerSpawn {
                    name: None,
                    cmd: Some("cat".to_string()),
                    cwd: None,
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ServerPane");
        };
        let Response::ServerPane(sp2) = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ServerSpawn {
                    name: None,
                    cmd: Some("cat".to_string()),
                    cwd: None,
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ServerPane");
        };
        let Response::ClientPaneCreated { pane, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ClientSpawn {
                    workspace: "1".to_string(),
                    split_of: None,
                    dir: None,
                    bind: Some(sp1.id.to_string()),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ClientPaneCreated");
        };
        let _ = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ClientAddTab {
                    workspace: "1".to_string(),
                    pane,
                    target: sp2.id.to_string(),
                },
            )
            .await
            .unwrap();
        app.focused = Some(pane);
        if let Response::Snapshot { workspace, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::Subscribe {
                    workspace: "1".to_string(),
                },
            )
            .await
            .unwrap()
        {
            app.workspace = workspace;
        }

        app.detach_and_open_menu(&mut write_half, &mut reader)
            .await
            .unwrap();
        assert!(app.attach_menu.is_some());

        app.dispatch_attach_menu_action(AttachMenuAction::DetachAll, &mut write_half, &mut reader)
            .await
            .unwrap();

        assert!(
            app.attach_menu.is_none(),
            "detach-all should close the menu"
        );
        let Response::ClientPaneList { panes, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ClientList {
                    workspace: Some("1".to_string()),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ClientPaneList");
        };
        let leaf = panes
            .iter()
            .find(|p| p.id == pane)
            .expect("pane should still exist");
        assert!(
            leaf.tabs.is_empty(),
            "every tab should have been detached, leaf.tabs = {:?}",
            leaf.tabs
        );

        // Both server-panes must still be running -- detaching is not killing.
        let Response::ServerPaneList(servers) = app
            .request(&mut write_half, &mut reader, Request::ServerList)
            .await
            .unwrap()
        else {
            panic!("expected ServerPaneList");
        };
        assert!(servers.iter().any(|s| s.id == sp1.id));
        assert!(servers.iter().any(|s| s.id == sp2.id));
    }

    /// `cmd-shift-z` on a multi-tab leaf must not drop a sibling tab.
    /// Regression test: `detach_and_open_menu` used to always send
    /// `ClientUnbind` before opening the picker, but `ClientUnbind` now
    /// *removes* the active tab from `tabs` rather than blanking a slot
    /// -- on a 2-tab leaf that shifts the surviving tab down to index 0,
    /// so the picker's `ClientBind` (which replaces "the active tab",
    /// now the wrong one) silently overwrote the sibling instead of the
    /// tab the user meant to replace.
    #[tokio::test]
    async fn detach_and_reattach_on_a_multi_tab_leaf_preserves_the_other_tab() {
        let (mut app, mut write_half, mut reader) = app_against_real_daemon().await;

        let Response::ServerPane(sp1) = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ServerSpawn {
                    name: None,
                    cmd: Some("cat".to_string()),
                    cwd: None,
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ServerPane");
        };
        let Response::ServerPane(sp2) = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ServerSpawn {
                    name: None,
                    cmd: Some("cat".to_string()),
                    cwd: None,
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ServerPane");
        };
        let Response::ServerPane(sp3) = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ServerSpawn {
                    name: None,
                    cmd: Some("cat".to_string()),
                    cwd: None,
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ServerPane");
        };

        let Response::ClientPaneCreated { pane, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ClientSpawn {
                    workspace: "1".to_string(),
                    split_of: None,
                    dir: None,
                    bind: Some(sp1.id.to_string()),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ClientPaneCreated");
        };
        app.focused = Some(pane);
        let _ = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ClientAddTab {
                    workspace: "1".to_string(),
                    pane,
                    target: sp2.id.to_string(),
                },
            )
            .await
            .unwrap();
        // Sync local workspace state so `detach_and_open_menu` sees the
        // real (2-tab, active = sp2) leaf rather than the stale
        // single-tab snapshot from bootstrap.
        if let Response::Snapshot { workspace, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::Subscribe {
                    workspace: "1".to_string(),
                },
            )
            .await
            .unwrap()
        {
            app.workspace = workspace;
        }

        app.detach_and_open_menu(&mut write_half, &mut reader)
            .await
            .unwrap();
        assert!(app.attach_menu.is_some(), "the picker should open");

        // Pick sp3 to replace the active tab (sp2). Row 0 is the
        // "Unknown" group's header (a no-op row for `confirm_attach_menu`),
        // row 1 is sp3's own `Server` row.
        let sp3_id = sp3.id;
        app.attach_menu.as_mut().unwrap().servers = vec![("Unknown".to_string(), sp3)];
        app.attach_menu.as_mut().unwrap().selected = 1;
        app.confirm_attach_menu(&mut write_half, &mut reader)
            .await
            .unwrap();

        let Response::ClientPaneList { panes, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ClientList {
                    workspace: Some("1".to_string()),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ClientPaneList");
        };
        let leaf = panes
            .iter()
            .find(|p| p.id == pane)
            .expect("pane should still exist");
        assert_eq!(
            leaf.tabs,
            vec![sp1.id, sp3_id],
            "sp1 (the untouched sibling tab) must survive; sp2 (the active tab) should have been replaced by sp3"
        );
    }

    /// `cmd-t` end to end: `open_add_tab_menu` sets `adding_tab: true`,
    /// and `confirm_attach_menu`'s eventual bind must branch on that to
    /// send `ClientAddTab` (append) rather than `ClientBind` (replace) --
    /// this is the one path that exercises `adding_tab: true` at all
    /// (every other test in this module opens the menu via
    /// `detach_and_open_menu`/`open_menu_with_one_group`, both of which
    /// use `false`).
    #[tokio::test]
    async fn open_add_tab_menu_then_confirm_appends_rather_than_replaces() {
        let (mut app, mut write_half, mut reader) = app_against_real_daemon().await;

        let Response::ServerPane(sp1) = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ServerSpawn {
                    name: None,
                    cmd: Some("cat".to_string()),
                    cwd: None,
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ServerPane");
        };
        let Response::ServerPane(sp2) = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ServerSpawn {
                    name: None,
                    cmd: Some("cat".to_string()),
                    cwd: None,
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ServerPane");
        };
        let sp2_id = sp2.id;

        let Response::ClientPaneCreated { pane, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ClientSpawn {
                    workspace: "1".to_string(),
                    split_of: None,
                    dir: None,
                    bind: Some(sp1.id.to_string()),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ClientPaneCreated");
        };
        app.focused = Some(pane);

        app.open_add_tab_menu(&mut write_half, &mut reader)
            .await
            .unwrap();
        assert!(app.attach_menu.is_some(), "cmd-t should open the picker");
        assert!(
            app.attach_menu.as_ref().unwrap().adding_tab,
            "cmd-t's menu must be in add-tab mode"
        );
        assert_eq!(
            app.attach_menu.as_ref().unwrap().previously_bound,
            None,
            "cmd-t must not unbind/detach anything -- nothing was previously bound from this menu's point of view"
        );

        // Pick sp2. Row 0 is the "Unknown" group's header, row 1 is
        // sp2's own Server row (sp1 isn't listed here since this test
        // builds the menu's row list directly rather than fetching the
        // real, both-panes-included ServerList).
        app.attach_menu.as_mut().unwrap().servers = vec![("Unknown".to_string(), sp2)];
        app.attach_menu.as_mut().unwrap().selected = 1;
        app.confirm_attach_menu(&mut write_half, &mut reader)
            .await
            .unwrap();

        let Response::ClientPaneList { panes, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ClientList {
                    workspace: Some("1".to_string()),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ClientPaneList");
        };
        let leaf = panes
            .iter()
            .find(|p| p.id == pane)
            .expect("pane should still exist");
        assert_eq!(
            leaf.tabs,
            vec![sp1.id, sp2_id],
            "add-tab mode must append sp2 as a new tab, not replace sp1"
        );
        assert_eq!(
            leaf.active_tab, 1,
            "the newly-added tab should become active"
        );
    }

    /// `cmd-t` should default the cursor onto the trailing `SpawnNew`
    /// row ("generic new tab") rather than row 0 -- the whole point of
    /// cmd-t is opening a fresh tab, so a bare Enter right after opening
    /// the menu should do exactly that, not re-pick some existing
    /// session.
    #[tokio::test]
    async fn open_add_tab_menu_defaults_selection_to_spawn_new() {
        let (mut app, mut write_half, mut reader) = app_against_real_daemon().await;

        let Response::ServerPane(sp1) = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ServerSpawn {
                    name: None,
                    cmd: Some("cat".to_string()),
                    cwd: None,
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ServerPane");
        };
        let Response::ClientPaneCreated { pane, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ClientSpawn {
                    workspace: "1".to_string(),
                    split_of: None,
                    dir: None,
                    bind: Some(sp1.id.to_string()),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ClientPaneCreated");
        };
        app.focused = Some(pane);

        app.open_add_tab_menu(&mut write_half, &mut reader)
            .await
            .unwrap();

        assert_eq!(
            app.selected_attach_menu_row(),
            Some(AttachMenuRow::SpawnNew),
            "cmd-t must default to the generic new-tab row, not row 0"
        );
    }

    /// Plain Enter on a group's spawn field: spawns a new server-pane in
    /// that cwd, binds it into the focused client-pane, and sends the
    /// typed command. Exercises `confirm_spawn_in_group(bind: true, ..)`
    /// end to end against a real daemon.
    #[tokio::test]
    async fn spawn_in_group_enter_spawns_binds_and_sends_typed_text() {
        let (mut app, mut write_half, mut reader) = app_against_real_daemon().await;

        // A client-pane to bind into -- `focused` is `None` on an empty
        // workspace otherwise, and `confirm_spawn_in_group`'s bind path
        // is a no-op with nothing focused.
        let Response::ClientPaneCreated { pane, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ClientSpawn {
                    workspace: "1".to_string(),
                    split_of: None,
                    dir: None,
                    bind: None,
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ClientPaneCreated");
        };
        app.focused = Some(pane);

        let Response::ServerPane(existing) = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ServerSpawn {
                    name: None,
                    cmd: Some("cat".to_string()),
                    cwd: Some("/tmp".to_string()),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ServerPane");
        };
        let existing_id = existing.id;
        open_menu_with_one_group(&mut app, "/tmp", existing);

        app.handle_attach_menu_input(b" ", &mut write_half, &mut reader)
            .await
            .unwrap();
        app.handle_attach_menu_input(b"echo hi", &mut write_half, &mut reader)
            .await
            .unwrap();
        assert_eq!(
            app.attach_menu
                .as_ref()
                .unwrap()
                .spawn_in_group
                .as_ref()
                .unwrap()
                .text,
            "echo hi",
            "space should open the field, then typing should fill it"
        );

        app.handle_attach_menu_input(b"\r", &mut write_half, &mut reader)
            .await
            .unwrap();
        assert!(
            app.attach_menu.is_none(),
            "confirming should close the menu"
        );

        let Response::ClientPaneList { panes, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ClientList {
                    workspace: Some("1".to_string()),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ClientPaneList");
        };
        let bound_pane = panes
            .iter()
            .find(|p| p.id == pane)
            .expect("pane should still exist");
        let new_server_pane = bound_pane
            .active_bound()
            .expect("Enter should have bound the newly spawned pane");
        assert_ne!(
            new_server_pane, existing_id,
            "should bind the NEW pane, not the pre-existing one"
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let Response::ServerReadOutput { text } = app
                .request(
                    &mut write_half,
                    &mut reader,
                    Request::ServerRead {
                        target: new_server_pane.to_string(),
                    },
                )
                .await
                .unwrap()
            else {
                panic!("expected ServerReadOutput");
            };
            if text.contains("hi") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "typed command never echoed back"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// Shift+Enter on a group's spawn field: spawns and sends the typed
    /// command, but leaves the new pane unbound -- the focused
    /// client-pane's binding must be untouched.
    #[tokio::test]
    async fn spawn_in_group_shift_enter_spawns_and_sends_but_leaves_unbound() {
        let (mut app, mut write_half, mut reader) = app_against_real_daemon().await;

        let Response::ClientPaneCreated { pane, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ClientSpawn {
                    workspace: "1".to_string(),
                    split_of: None,
                    dir: None,
                    bind: None,
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ClientPaneCreated");
        };
        app.focused = Some(pane);

        let Response::ServerPane(existing) = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ServerSpawn {
                    name: None,
                    cmd: Some("cat".to_string()),
                    cwd: Some("/tmp".to_string()),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ServerPane");
        };
        let existing_id = existing.id;
        open_menu_with_one_group(&mut app, "/tmp", existing);

        app.handle_attach_menu_input(b" ", &mut write_half, &mut reader)
            .await
            .unwrap();
        app.handle_attach_menu_input(b"echo unbound-test", &mut write_half, &mut reader)
            .await
            .unwrap();
        app.handle_attach_menu_input(keys::SHIFT_ENTER_CHORD, &mut write_half, &mut reader)
            .await
            .unwrap();
        assert!(
            app.attach_menu.is_some(),
            "Shift+Enter should keep the menu open, unlike plain Enter"
        );
        assert!(
            app.attach_menu.as_ref().unwrap().spawn_in_group.is_none(),
            "the just-confirmed field should close even though the menu itself stays open"
        );
        assert!(
            app.attach_menu
                .as_ref()
                .unwrap()
                .servers
                .iter()
                .any(|(_, s)| s.id != existing_id),
            "the newly spawned pane should already be visible in the still-open menu's row list"
        );

        let Response::ClientPaneList { panes, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ClientList {
                    workspace: Some("1".to_string()),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ClientPaneList");
        };
        let bound_pane = panes
            .iter()
            .find(|p| p.id == pane)
            .expect("pane should still exist");
        assert_eq!(
            bound_pane.active_bound(),
            None,
            "Shift+Enter must not bind the new pane into the focused client-pane"
        );

        // The command should still have been sent to *some* new pane --
        // find it by listing server-panes and reading whichever one
        // isn't `existing`.
        let Response::ServerPaneList(server_panes) = app
            .request(&mut write_half, &mut reader, Request::ServerList)
            .await
            .unwrap()
        else {
            panic!("expected ServerPaneList");
        };
        let new_pane = server_panes
            .iter()
            .find(|p| p.id != existing_id)
            .expect("a second server-pane should have been spawned");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let Response::ServerReadOutput { text } = app
                .request(
                    &mut write_half,
                    &mut reader,
                    Request::ServerRead {
                        target: new_pane.id.to_string(),
                    },
                )
                .await
                .unwrap()
            else {
                panic!("expected ServerReadOutput");
            };
            if text.contains("unbound-test") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "typed command never echoed back"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// Arrow/`j`/`k` navigation off the spawn-in-group row must still
    /// work as ordinary selection movement, not open the field -- only
    /// plain printable bytes do that. Routed through the real
    /// `handle_attach_menu_input` (hence needing a live connection, even
    /// though this particular path never sends a request) rather than
    /// calling `move_attach_menu_selection` directly, so this actually
    /// exercises the dispatch logic that decides nav-vs-open-field.
    #[tokio::test]
    async fn spawn_in_group_row_nav_keys_move_selection_without_opening_the_field() {
        let (mut app, mut write_half, mut reader) = app_against_real_daemon().await;
        let Response::ServerPane(existing) = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ServerSpawn {
                    name: None,
                    cmd: Some("cat".to_string()),
                    cwd: Some("/tmp".to_string()),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ServerPane");
        };
        // Rows: 0 = "/tmp" header, 1 = existing server, 2 = spawn-in-group.
        open_menu_with_one_group(&mut app, "/tmp", existing);
        assert_eq!(
            app.selected_attach_menu_row(),
            Some(AttachMenuRow::SpawnNewInGroup(0))
        );

        app.handle_attach_menu_input(b"j", &mut write_half, &mut reader)
            .await
            .unwrap();
        assert!(
            app.attach_menu.as_ref().unwrap().spawn_in_group.is_none(),
            "`j` should navigate, not open the spawn field"
        );
        assert_eq!(
            app.selected_attach_menu_row(),
            Some(AttachMenuRow::SpawnNew),
            "`j` should move to the next row"
        );

        app.handle_attach_menu_input(b"\x1b[A", &mut write_half, &mut reader)
            .await
            .unwrap();
        assert_eq!(
            app.selected_attach_menu_row(),
            Some(AttachMenuRow::SpawnNewInGroup(0)),
            "up-arrow should move back"
        );
        assert!(app.attach_menu.as_ref().unwrap().spawn_in_group.is_none());
    }

    /// A filter/action key (`f`, here) pressed while a spawn-in-group row
    /// happens to be selected must still toggle the filter -- before the
    /// space-to-open fix, any non-navigation byte on this row opened the
    /// typed-command field instead, silently swallowing the very keys a
    /// user would press to fix an attach menu that isn't showing the
    /// panes they expect (this is exactly the bug report that prompted
    /// the fix: landing on a lone-pane group's spawn row and pressing a
    /// filter key did nothing visible).
    #[tokio::test]
    async fn spawn_in_group_row_still_dispatches_filter_and_action_keys() {
        let (mut app, mut write_half, mut reader) = app_against_real_daemon().await;
        let Response::ServerPane(existing) = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ServerSpawn {
                    name: None,
                    cmd: Some("cat".to_string()),
                    cwd: Some("/tmp".to_string()),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ServerPane");
        };
        open_menu_with_one_group(&mut app, "/tmp", existing);
        assert_eq!(
            app.selected_attach_menu_row(),
            Some(AttachMenuRow::SpawnNewInGroup(0))
        );
        let before = app.agents_only_view;

        app.handle_attach_menu_input(b"f", &mut write_half, &mut reader)
            .await
            .unwrap();

        assert!(
            app.attach_menu.as_ref().unwrap().spawn_in_group.is_none(),
            "`f` must not open the spawn field"
        );
        assert_eq!(
            app.agents_only_view, !before,
            "`f` must still toggle agents-only even though a spawn row was selected"
        );
    }

    /// `refresh_attach_menu_preview` fetches the selected server-pane's
    /// current screen via `Request::ServerRead` and caches it -- the
    /// full round-trip `run`'s loop relies on to keep the attach menu's
    /// preview panel showing live content.
    #[tokio::test]
    async fn refresh_attach_menu_preview_fetches_the_selected_servers_output() {
        let (mut app, mut write_half, mut reader) = app_against_real_daemon().await;
        let Response::ServerPane(existing) = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ServerSpawn {
                    name: None,
                    cmd: Some("cat".to_string()),
                    cwd: Some("/tmp".to_string()),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ServerPane");
        };
        let existing_id = existing.id;
        // Row 0 = "/tmp" header, row 1 = the server row itself.
        open_menu_with_one_group(&mut app, "/tmp", existing);
        app.attach_menu.as_mut().unwrap().selected = 1;
        assert_eq!(
            app.selected_attach_menu_row(),
            Some(AttachMenuRow::Server(0))
        );

        app.request(
            &mut write_half,
            &mut reader,
            Request::ServerSend {
                target: existing_id.to_string(),
                text: "preview-marker".to_string(),
                enter: true,
            },
        )
        .await
        .unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            app.refresh_attach_menu_preview(&mut write_half, &mut reader)
                .await
                .unwrap();
            if let Some((id, text)) = &app.attach_menu_preview
                && *id == existing_id
                && text.contains("preview-marker")
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "preview never picked up the sent text"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// Moving off the server row (onto the group header) must clear any
    /// cached preview -- otherwise a header/spawn row would render the
    /// previously selected pane's stale content.
    #[tokio::test]
    async fn refresh_attach_menu_preview_clears_when_selection_leaves_a_server_row() {
        let (mut app, mut write_half, mut reader) = app_against_real_daemon().await;
        let Response::ServerPane(existing) = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ServerSpawn {
                    name: None,
                    cmd: Some("cat".to_string()),
                    cwd: Some("/tmp".to_string()),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ServerPane");
        };
        open_menu_with_one_group(&mut app, "/tmp", existing);
        app.attach_menu.as_mut().unwrap().selected = 1;
        app.refresh_attach_menu_preview(&mut write_half, &mut reader)
            .await
            .unwrap();
        assert!(
            app.attach_menu_preview.is_some(),
            "expected a preview to be cached for the server row"
        );

        app.attach_menu.as_mut().unwrap().selected = 0; // the group header
        app.refresh_attach_menu_preview(&mut write_half, &mut reader)
            .await
            .unwrap();
        assert!(
            app.attach_menu_preview.is_none(),
            "selecting the header should clear the cached preview"
        );
    }

    /// Install a deterministic two-leaf, side-by-side tree into `app`,
    /// returning `(left, right)`. The daemon pushes its own tree via an
    /// async `LayoutDelta` broadcast that `app` only applies while inside
    /// a `request` call, so a `ClientSpawn`'s response landing does NOT
    /// mean the spawned leaf is in `app.workspace.tree` yet -- assigning
    /// the tree directly is what makes these mouse tests' geometry
    /// deterministic. `SplitTree` is the same type the daemon sends, and
    /// `render::leaf_rects`/`divider_rects` are pure functions of it, so
    /// the layout under test is identical either way.
    fn app_with_two_side_by_side_leaves(app: &mut App) -> (ClientPaneId, ClientPaneId) {
        let left = Uuid::new_v4();
        let right = Uuid::new_v4();
        app.workspace.tree = Some(split(SplitDir::Vertical, leaf(left), leaf(right)));
        app.focused = Some(left);
        app.frame_area = rect(0, 0, 80, 24);
        (left, right)
    }

    #[tokio::test]
    async fn click_inside_a_pane_focuses_it() {
        let (mut app, mut write_half, mut reader) = app_against_real_daemon().await;
        let (_left, right) = app_with_two_side_by_side_leaves(&mut app);

        // `right` occupies the rightmost half, so a click near the right
        // edge lands inside it regardless of the exact divider position.
        let _ = app
            .handle_mouse(
                mouse::MouseEvent::Down { col: 75, row: 5 },
                &mut write_half,
                &mut reader,
            )
            .await
            .unwrap();

        assert_eq!(
            app.focused,
            Some(right),
            "clicking inside the right pane should focus it"
        );
    }

    #[tokio::test]
    async fn dragging_pane_text_returns_it_for_clipboard_copy_and_keeps_highlight() {
        let (mut app, mut write_half, mut reader) = app_against_real_daemon().await;
        let (_left, right) = app_with_two_side_by_side_leaves(&mut app);
        let server_pane = Uuid::new_v4();
        let right_leaf = app
            .workspace
            .tree
            .as_mut()
            .unwrap()
            .find_mut(right)
            .unwrap();
        right_leaf.tabs = vec![server_pane];
        right_leaf.active_tab = 0;
        let cells = "abcdef"
            .chars()
            .map(|ch| crate::protocol::Cell {
                text: ch.to_string(),
                fg: None,
                bg: None,
                bold: false,
                italic: false,
                underline: false,
                reverse: false,
            })
            .collect();
        app.grids.insert(
            server_pane,
            GridSnapshot {
                server_pane,
                size: Size { rows: 1, cols: 6 },
                cursor: (0, 0),
                lines: vec![cells],
                scroll_offset: 0,
            },
        );
        let (_, right_rect) =
            render::leaf_rects(app.workspace.tree.as_ref().unwrap(), app.frame_area)
                .into_iter()
                .find(|(pane, _)| *pane == right)
                .unwrap();
        let start_col = right_rect.x + 1;
        let content_row = right_rect.y + 1;

        assert_eq!(
            app.handle_mouse(
                mouse::MouseEvent::Down {
                    col: start_col,
                    row: content_row
                },
                &mut write_half,
                &mut reader,
            )
            .await
            .unwrap(),
            None
        );
        assert_eq!(
            app.handle_mouse(
                mouse::MouseEvent::Drag {
                    col: start_col + 2,
                    row: content_row
                },
                &mut write_half,
                &mut reader,
            )
            .await
            .unwrap(),
            None
        );
        let copied = app
            .handle_mouse(
                mouse::MouseEvent::Up {
                    col: start_col + 2,
                    row: content_row,
                },
                &mut write_half,
                &mut reader,
            )
            .await
            .unwrap();

        assert_eq!(copied.as_deref(), Some("bcd"));
        let selection = app
            .text_selection
            .as_ref()
            .expect("highlight should remain after copying");
        assert!(!selection.is_dragging());
        assert!(selection.contains(0, 1));
        assert!(selection.contains(0, 3));
    }

    #[tokio::test]
    async fn click_on_a_divider_does_not_change_focus() {
        let (mut app, mut write_half, mut reader) = app_against_real_daemon().await;
        let (left, _right) = app_with_two_side_by_side_leaves(&mut app);

        // Read the divider's real column out of the same layout math
        // `handle_mouse` hit-tests against, rather than hardcoding it --
        // a 50/50 split of an 80-wide area lands it at 39, not 40.
        let divider = render::divider_rects(app.workspace.tree.as_ref().unwrap(), app.frame_area);
        let [hit] = divider.as_slice() else {
            panic!("expected exactly one divider, got {divider:?}")
        };
        let col = hit.grab_zone.x;

        let _ = app
            .handle_mouse(
                mouse::MouseEvent::Down { col, row: 5 },
                &mut write_half,
                &mut reader,
            )
            .await
            .unwrap();

        assert_eq!(
            app.focused,
            Some(left),
            "clicking the divider should start a drag, not change focus"
        );
        assert!(
            app.dragging_split.is_some(),
            "the click should have been recognized as a divider grab"
        );
    }

    /// Serializes every test in this module that spawns a real daemon
    /// AND toggles a pin: `State::new`/`toggle_pinned_dir` both read or
    /// write `$XDG_CONFIG_HOME` (process-global) via `daemon::pinned_dirs`,
    /// so without this a concurrently-running such test could read/write
    /// the wrong fake config dir mid-test -- and without redirecting
    /// `$XDG_CONFIG_HOME` at all, these tests would otherwise touch the
    /// real user's `~/.config/dimax/pinned_dirs.json`.
    static PIN_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn app_against_real_daemon_with_fake_pin_config(
        config_dir: &std::path::Path,
    ) -> (App, OwnedWriteHalf, FrameReader) {
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", config_dir);
        }
        app_against_real_daemon().await
    }

    /// `p` on a group header pins the directory: it must sort first
    /// (ahead of an alphabetically-earlier, unpinned directory) and the
    /// header must round-trip a pinned state back from the daemon after
    /// a fresh `PinnedDirsList` fetch -- exercises the full
    /// `Request::ToggleDirectoryPin`/`PinnedDirsList` wire round-trip,
    /// not just `State::toggle_pinned_dir` directly.
    #[tokio::test]
    async fn toggle_directory_pin_on_a_header_row_sorts_it_first() {
        let config_dir =
            std::env::temp_dir().join(format!("dmx-am-pin-config-{}", std::process::id()));
        let _guard = PIN_ENV_LOCK.lock().await;
        let (mut app, mut write_half, mut reader) =
            app_against_real_daemon_with_fake_pin_config(&config_dir).await;

        // Real directories -- `ServerSpawn`'s `cwd` does a genuine
        // `chdir`, which silently falls back to the daemon's own cwd
        // for a directory that doesn't exist, defeating the "which
        // group sorts first" check below.
        let dir_a = std::env::temp_dir().join(format!("dmx-am-pin-aaa-{}", std::process::id()));
        let dir_z = std::env::temp_dir().join(format!("dmx-am-pin-zzz-{}", std::process::id()));
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_z).unwrap();
        let (dir_a_str, dir_z_str) = (
            dir_a.to_str().unwrap().to_string(),
            dir_z.to_str().unwrap().to_string(),
        );

        let Response::ServerPane(a) = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ServerSpawn {
                    name: None,
                    cmd: Some("cat".to_string()),
                    cwd: Some(dir_a_str),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ServerPane");
        };
        let Response::ServerPane(z) = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ServerSpawn {
                    name: None,
                    cmd: Some("cat".to_string()),
                    cwd: Some(dir_z_str),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ServerPane");
        };
        // Read back the *actually resolved* cwd (e.g. macOS resolves
        // `/tmp` to `/private/tmp` for a real process) rather than
        // assuming the input string round-trips exactly.
        let resolved_a = a
            .foreground
            .as_ref()
            .and_then(|f| f.cwd.clone())
            .expect("cwd should resolve for `cat`");
        let resolved_z = z
            .foreground
            .as_ref()
            .and_then(|f| f.cwd.clone())
            .expect("cwd should resolve for `cat`");

        let Response::ServerPaneList(servers) = app
            .request(&mut write_half, &mut reader, Request::ServerList)
            .await
            .unwrap()
        else {
            panic!("expected ServerPaneList");
        };
        app.attach_menu = Some(AttachMenu {
            servers: group_servers_by_cwd(servers, &app.pinned_dirs),
            selected: 0,
            pending_delete: None,
            rename: None,
            previously_bound: None,
            spawn_in_group: None,
            adding_tab: false,
        });
        // Before pinning: alphabetically-earlier `resolved_a` sorts
        // first (both dirs share the same "dmx-am-pin-" prefix, so
        // ordinary string comparison between them is exactly "aaa" vs
        // "zzz").
        let sort_key = |app: &App| app.attach_menu.as_ref().unwrap().servers[0].0.clone();
        assert_eq!(sort_key(&app), resolved_a);

        // Select `dir_z`'s header (row 3: header(a)=0, server(a)=1,
        // spawn-in-group(a)=2, header(z)=3) and pin it.
        app.attach_menu.as_mut().unwrap().selected = 3;
        assert_eq!(
            app.selected_attach_menu_row(),
            Some(AttachMenuRow::GroupHeader(1)),
            "row 3 should be dir_z's header"
        );
        app.toggle_directory_pin(&mut write_half, &mut reader)
            .await
            .unwrap();

        assert_eq!(app.pinned_dirs, vec![resolved_z.clone()]);
        assert_eq!(
            sort_key(&app),
            resolved_z,
            "pinning dir_z should move it to the front of the grouped list"
        );

        // Confirm the pin also comes back from a totally fresh fetch
        // (i.e. it's real daemon-side state, not just local bookkeeping).
        let Response::PinnedDirsList(pinned) = app
            .request(&mut write_half, &mut reader, Request::PinnedDirsList)
            .await
            .unwrap()
        else {
            panic!("expected PinnedDirsList");
        };
        assert_eq!(pinned, vec![resolved_z]);

        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
        }
        let _ = std::fs::remove_dir_all(&config_dir);
        let _ = std::fs::remove_dir_all(&dir_a);
        let _ = std::fs::remove_dir_all(&dir_z);
    }

    /// `p` on a `Server` row must pin that row's own directory, exactly
    /// as if its group's header had been selected instead -- the only
    /// way to pin at all while `App.grouped_view` is off (see
    /// `toggle_directory_pin`'s doc comment), since the flat view has no
    /// header rows to select in the first place.
    #[tokio::test]
    async fn toggle_directory_pin_on_a_server_row_pins_its_group() {
        let config_dir = std::env::temp_dir().join(format!(
            "dmx-am-pin-server-row-config-{}",
            std::process::id()
        ));
        let _guard = PIN_ENV_LOCK.lock().await;
        let (mut app, mut write_half, mut reader) =
            app_against_real_daemon_with_fake_pin_config(&config_dir).await;

        let dir =
            std::env::temp_dir().join(format!("dmx-am-pin-server-row-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let Response::ServerPane(existing) = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ServerSpawn {
                    name: None,
                    cmd: Some("cat".to_string()),
                    cwd: Some(dir.to_str().unwrap().to_string()),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ServerPane");
        };
        let resolved = existing
            .foreground
            .as_ref()
            .and_then(|f| f.cwd.clone())
            .expect("cwd should resolve for `cat`");
        open_menu_with_one_group(&mut app, &resolved, existing);
        app.attach_menu.as_mut().unwrap().selected = 1; // the server row, not the header
        assert_eq!(
            app.selected_attach_menu_row(),
            Some(AttachMenuRow::Server(0))
        );

        app.toggle_directory_pin(&mut write_half, &mut reader)
            .await
            .unwrap();

        assert_eq!(app.pinned_dirs, vec![resolved]);

        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
        }
        let _ = std::fs::remove_dir_all(&config_dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `toggle_directory_pin` on the trailing generic `SpawnNew` row
    /// must stay a genuine no-op: it carries no group index at all, so
    /// there is no directory to pin from it (verified indirectly -- if a
    /// request had been sent, this daemon has nothing pinned, and
    /// `app.pinned_dirs` would be mutated by the response handling
    /// either way, so asserting it's still empty after calling this
    /// confirms the early-return path, not just "nothing happened to
    /// look at").
    #[tokio::test]
    async fn toggle_directory_pin_on_the_spawn_new_row_is_a_no_op() {
        let (mut app, mut write_half, mut reader) = app_against_real_daemon().await;
        let Response::ServerPane(existing) = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ServerSpawn {
                    name: None,
                    cmd: Some("cat".to_string()),
                    cwd: Some("/tmp".to_string()),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ServerPane");
        };
        open_menu_with_one_group(&mut app, "/tmp", existing);
        app.attach_menu.as_mut().unwrap().selected = 3; // header(0), server(1), spawn-in-group(2), spawn-new(3)
        assert_eq!(
            app.selected_attach_menu_row(),
            Some(AttachMenuRow::SpawnNew)
        );

        app.toggle_directory_pin(&mut write_half, &mut reader)
            .await
            .unwrap();
        assert!(
            app.pinned_dirs.is_empty(),
            "toggling on the spawn-new row must not pin anything"
        );
    }
}
