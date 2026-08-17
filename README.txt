dimax
=====

A terminal multiplexer with persistent server sessions, detachable client
panes, multiple workspaces, tabs, mouse resizing, and a scriptable CLI.

Recommended terminal: Kitty
---------------------------

dimax runs in any Unix terminal, but Kitty is the recommended host. Kitty
mode maps every dimax action to a real single-press Cmd chord (Cmd+D to
split, Cmd+T to add a tab, Cmd+Shift+Z for the attach menu, and so on) --
the same shortcuts a native macOS app would use. The portable prefix
(Ctrl-Space, then a key) still works everywhere else and is fully
supported; it's just noticeably less ergonomic for actions you take
dozens of times a day.

Install
-------

Requirements:

- Rust toolchain with Cargo (any recent stable)
- A Unix-like operating system (macOS or Linux)
- Kitty, if you want the recommended Cmd-key integration

Recommended: install from the Git repository via Cargo, then wire up the
recommended Kitty bindings:

    cargo install --git https://github.com/DIvkov575/dimax --locked dimax
    dimax keys install --mode both --reload

`--mode both` enables both the Kitty Cmd-key chords and the portable
Ctrl-Space prefix; `--reload` tells the running Kitty instance to pick
up the new config immediately (requires Kitty remote control to be
available, which is Kitty's default when launched from macOS). The
`keys install` step amends `~/.config/kitty/kitty.conf` -- see
"Keybinding modes" below for exactly what it writes and how to remove
it. Pass `--yes`/`-y` to skip the confirmation prompt for a scripted
install.

Not using Kitty (or not sure yet)? Skip the second command -- `dimax
attach`'s one-time first-run wizard (see "First-run wizard" below) will
walk you through picking a mode interactively, defaulting to portable
so it never touches a config file you didn't opt into.

Other install methods
---------------------

From a local checkout (working on dimax itself, or wanting a specific
revision):

    cargo install --path .

After a crates.io release, the equivalent of the recommended `--git`
install is:

    cargo install dimax --locked

For Homebrew, use the repository as a source-build tap:

    brew tap divkov575/dimax https://github.com/DIvkov575/dimax
    brew install --HEAD divkov575/dimax/dimax

All install methods above only place the binary; they never touch your
terminal config or `~/.claude` on their own. Run `dimax keys install
--mode <mode>` yourself afterward, or let the first-run wizard do it
for you the first time you run `dimax attach`.

First-run wizard
----------------

The first time you run `dimax attach` (or bare `dimax`) after installing,
a one-time wizard appears before anything else loads:

1. Pick a keybinding mode -- portable, kitty, or both (see "Keybinding
   modes" below for what each means). Arrow keys or j/k to move, Enter to
   confirm.
2. Confirm or decline installing the bundled Claude Code skill (see
   "Claude Code skill" below). y/n to choose, Enter to confirm.

Esc at either step skips the rest of the wizard and applies whatever was
selected so far (portable keybindings and skill installation are both
on by default, so pressing Esc immediately is equivalent to accepting
both defaults). The wizard runs at most once: its choice is recorded in
`~/.config/dimax/keybindings.json` (or under `$XDG_CONFIG_HOME/dimax`),
so reattaching never shows it again. Every choice it makes can also be
set later by hand -- see "Keybinding modes" and "Claude Code skill".

Keybinding modes
----------------

Four modes; the recommendation is `both` on Kitty:

    dimax keys install --mode both --reload   # recommended on Kitty
    dimax keys install --mode kitty
    dimax keys install --mode portable        # non-Kitty terminals
    dimax keys install --mode tmux            # tmux-compatible chords

`both` enables both input layers -- Kitty's Cmd-key chords for the main
day-to-day actions, and the portable Ctrl-Space prefix as a fallback
that keeps working in any terminal (e.g. when you SSH into a remote
box or open a non-Kitty pane). `kitty` alone drops the fallback and is
mostly useful for a scripted install that will never see anything but
Kitty. `portable` never modifies any terminal files and is the right
choice when you can't (or don't want to) run Kitty. `tmux` swaps the
Ctrl-Space prefix for tmux's own `Ctrl-B`, then maps tmux's standard
chords (`%` to split side-by-side, `"` to split stacked, `c` for a new
tab, `d` to detach/quit, `s` for the attach menu, `1`-`9` to switch
workspaces, `h`/`j`/`k`/`l` for focus) onto the dimax actions -- pick
this if you have tmux muscle memory and want to keep it. It's
standalone: Kitty chords and the Ctrl-Space prefix are both off in this
mode.

The selection is stored in `~/.config/dimax/keybindings.json`, or under
`$XDG_CONFIG_HOME/dimax` when that variable is set.

Installing `kitty` or `both` mode amends `~/.config/kitty/kitty.conf` (one
`include dimax.conf` line, plus the generated `dimax.conf` itself). On a
real terminal you're asked to confirm first, defaulting to yes; pass
`--yes`/`-y` to skip the prompt for scripted installs, or run in a
non-interactive context (piped stdin, CI) where it's assumed automatically.
Remove everything it added with `dimax keys uninstall`. `tmux` and
`portable` modes never touch any terminal config file.

Inspect bindings without changing files:

    dimax keys list
    dimax keys print --mode portable
    dimax keys print --mode kitty
    dimax keys print --mode both
    dimax keys print --mode tmux

Add aliases for an existing action:

    dimax keys bind focus-left --portable x
    dimax keys bind split-vertical --kitty cmd+enter
    dimax keys bind session-1 --portable g1 --kitty cmd+alt+a

Portable aliases are one to three printable ASCII characters after the
Ctrl-Space prefix. Remove aliases or reset all custom aliases:

    dimax keys unbind --portable x
    dimax keys unbind --kitty cmd+enter
    dimax keys reset

`dimax keys list` prints valid action names. Conflicting aliases are rejected
instead of silently replacing a default action.

Portable bindings
-----------------

Press Ctrl-Space, release it, then press the listed key:

    1..9    switch to workspace 1..9
    s 1..9  bind the focused pane to existing session 1..9
    d       split vertically
    D       split horizontally
    w       close the focused tab
    W       kill the focused server session
    Z       detach and open the session picker
    h/j/k/l focus left/down/up/right
    t       add a tab
    ]/[     next/previous tab
    q       quit dimax (return to the shell)

Press Ctrl-Space twice to send a literal Ctrl-Space to the focused process.
Session numbers use the same stable, name-sorted order shown by
`dimax server ls`. A missing session number is a no-op and never creates a
session. Workspace numbers retain their existing behavior and may create or
switch to the corresponding workspace.

Tmux bindings
-------------

Only active in `--mode tmux`. Press Ctrl-B, release it, then press the
listed key (tmux's default prefix and chord vocabulary, mapped to the
semantically-closest dimax action):

    1..9    switch to workspace 1..9
    %       split side-by-side (tmux split-window -h)
    "       split stacked (tmux split-window -v)
    c       add a tab (tmux new-window)
    x       close the focused tab (tmux kill-pane)
    &       kill the focused server session (tmux kill-window)
    s       detach and open the session picker (tmux choose-tree)
    d       quit dimax (tmux detach-client -- returns to the shell)
    h/j/k/l focus left/down/up/right
    n / p   next / previous tab

Press Ctrl-B twice to send a literal Ctrl-B to the focused process, same
convention as tmux's `send-prefix`. Tmux mode is standalone: Kitty
Cmd-chords and the portable Ctrl-Space prefix are both off in this
mode. There is no tmux-mode analog of `session-1..9` (tmux itself has
no per-session numeric-jump concept -- use `s` for the picker).

Mouse selection
---------------

Drag across text inside a pane to highlight it. Releasing the left mouse
button automatically copies the highlighted text to the system clipboard.
Selections stay within their originating pane and remain highlighted until
the next click, keyboard action, scroll, or incompatible layout change.

Clipboard writes use OSC 52, so this does not require Kitty configuration or
platform-specific clipboard commands. The containing terminal must permit
OSC 52 clipboard writes; when it does not, highlighting still works but the
system clipboard is unchanged. Divider drags continue to resize panes.

Kitty integration
-----------------

Kitty mode keeps the Cmd bindings and adds Cmd+Alt+1..9 for jumping to
existing sessions, plus Cmd+Shift+Q to quit dimax -- deliberately not
Cmd+Q, which macOS/Kitty already use to quit the whole terminal.
Installation:

1. Generates `dimax.conf` in Kitty's config directory.
2. Adds one exact `include dimax.conf` line to `kitty.conf`.
3. Creates `kitty.conf` when it does not exist.
4. Saves the original as `kitty.conf.dimax.bak` before the first edit.
5. Refuses to overwrite a `dimax.conf` that is not marked as dimax-managed.

Repeated installation is idempotent. Restart Kitty or reload its config after
installing. `--reload` runs `kitty @ load-config` and therefore requires Kitty
remote control to be available.

Remove only files and lines managed by dimax:

    dimax keys uninstall

This removes the generated `dimax.conf`, removes the exact include line, and
leaves the backup and all other Kitty settings untouched. Portable bindings
remain active.

Claude Code skill
-----------------

dimax ships a Claude Code skill (`dimax-pane-control`) that lets Claude
drive the entire `dimax` CLI directly -- spawn a server-pane, send it
commands, read back what it printed, rename/list/kill it; create and
manage client-panes/workspaces; pin directories; configure keybindings;
hot-reload the daemon -- all without opening the TUI.

The first-run wizard offers to install it automatically. Install or
reinstall it explicitly at any time with:

    dimax skills install

This writes `~/.claude/skills/dimax-pane-control/SKILL.md`, overwriting
any previous copy -- the skill's content ships inside the `dimax` binary
itself, so this works the same from a `cargo install`/Homebrew install as
from a source checkout. It is a global install: once written, the skill
is available to Claude Code in every project, not just this one. There is
no separate uninstall command; remove the directory by hand if needed:

    rm -rf ~/.claude/skills/dimax-pane-control

Usage
-----

Run `dimax` or `dimax attach` to open the TUI. `Ctrl-Q` always exits it and
returns to the shell, in every keybinding mode -- including before any mode
has been chosen. `q` (portable) / `Cmd+Shift+Q` (kitty) do the same once a
mode is configured; see "Portable bindings" and "Kitty integration" above.

CLI session control includes:

    dimax server spawn <name> [--cwd <dir>]
    dimax server ls
    dimax server send <name> "command" --enter
    dimax server read <name>
    dimax server rename <name> <new-name>
    dimax server kill <name>

`dimax server ls` also prints a trailing `kind` column: `claude`/`codex`/
`opencode`/`omp`/`herdr` when the pane's foreground process is a
recognized AI-coding CLI tool, `-` otherwise. This is a stable tag (see
`daemon::state`/`term::session_name` in the source), not something a
caller needs to re-derive by pattern-matching the `process` column
itself. The same tag shows up as a `[claude]`/`[codex]`/etc. marker in
the attach menu's row list, so a recognized session stands out at a
glance there too.

`dimax server ls`'s final column is `attached`, answering "where is
this server-pane bound right now?" in the form `<workspace>/<client-
pane-short-id>`. A trailing `+` marks that this pane is a background
tab on that client-pane (bound but not the currently-displayed one);
commas separate multiple bindings; a lone `-` means unattached (still
running, but no client-pane is showing it). The attach menu's row list
shows the same information in its own `attached` column, so the "which
of these panes am I actually looking at right now?" question is
answerable at a glance in the picker too, not just via the CLI.

The attach menu (`cmd-shift-z` / `Ctrl-Space Z`) defaults to an
agents-only view: only server-panes tagged with a recognized `kind` are
listed. Press `f` inside the menu to flip between the agents-only
default and the full unfiltered list (plain shells, editors, and every
other non-agent pane). Server-panes are never scoped to a workspace --
every pane spawned anywhere shows up in every workspace's attach menu,
the same list `dimax server ls` sees.

Client-panes and workspaces (the TUI's grid) are also fully scriptable:

    dimax client spawn <workspace> [--split <pane-uuid>] [--dir h|v] [--bind <target>]
    dimax client ls [workspace]
    dimax client bind <addr> <target>
    dimax client unbind <addr>
    dimax client add-tab <addr> <target>
    dimax client cycle-tab <addr> [--backward]
    dimax client close-tab <addr>
    dimax client rename <addr> <new-name>
    dimax client close <addr>

`<addr>` is `<workspace>/<pane-id>`, as printed by `client spawn`/`ls`.

Directory pinning
------------------

The attach menu groups server-panes by working directory; a pinned
directory always sorts first. `p` pins/unpins the directory of whichever
row is currently selected -- a group header if the menu is in its
grouped view, or a server-pane row directly if `g` has toggled grouping
off, so pinning works the same way in either view. Pinning is
scriptable, not just reachable via the attach menu's `p` key:

    dimax pin add <dir>
    dimax pin remove <dir>
    dimax pin list

`add`/`remove` are idempotent (pinning an already-pinned directory, or
unpinning one that isn't pinned, is a no-op that still succeeds) --
unlike the attach menu's `p` key, which always just flips the current
state.

Hot reload
----------

    dimax daemon reload

Upgrades the running daemon onto whatever binary is currently installed
(after a `cargo install`/rebuild), without killing any server-pane -- a
plain restart tears down every PTY the old process owned, since a shell's
controlling terminal dies with the process holding its master fd. Instead
the daemon re-executes its own process image in place (same pid), carrying
every server-pane's PTY and every attached client's connection to the
listening socket across the transition; the shells inside never notice.

Already-attached `dimax attach` clients see their individual connection
drop for a moment and silently reconnect -- the same client-side retry
that recovers from a daemon crash, just faster in practice, since the new
process image is usually listening again within milliseconds.

`Ack` from this command means the daemon *attempted* the reload, not that
it necessarily succeeded -- a successful re-exec never returns, so there's
no way to confirm success back over the same connection. Run `dimax server
ls` afterward if you want to check.

Recovering from a crash or power loss
--------------------------------------

A hot reload survives on purpose -- it never actually stops the daemon
process. An unclean death (a crash, `SIGKILL`, or the machine losing
power) is different: every PTY and every process under it dies for
real, along with anything about them the daemon held only in memory.
There is no way to get that back -- no software can run code after
the machine has actually lost power.

What dimax does instead: every ~30s while the daemon runs, it saves a
snapshot of every server-pane whose foreground process is a recognized
AI-coding CLI tool (see the `kind` column above) -- just enough to
re-launch the same tool in the same directory, not the pane's actual
prior screen contents or conversation state, which are genuinely gone.
Plain shells and editors aren't snapshotted; there's no "resume"
concept worth re-launching for those, and restoring every idle shell
that ever existed would be noise, not signal.

If the next daemon start (after such a death) finds this snapshot, it
re-spawns each described session as a fresh, unbound ("orphan") pane,
picked up the same way any orphan pane is via the attach menu or
`dimax server ls`. Most recognized tools have their own session-resume
mechanism (e.g. `claude --continue`) that starting back up in the same
directory puts back within reach.

A *clean* shutdown (a normal `kill`/`Ctrl-C` to the daemon) deletes
this snapshot on its way out, precisely so the next start doesn't
mistake an intentional stop for a crash and resurrect sessions nobody
wanted back.

Workspace/client-pane layout (splits, tabs, which pane was bound
where) is not part of this snapshot -- recreating that automatically
would need to remap ids a fresh restart can't reuse, and unlike a
session, a layout is trivial to rebuild by hand in a few keystrokes.
