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
| `cmd-d` | split focused client-pane vertically, open picker in new pane |
| `cmd-shift-d` | split focused client-pane horizontally, open picker in new pane |
| `cmd-p` | open picker to rebind focused client-pane (no split) |
| `cmd-w` | close focused client-pane (server-pane keeps running) |
| `cmd-shift-w` | kill focused client-pane's bound server-pane |
| `cmd-h/j/k/l` | move focus between client-panes |

The picker overlay lists all server-panes (id, name, running/dead, current
size) plus a "spawn new" entry; selecting an existing one binds it,
selecting "spawn new" prompts for a name/command and binds the result.

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
