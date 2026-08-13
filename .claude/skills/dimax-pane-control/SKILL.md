---
name: dimax-pane-control
description: Drive dimax entirely via the `dimax` CLI over Bash -- spawn/read/send/rename/kill server-panes, create and manage client-panes and workspaces, pin directories, configure keybindings, hot-reload the daemon, and install/inspect the skill itself. Use when the user asks to run something in a "dimax pane", keep a long-running process alive across turns, inspect an existing dimax pane, drive dimax's workspace/tab layout headlessly, or otherwise control dimax from the command line rather than through its interactive TUI.
---

# dimax pane control

`dimax` is a tmux-style terminal multiplexer with a daemon/client architecture. Every `dimax` subcommand below is fully scriptable and auto-starts the daemon on first use if it isn't already running -- nothing in this skill requires opening the interactive TUI (`dimax attach`).

## Core loop: spawn -> send -> read

The most common need: run something in the background, feed it input, read its output.

```bash
# 1. Start a pane running a shell (or any command) in the background.
dimax server spawn build-shell
# -> spawned server-pane build-shell (3f9a1e2c-...-b7d1)

# 2. Type a command into it, as if you'd typed it and hit Enter.
dimax server send build-shell "npm run build" --enter

# 3. Give it a moment to produce output, then read the current screen.
sleep 2
dimax server read build-shell
```

- **Addressing**: every `server` subcommand's target accepts either the pane's **name** (what you passed to `spawn`) or its **UUID** -- use the name, it's what a human (and you) can actually read back later. Names must be unique; `spawn` errors if the name is taken.
- **`send --enter`**: appends a newline after the text, matching a user typing the line and pressing Enter. Omit `--enter` to send raw keystrokes with no newline (e.g. to answer a `y/n` prompt char-by-char, or send a control sequence).
- **`read` is a one-shot snapshot** of whatever is on screen *right now* -- it does not block or wait for the command to finish. There is no "wait for output" or "wait for idle" command yet. Poll it yourself:

```bash
dimax server send build-shell "long-running-command; echo DONE-MARKER-1" --enter
for i in $(seq 1 30); do
  sleep 1
  out=$(dimax server read build-shell)
  if echo "$out" | grep -qx 'DONE-MARKER-1'; then   # -x: exact line match
    break
  fi
done
echo "$out"
```

Don't rely on matching the shell prompt itself to detect "done" -- the prompt varies by shell/config (a login shell may print `bash-3.2$`, `❯`, or something else entirely you can't predict up front) and a fixed sleep duration is a guess that's either too short or wastes time. Appending `; echo <unique-marker>` to the command you send is the reliable way to know it finished, for any shell.

**Use `grep -qx`, not a plain substring `grep -q`, for the marker.** The pane's screen shows the typed command *and* its output -- the command you sent (`... echo DONE-MARKER-1`) is echoed back onto the screen the moment you send it, before it's actually run. A substring match on `DONE-MARKER-1` matches that echoed command line immediately, giving a false "done" before the command has even started. Matching the *exact* line (`-x`) only matches the marker's actual printed output, not the command text that produced it.

## `dimax server ...` -- server-panes (persistent background processes)

| Command | Effect |
|---|---|
| `dimax server spawn <name> [--cmd "<command>"] [--cwd <dir>]` | Start a new server-pane named `<name>`, running `<command>` (default: the user's `$SHELL` as a login shell) with starting directory `<dir>` (default: the daemon's own cwd). Prints the new pane's id. |
| `dimax server send <target> <text> [--enter]` | Write `<text>` into the pane's stdin, as if typed. `--enter` adds a trailing newline. |
| `dimax server read <target>` | Print the pane's current visible screen as plain text (styling/color stripped). Trailing blank rows are trimmed. |
| `dimax server ls` | List every server-pane: `id  name  status  rowsxcols  process  cwd  kind`, tab-separated, one per line. `status` is `running` or `dead`. `kind` is `claude`/`codex`/`opencode`/`omp`/`herdr` when the foreground process is a recognized AI-coding CLI tool, `-` otherwise -- a stable tag, not something you need to re-derive by pattern-matching `process`. |
| `dimax server rename <target> <new-name>` | Rename a pane. Errors if `<new-name>` is already taken. |
| `dimax server kill <target>` | Terminate the pane's process and remove it from the daemon. Do this when you're done with a pane you spawned -- nothing else cleans it up. |

`<target>` is always name-or-id, consistently, across `send`/`read`/`rename`/`kill`.

**Practical notes:**
- **Always give panes a name at spawn time.** An unnamed pane only has a UUID to address it by, which you'd have to scrape from `spawn`'s output or `ls` and carry around.
- **Clean up panes you spawn** with `dimax server kill <name>` once done, unless the user asked for the pane to be left running (e.g. a dev server they want to keep using). Check `dimax server ls` if unsure what's still around from earlier in the session.
- **Check before spawning a duplicate.** `dimax server ls` first rather than blindly spawning a second pane for the same purpose -- `spawn` also errors if you reuse a name that's still alive.
- **`read` shows exactly what's on screen**, including the shell prompt and command echo -- parse it like a terminal transcript, not a clean command-output capture. A pane running an interactive REPL, editor, or anything else that redraws the screen reads back as that screen's visible contents, not a linear log.

## `dimax client ...` -- client-panes and workspaces (the TUI's grid, headlessly)

A *workspace* is a named split-tree of *client-panes*; each client-pane can be bound to a server-pane (or several, as tabs it cycles between). Workspaces are created implicitly by spawning the first pane into a new name.

| Command | Effect |
|---|---|
| `dimax client spawn <workspace> [--split <pane-uuid>] [--dir h\|v] [--bind <target>]` | Create a client-pane in `<workspace>` (created if it doesn't exist yet). With no `--split`, the workspace must currently be empty. `--split <pane-uuid>` splits an existing leaf; `--dir h` (side-by-side) or `v` (stacked) picks the split direction (default `h`). `--bind <target>` binds a server-pane (by name or id) into the new leaf immediately. Prints `<workspace>/<pane-id>`. |
| `dimax client ls [workspace]` | List client-panes: `id  name  active-server-pane-or-dash  active-index/tab-count-or-dash`. Omit `workspace` to list across all workspaces. |
| `dimax client bind <addr> <target>` | Bind a server-pane into an existing client-pane, replacing its active tab. |
| `dimax client unbind <addr>` | Detach the client-pane's active tab, leaving it an empty placeholder. The server-pane keeps running. |
| `dimax client add-tab <addr> <target>` | Bind a server-pane as an *additional* tab (not a replacement) and make it active. |
| `dimax client cycle-tab <addr> [--backward]` | Step the active tab forward (default) or backward, wrapping. No-op on fewer than two tabs. |
| `dimax client close-tab <addr>` | Drop the active tab (its server-pane keeps running). Closes the whole pane if that was the last tab. |
| `dimax client rename <addr> <new-name>` | Rename the client-pane leaf itself (distinct from the server-pane's own name). |
| `dimax client close <addr>` | Close the whole client-pane leaf, dropping every tab it had. |

`<addr>` is `<workspace>/<pane-id>` (e.g. `1/3f9a1e2c-...`) -- both `spawn` and `add-tab`/`bind` print or accept this form.

Most automation only needs `server ...` (background process control). Reach for `client ...` when the user specifically wants something to show up in their interactive `dimax attach` grid -- e.g. "open a pane in my dimax session running X" rather than "run X in the background."

## `dimax pin ...` -- pinned directories

The attach menu groups server-panes by their working directory; a pinned directory always sorts first (earliest-pinned first), matching the `p` key in that menu.

| Command | Effect |
|---|---|
| `dimax pin add <dir>` | Pin `<dir>`. No-op (still succeeds) if already pinned. |
| `dimax pin remove <dir>` | Unpin `<dir>`. No-op if not currently pinned. |
| `dimax pin list` | List currently pinned directories, one per line, earliest-pinned first. |

`<dir>` is matched verbatim against a server-pane's foreground cwd -- pinning a directory with nothing in it yet is fine, it just has no effect until something appears there.

## `dimax keys ...` -- keybinding configuration

| Command | Effect |
|---|---|
| `dimax keys install --mode <portable\|kitty\|both\|tmux> [--reload] [-y]` | Select a keybinding mode. `kitty`/`both` amend `~/.config/kitty/kitty.conf` (prompts for confirmation on a real terminal; `-y`/non-interactive skips the prompt). `--reload` tells a running Kitty to pick up the change immediately. |
| `dimax keys uninstall` | Remove managed Kitty bindings; portable bindings remain active. |
| `dimax keys print --mode <mode>` | Print the binding table for a mode without changing any files. |
| `dimax keys list` | Show the active mode and its binding table. |
| `dimax keys bind <action> [--portable <seq>] [--kitty <combo>]` | Add an alias for an existing action. `dimax keys list` shows valid action names. |
| `dimax keys unbind [--portable <seq>] [--kitty <combo>]` | Remove a specific alias. |
| `dimax keys reset` | Remove every custom alias, restoring defaults. |

## `dimax daemon reload` -- hot reload

Upgrades the running daemon onto whatever binary is currently installed, without killing any server-pane -- the daemon re-execs its own process image in place. Run this after `cargo install`ing a new build. Prints once the reload has been *attempted*, not confirmed successful (a successful reload's `execve` never returns to report back) -- follow up with `dimax server ls` to confirm the daemon came back.

## `dimax skills install` -- (re)install this skill

Writes this skill's `SKILL.md` to `~/.claude/skills/dimax-pane-control/`, overwriting any previous copy. The content ships inside the `dimax` binary itself (`include_str!`), so this works the same from any install method. Global, not per-project.

## `dimax config` -- edit Kitty chord mappings

Regenerates `dimax.conf` (the Kitty Cmd-key mapping file) if needed, then opens it in `$EDITOR`/`$VISUAL`. Rarely needed directly -- `dimax keys install`/`bind`/`unbind` cover normal keybinding changes.

## Out of scope for this skill

- **The interactive TUI itself** (`dimax attach`) -- this skill's commands work headlessly against the same daemon and don't require (or interact with) any attached TUI session. A pane spawned via `dimax server spawn` shows up in `dimax server ls` and can be bound into a TUI workspace later (`dimax client spawn --bind <target>`, or the attach menu) if the user wants to watch it interactively.
- **Waiting for output / idle detection** -- there is no such command; poll `dimax server read` with a unique echo marker, per the Core loop section above.
- **Resizing an existing split's ratio** -- only reachable via the TUI's mouse-drag today; there's no CLI introspection of split ids to target one, so no CLI command for it either.
