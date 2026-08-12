# Feature queue

Lightweight backlog of requested features not yet scoped/planned. Not a
spec — just enough context to pick up later. Promote to a real plan
(`docs/superpowers/plans/`) when someone's ready to build it.

## `cmd-t` should default to a fresh shell, not the selector menu

`cmd-t` (`Action::AddTab`) currently always opens the add-tab picker
(`App::open_add_tab_menu`, `src/tui/mod.rs:1903`) — the same
attach-menu-style selector used to pick an *existing* server-pane to
add as a tab. Requested: spawning a new tab via `cmd-t` should default
to just creating a brand new shell session immediately (no selector
step), matching how `cmd-d`/`cmd-shift-d` (split) already spawn a
fresh pane directly rather than prompting. The selector/picker
behavior (choosing an existing pane to add as a tab) should still be
reachable somehow, just not be the default `cmd-t` action.

## Keybinding to spawn a plain shell in a given directory, no selector

Requested: a dedicated keybinding that spawns a plain default shell in
a chosen directory directly, without going through the attach
menu/picker flow at all. Needs a way to specify/pick the target
directory (a lightweight inline prompt? reuse the pinned-directories
list? cwd of the currently focused pane as a starting point?) — not
yet scoped.
