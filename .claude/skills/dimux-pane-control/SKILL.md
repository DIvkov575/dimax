---
name: dimux-pane-control
description: Drive dimux server-panes (persistent background shells/processes) directly via the `dimux` CLI over Bash -- spawn a pane, type commands into it, read back what it printed, rename/list/kill it. Use when the user asks to run something in a "dimux pane", keep a long-running process alive across turns, inspect what's happening in an existing dimux pane, or otherwise control dimux from the command line rather than through its interactive TUI.
---

# dimux pane control

`dimux` is a tmux-style terminal multiplexer with a daemon/client architecture. Its `server` subcommand gives full scripted control over **server-panes** — persistent PTY-backed processes the daemon keeps running — with no need to open the TUI (`dimux attach`), create a workspace, or bind a client-pane. Every command below auto-starts the daemon on first use if it isn't already running.

This skill covers the `dimux server ...` surface only: spawn, list, read, send, rename, kill. That is everything needed to run a process in the background, feed it input, and inspect its output — full control per the pane's lifecycle.

## Core loop: spawn → send → read

```bash
# 1. Start a pane running a shell (or any command) in the background.
dimux server spawn build-shell
# -> spawned server-pane build-shell (3f9a1e2c-...-b7d1)

# 2. Type a command into it, as if you'd typed it and hit Enter.
dimux server send build-shell "npm run build" --enter

# 3. Give it a moment to produce output, then read the current screen.
sleep 2
dimux server read build-shell
```

- **Addressing**: every `server` subcommand's target accepts either the pane's **name** (what you passed to `spawn`) or its **UUID** — use the name, it's what a human (and you) can actually read back later. Names must be unique; `spawn` errors if the name is taken.
- **`send --enter`**: appends a newline after the text, matching a user typing the line and pressing Enter. Omit `--enter` to send raw keystrokes with no newline (e.g. to answer a `y/n` prompt char-by-char, or send a control sequence).
- **`read` is a one-shot snapshot** of whatever is on screen *right now* — it does not block or wait for the command to finish. There is no "wait for output" or "wait for idle" command yet. Poll it yourself:

```bash
dimux server send build-shell "long-running-command; echo DONE-MARKER-1" --enter
for i in $(seq 1 30); do
  sleep 1
  out=$(dimux server read build-shell)
  if echo "$out" | grep -qx 'DONE-MARKER-1'; then   # -x: exact line match
    break
  fi
done
echo "$out"
```

Don't rely on matching the shell prompt itself to detect "done" — the prompt varies by shell/config (a login shell may print `bash-3.2$`, `❯`, or something else entirely you can't predict up front) and a fixed sleep duration is a guess that's either too short or wastes time. Appending `; echo <unique-marker>` to the command you send is the reliable way to know it finished, for any shell.

**Use `grep -qx`, not a plain substring `grep -q`, for the marker.** The pane's screen shows the typed command *and* its output — the command you sent (`... echo DONE-MARKER-1`) is echoed back onto the screen the moment you send it, before it's actually run. A substring match on `DONE-MARKER-1` matches that echoed command line immediately, giving a false "done" before the command has even started. Matching the *exact* line (`-x`) only matches the marker's actual printed output, not the command text that produced it.

## Full command reference

| Command | Effect |
|---|---|
| `dimux server spawn <name> [--cmd "<command>"]` | Start a new server-pane named `<name>`, running `<command>` (default: the user's `$SHELL` as a login shell). Prints the new pane's id. |
| `dimux server send <target> <text> [--enter]` | Write `<text>` into the pane's stdin, as if typed. `--enter` adds a trailing newline. |
| `dimux server read <target>` | Print the pane's current visible screen as plain text (styling/color stripped). Trailing blank rows are trimmed. |
| `dimux server ls` | List every server-pane: `id  name  status  rowsxcols  process  cwd`, tab-separated, one per line. `status` is `running` or `dead`. |
| `dimux server rename <target> <new-name>` | Rename a pane. Errors if `<new-name>` is already taken. |
| `dimux server kill <target>` | Terminate the pane's process and remove it from the daemon. Do this when you're done with a pane you spawned — nothing else cleans it up. |

`<target>` is always name-or-id, consistently, across `send`/`read`/`rename`/`kill`.

## Practical notes

- **Always give panes a name at spawn time.** An unnamed pane only has a UUID to address it by, which you'd have to scrape from `spawn`'s output or `ls` and carry around — a name is one less thing to track.
- **Clean up panes you spawn** with `dimux server kill <name>` once you're done, unless the user asked for the pane to be left running (e.g. a dev server they want to keep using after you're done). Check `dimux server ls` if you're unsure what's still around from earlier in the session.
- **Check before spawning a duplicate.** If you might already have a pane for this purpose (e.g. from earlier in the same task), `dimux server ls` first rather than spawning a second one blindly — `spawn` will error anyway if you reuse a name that's still alive.
- **`read` shows exactly what's on screen**, including the shell prompt and command echo — parse it like a terminal transcript, not like a clean command-output capture. A pane running an interactive REPL, an editor, or anything else that redraws the screen will read back as that screen's visible contents, not a linear log.
- **This is not the TUI.** `dimux attach` opens an interactive multi-pane terminal UI for a human; this skill's commands work headlessly against the same daemon and don't require (or interact with) any attached TUI session, workspace, or client-pane. A pane spawned here shows up in `dimux server ls` and can also be *bound into* a TUI workspace later (via `dimux client spawn --bind <target>` or the attach menu) if the user wants to watch it interactively — but that's outside this skill's scope.
