dimax
=====

A terminal multiplexer with persistent server sessions, detachable client
panes, multiple workspaces, tabs, mouse resizing, and a scriptable CLI.

Install
-------

Requirements:

- Rust toolchain with Cargo
- A Unix-like operating system
- Kitty only when using the optional Cmd-key integration

Install the binary from a checkout:

    cargo install --path .

No terminal configuration is modified by installing dimax. The first time
you run `dimax attach` (or bare `dimax`), a one-time setup wizard asks you
to pick a keybinding mode and offers to install the bundled Claude Code
skill -- see "First-run wizard" and "Claude Code skill" below. Nothing
about `install` itself touches your terminal config or `~/.claude`.

Package managers
----------------

Install directly from the Git repository with Cargo:

    cargo install --git https://github.com/DIvkov575/dimax --locked dimax

After a crates.io release, the equivalent command is:

    cargo install dimax --locked

For Homebrew, use the repository as a source-build tap:

    brew tap divkov575/dimax https://github.com/DIvkov575/dimax
    brew install --HEAD divkov575/dimax/dimax

Package managers install only the executable. Select terminal integration
explicitly afterward, so package installation never edits user configuration:

    dimax keys install --mode portable
    dimax keys install --mode both --reload

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

Choose one mode explicitly:

    dimax keys install --mode portable
    dimax keys install --mode kitty
    dimax keys install --mode both
    dimax keys install --mode both --reload

`portable` uses a Ctrl-Space prefix and does not modify terminal files.
`kitty` installs Cmd-key mappings in Kitty. `both` enables both input layers.
The selection is stored in `~/.config/dimax/keybindings.json`, or under
`$XDG_CONFIG_HOME/dimax` when that variable is set.

Installing `kitty` or `both` mode amends `~/.config/kitty/kitty.conf` (one
`include dimax.conf` line, plus the generated `dimax.conf` itself). On a
real terminal you're asked to confirm first, defaulting to yes; pass
`--yes`/`-y` to skip the prompt for scripted installs, or run in a
non-interactive context (piped stdin, CI) where it's assumed automatically.
Remove everything it added with `dimax keys uninstall`.

Inspect bindings without changing files:

    dimax keys list
    dimax keys print --mode portable
    dimax keys print --mode kitty
    dimax keys print --mode both

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
drive `dimax server ...` directly -- spawn a pane, send it commands, read
back what it printed, rename/list/kill it -- without opening the TUI.

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

    dimax server spawn <name>
    dimax server ls
    dimax server send <name> "command" --enter
    dimax server read <name>
    dimax server rename <name> <new-name>
    dimax server kill <name>

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
