# dimux — design

## Summary

`dimux` is a tmux-style, client/server terminal multiplexer written in Rust.
A background daemon owns two pools of global, shared state:

- **server-panes** — PTY-backed processes (a shell, an editor, whatever).
- **workspaces** — named/numbered groups of **client-panes**, arranged as a
  split tree. Each client-pane binds to zero or one server-pane by id.

Both pools are live-synced across every attached frontend (a `dimux attach`
process, normally run inside a Kitty window) — the same way tmux mirrors a
session's window layout across multiple attached terminals. A single
server-pane can be bound into many client-panes at once, even across
different workspaces or different frontends. Everything the TUI can do
(spawn/close/rename/split/rebind panes) is also available as a CLI command,
and CLI-triggered changes appear live in any frontend currently viewing the
affected workspace.

`name` fields on both server-panes and client-panes are just a human-legible
alias alongside their `id` — convenient for CLI reference (`dimux client
close dev/editor`) and for the picker UI. They are not a saved/loadable
config mechanism; there is no separate "save layout" feature — the daemon's
live state *is* the only persisted-while-running layout, and every frontend
sees the same thing.

## Non-goals

- No named/saved layout snapshots or config profiles (rejected during design
  — see "Sync scope" decision below; live sync replaces this).
- No support for terminal emulators other than Kitty in v1 (Cmd-key
  forwarding depends on Kitty's `map` remap system).
- No cross-machine/remote attach (mosh-style) — local Unix socket only.
- No persistence across daemon restarts — server-panes and workspaces are
  in-memory; a daemon restart loses all panes (matches tmux without
  tmux-resurrect).

## Architecture

Single binary, `dimux`, three roles over one protocol:

1. **Daemon** — long-lived background process, one per user, listening on a
   Unix domain socket (`$XDG_RUNTIME_DIR/dimux.sock`, falling back to
   `/tmp/dimux-$UID.sock` if `XDG_RUNTIME_DIR` is unset). Owns all state:

   - **Server-pane pool**: `{id, name, state, size}` where `state` is
     `Running(portable_pty::Child handle, wezterm_term::Terminal grid)` or
     `Dead(last grid snapshot)`. The daemon spawns the process, feeds its
     output bytes into the `wezterm-term` parser, and keeps the resulting
     cell grid as the source of truth for "what does this pane look like."
   - **Workspaces**: `Vec<{id, number, name, split_tree}>` where
     `split_tree` is a binary tree of `Split(dir, ratio, Box<Node>,
     Box<Node>)` / `Leaf(ClientPane)`, and `ClientPane = {id, name,
     bound_server_pane: Option<ServerPaneId>}`.
   - A registry of connected frontends, each tagged with the workspace it's
     currently viewing (or none, right after connecting).

2. **Frontend** (`dimux attach`) — holds no durable state of its own. On
   attach: lists workspaces, subscribes to one (see Protocol below), renders
   via `ratatui`, applies deltas as they stream in. Translates Kitty-forwarded
   Cmd-chords and normal keystrokes into either local view changes (switch
   workspace, move focus — pure client-side) or commands sent to the daemon
   (split, close, rebind, spawn — same command path the CLI uses).

3. **CLI** (`dimux server ...`, `dimux client ...`) — connects, sends one
   command, prints the result, exits. Uses the exact same request types the
   frontend sends internally; the daemon does not distinguish CLI callers
   from frontend callers except that frontends additionally hold a live
   subscription.

### Why global, synced workspaces (not per-frontend)

Originally considered scoping workspaces/client-panes to the frontend that
created them, with the CLI addressing panes as
`<frontend>/<workspace>/<pane>`. Rejected: the whole point of CLI-driven live
splitting is that a script or a second window can reshape a layout and see
it reflected wherever that workspace is open — per-frontend ownership would
mean the CLI could only affect one specific window's private copy, and
"loading dimux from a new window" would show nothing shared by default. Global
workspaces make `<workspace>/<pane>` a complete, frontend-independent address,
and dropping the frontend segment simplifies every CLI command.

## Protocol

Length-prefixed JSON frames (`u32` little-endian length + UTF-8 JSON body)
over the Unix socket. JSON chosen over a binary format for debuggability
(`socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/dimux.sock` + manual frames while
developing) at negligible cost for single-machine, human-scale pane counts.

### Subscription model

A frontend does **not** receive a firehose of all daemon activity. Instead:

- On attach, and on every workspace switch, the frontend sends
  `Subscribe(workspace_id)`. The daemon responds with a full snapshot: the
  workspace's current split tree, plus a full grid dump for every
  server-pane currently bound within it.
- While subscribed, the daemon streams:
  - `LayoutDelta` events (split added/removed, pane renamed, rebind) for
    that workspace.
  - `GridDelta` events (changed cells/lines) for any server-pane bound
    within that workspace.
- Switching workspaces sends `Unsubscribe(old)` then `Subscribe(new)` — no
  incremental diffing across the switch; a fresh snapshot avoids ever
  reasoning about missed deltas.
- The daemon tracks, per server-pane, the set of frontends currently
  subscribed to a workspace that binds it. This set is exactly "who is
  currently viewing this server-pane," which is what the PTY resize rule
  (smallest-viewer-wins, see below) uses to compute PTY size on every
  subscribe/unsubscribe/resize event.

CLI commands don't subscribe — they send one request (e.g.
`ClientSpawn{workspace, split_of: Option<pane_id>, dir, bind: Option<name_or_id>}`),
get one `Ack`/`Error` response, and disconnect. The mutation itself is
applied to the shared workspace state and broadcast to subscribed frontends
exactly like a frontend-originated command would be.

### PTY sizing

A server-pane's PTY size = the smallest (rows × cols, taken dimension-wise)
among all client-panes currently displaying it, across all subscribed
frontends. Recomputed whenever: a client-pane binds/unbinds it, a frontend
subscribes/unsubscribes from a workspace that binds it, or any such
client-pane is resized (frontend terminal resize, or a split ratio change).
If the last viewer disappears, the PTY keeps its last size (no viewers to
be smallest-of).

## Data model reference

```
ServerPane   { id: Uuid, name: String, size: (u16, u16), state: Running | Dead }
Workspace    { id: Uuid, number: u8, name: String, tree: SplitTree }
SplitTree    = Leaf(ClientPane) | Split { dir: H | V, ratio: f32, a: Box<SplitTree>, b: Box<SplitTree> }
ClientPane   { id: Uuid, name: String, bound: Option<ServerPaneId> }
```

`number` (1–9) is what `cmd-1`..`cmd-9` switches to; `name` is optional and
independent, used only for CLI/picker legibility. Switching to a number with
no existing workspace creates one, empty, on the fly.

## CLI surface

```
dimux attach                                   # launch TUI, auto-start daemon if needed

dimux server spawn  <name> [--cmd <shell-cmd>] # default $SHELL if --cmd omitted
dimux server kill   <name-or-id>
dimux server rename <name-or-id> <new-name>
dimux server ls

dimux client spawn   <workspace> [--split <pane-id> --dir h|v] [--bind <server-name-or-id>]
dimux client close   <workspace>/<pane-id>       # closes the client-pane; bound server-pane keeps running
dimux client rename  <workspace>/<pane-id> <new-name>
dimux client bind    <workspace>/<pane-id> <server-name-or-id>
dimux client unbind  <workspace>/<pane-id>       # detaches; bound server-pane keeps running
dimux client ls      [workspace]                 # omit workspace to list all
```

`dimux client spawn` with no `--split` creates a new top-level leaf in an
otherwise-empty workspace (error if the workspace already has panes and no
split point was given — ambiguous where to put it). Every `spawn`/`bind`
targeting a workspace some frontend is currently subscribed to results in
that frontend seeing the change immediately (this is "live splitting" from
the CLI, the feature that motivated dropping per-frontend layout state).

## Default keybinds (TUI)

Forwarded from Kitty via its `map` remap config (each Cmd-chord bound to
`send_text` with a distinct escape sequence dimux's input parser recognizes).
Setup docs will include the exact `kitty.conf` snippet.

| Chord | Action |
|---|---|
| `cmd-1`..`cmd-9` | switch to workspace N (create if absent) |
| `cmd-d` | split focused client-pane vertically, spawn a fresh server-pane and bind it into the new half |
| `cmd-shift-d` | split focused client-pane horizontally, spawn a fresh server-pane and bind it into the new half |
| `cmd-w` | close focused client-pane (server-pane keeps running) |
| `cmd-shift-w` | kill focused client-pane's bound server-pane |
| `cmd-shift-z` | detach focused client-pane (server-pane keeps running), open attach menu to pick its replacement |
| `cmd-h/j/k/l` | move focus between client-panes |
| *(mouse drag)* | drag a divider to resize the two panes it separates — no keybind, drag-only |

Superseded: an earlier revision of this doc specified a `cmd-p` chord
opening a picker overlay (choose an existing server-pane, or "spawn new")
before binding it into the focused client-pane. That picker was built,
then removed — `cmd-d`/`cmd-shift-d` becoming an instant "split + new
shell" shortcut covers the common case with one keystroke instead of two.
The picker's other capability — binding a client-pane to an *existing*
server-pane, e.g. to display one shell in two places — now has two paths:
`cmd-shift-z` (detach the focused pane, then pick a replacement from a
rebuilt attach menu) in the TUI, or `dimux client bind <workspace>/<pane>
<server-name-or-id>` directly from the CLI without detaching first.

### Attach menu identification columns

The attach menu (and `dimux server ls`) list every server-pane with five
columns: `name | cwd | process | id | status`. `name` is the user-given
name (via `dimux server rename`), falling back to an 8-character id
prefix if unset — the same fallback `render::short_id` uses elsewhere.
`cwd` and `process` come from `ServerPaneInfo.foreground: Option<
ForegroundProcessInfo>`, a live OS-level snapshot of the PTY's
*foreground* process (e.g. `vim` if you ran it inside the pane's shell,
not the shell itself) — queried fresh every time a `ServerPaneInfo` is
built, not tracked or cached, since callers already re-fetch on their
own cadence (the attach menu re-lists on every open).

Getting the foreground process is two steps: `portable_pty::MasterPty::
process_group_leader()` (already provided by the crate, no unsafe code
needed) gives the PID of whichever process is currently in the PTY's
foreground process group; that PID is then looked up via the `sysinfo`
crate for its command name and working directory. `cwd` is start-truncated
(`.../project/src`) and `process`/`name` are end-truncated
(`a-very-long-...`) to fit the menu's fixed column widths — the tail of a
path is usually the more informative end, while the head of a name/process
usually is.

### Divider resizing

Each `SplitTree::Split` node carries a stable `id: SplitId`, independent
of its `a`/`b` children's contents, so a specific divider can be
addressed for resizing without needing to name a pane on either side
(which may itself be a nested split with no single pane id of its own).
`Request::ResizeSplit { workspace, split, new_ratio }` sets that divider's
ratio directly; the daemon clamps `new_ratio` to `[0.05, 0.95]` so a drag
can shrink a pane small but never collapse it to zero or push it past the
opposite pane's minimum.

The TUI hand-parses the SGR mouse escape-sequence protocol
(`ESC [ < Cb ; Cx ; Cy M/m`) directly off stdin, the same way it
hand-parses Kitty's Cmd-chord sequences rather than using crossterm's
event abstraction (see "Requests are sent through an `Event`-tolerant
helper" design note in `tui/mod.rs` for why raw bytes are read at all).
On mouse-down, it hit-tests the click position against every divider's
current on-screen grab zone; while held, every mouse-move updates the
drag's ratio purely locally (rendered via a workspace clone with the live
ratio patched in — no network round-trip per move). Exactly one
`Request::ResizeSplit` is sent, on mouse-up, committing the final
position — other frontends see the resize once it's released, not live
throughout the drag. (An earlier revision sent `ResizeSplit` on every
`Drag` event; at typical drag speed this was dozens of requests per
second, each awaited synchronously before the event loop's next stdin
read, badly enough to stall the whole session.)

At startup, the TUI enables only xterm's normal (`?1000h`) and
button-event (`?1002h`) mouse tracking plus SGR extended coordinates
(`?1006h`) — deliberately not any-event tracking (`?1003h`, which reports
*every* mouse movement, including with no button held at all). An earlier
revision used `ratatui::crossterm::event::EnableMouseCapture`, which
enables all four unconditionally; under `?1003h`, moving the mouse over
the window for any reason — not just dragging a divider — generated a
mouse escape sequence dimux's parser didn't recognize (a bare-movement
event encodes as SGR button number 3, which dimux has no binding for),
and an unrecognized mouse byte used to fall through to the keyboard-chord
parser, which also didn't recognize it, and wrote the raw escape sequence
into the focused pane as literal text — the "random garbage characters"
symptom, with enough volume during normal mouse use to also stall the
event loop ("random hangups"). As defense in depth beyond narrowing which
modes are requested, `tui::mouse::parse` also now recognizes *any*
well-formed SGR mouse sequence (any button, any event kind) as
"definitely mouse input" and discards it rather than falling through, so
even a stray sequence from a terminal that doesn't fully honor the
narrower mode request still can't leak into a pane as text.

### PTY read batching

A second "dimux hangs often" investigation, separate from the mouse
report above, traced a real freeze to a different cause: the daemon's
one global state lock (see "Concurrency model" at the top of this doc)
being held not just during request handling but during PTY-output
broadcasting too. Every `ServerPaneEvent::Changed` — one per individual
PTY `read()` — used to trigger `broadcast_grid`: lock the global state,
snapshot the pane's grid, serialize it to JSON, and push it to
subscribers, all under that one lock. Measured directly against a
fast-scrolling pane (`yes`): serializing a single, ordinary 24×80 grid
took ~5ms on its own, and the reader thread was producing ~2,584 such
events per second — meaning the lock was essentially always held by
this broadcast path during a flood, starving every other request the
daemon needed to handle (including keystrokes typed into an unrelated
pane, which is what made the freeze visible).

Two independent fixes, at two different layers:

- **`daemon::mod.rs`**: `broadcast_grid` was split into
  `broadcast_grid_prepare` (cheap — checks subscribers, and only if any
  exist, copies the grid; called while the lock is held) and
  `broadcast_grid_send` (the expensive serialize+push; called *after*
  the lock is released). A pane nobody is currently viewing now costs
  nothing at all, not even a snapshot.
- **`term/mod.rs`**: the reader thread's read loop no longer fires one
  `Changed` event per individual `read()`. After the first (blocking)
  read, it sleeps for a short `BATCH_WINDOW` (8ms), then drains
  whatever else has accumulated in the kernel's PTY buffer
  (non-blocking, via a brief `O_NONBLOCK` toggle on the underlying fd —
  `try_clone_reader`'s fd is `dup()`'d from the master's, and POSIX file
  *status* flags are shared across `dup()`'d descriptors, so this needs
  no second real fd) before applying everything as one `advance_bytes`
  call and firing one event. An earlier attempt (drain immediately,
  no delay) measured no improvement at all — the reader thread's own
  loop ran faster than `yes` could refill the buffer, so there was
  essentially never anything extra sitting there to grab. Adding the
  fixed delay before draining is what actually gives a fast producer
  time to queue up multiple reads' worth of output first; it cut the
  measured event rate for the same `yes` workload from ~2,584/sec to
  ~90/sec (matching the `1000ms / 8ms` ceiling), while adding only
  ~10ms of latency to a single, isolated keystroke's echo — well under
  typical human perception thresholds for input lag.

## Error handling

- Any CLI/TUI command connecting to a missing socket triggers auto-spawn of
  the daemon (double-fork + detach), then retries the connection once,
  tmux-style.
- A server-pane's underlying process exiting transitions it to `Dead` state
  (last grid snapshot retained, rendered greyed-out in any client-pane still
  bound to it) rather than disappearing. Only `dimux server kill` (or the
  daemon exiting) removes it from the pool.
- If a client-pane's bound server-pane is killed while displayed, that
  client-pane switches to an "unbound" placeholder (not auto-closed, not
  auto-rebound).
- Any command referencing an unknown workspace/pane/server-pane id/name is
  a clean `Error{message}` response — CLI prints to stderr and exits
  non-zero; TUI shows a transient status-line message. No partial mutation
  on error (validate-then-apply, not apply-then-rollback).

## Testing

- **Unit**: protocol frame serde round-trips; `SplitTree` operations
  (insert-at-leaf, remove-leaf, rebind) as pure data-structure logic with no
  PTY/socket involved; PTY-sizing computation given a set of viewer
  rects.
- **Integration**: daemon spun up in-process against a temp socket path.
  Drive it through the same client library the CLI/frontend use:
  - spawn a server-pane running `cat`, write bytes to its PTY, assert the
    resulting `wezterm-term` grid content.
  - open two mock frontend connections subscribed to the same workspace,
    issue a `ClientSpawn` (split) as a bare CLI-style request from a third
    connection, assert both mock frontends receive the matching
    `LayoutDelta`.
  - kill a server-pane bound in two workspaces at once, assert both
    corresponding client-panes transition to unbound/dead.
- **Manual**: actual rendering and Cmd-chord feel under Kitty — not
  practically covered by automated tests; verified by hand before each
  release.

## Crate layout (initial)

Single Rust crate to start (split into a workspace of multiple crates later
only if/when a real boundary emerges — no speculative split now):

```
dimux/
  src/
    main.rs        # subcommand dispatch: attach / server / client
    daemon/        # server-pane pool, workspace state, socket listener
    protocol.rs     # wire types (requests/responses/events), framing
    term/           # wezterm-term + portable-pty glue (spawn, feed, snapshot)
    tui/            # ratatui compositor, keybind parsing, picker overlay
    cli.rs          # `dimux server`/`dimux client` command implementations
  Cargo.toml
  docs/superpowers/specs/
```

## Key dependencies

- `wezterm-term` + `portable-pty` — terminal emulation + PTY spawning.
- `ratatui` — TUI layout/rendering/compositing.
- `tokio` — async runtime for the daemon (socket listener, per-connection
  tasks, PTY read loops) and frontend (socket + input handling).
- `serde` / `serde_json` — protocol frame (de)serialization.
- `uuid` — pane/workspace ids.
