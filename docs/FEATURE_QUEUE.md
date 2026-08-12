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

## Attach-menu / server-pane selector polish

Several small requests against the attach menu's row list
(`AttachMenuRow`, `src/tui/mod.rs:2150`; `visible_attach_menu_rows`,
`src/tui/mod.rs:2185`) and its rendering (`draw_attach_menu`,
`src/tui/render.rs:495`):

- More vertical padding between directory groups (each group starts
  with a `GroupHeader` row) so groups read as visually distinct.
- Two navigation modes: one where arrow keys move between
  `GroupHeader` rows (jump group-to-group), another where they move
  between the actual server-pane rows within a group — today there's
  only one flat up/down traversal across every row.
- Make the preview panel (`PREVIEW_PANEL_HEIGHT`, `src/tui/render.rs:472`,
  currently 10 rows / 8 interior) a couple of lines taller.

## Split-tree render polish

- A small margin between adjacent panes in a split (currently panes
  are drawn edge-to-edge; `draw_tree`, `src/tui/render.rs:103`).
- Make the focused pane's border more visually distinct — some
  highlighting already exists (`is_focused`/`border_style`,
  `src/tui/render.rs:340-341`), but it's reportedly not prominent
  enough.

## Unbind and close-with-server-pane commands

Two related but distinct pane-management actions requested:

- An "unbind" command: detach a client-pane's tab from its
  server-pane without closing either (the server-pane keeps running,
  unbound, same end state `server_kill`'s callers already leave
  behind — see design doc "Error handling").
- A "close pane" command that, by default, also kills the associated
  server-pane (today `ClientClose`/`ClientCloseTab` only ever remove
  the client-side tab/leaf; the bound server-pane is deliberately left
  running — see their doc comments in `src/protocol.rs`). Needs
  deciding whether this replaces the current close behavior outright
  or is a separate/modifier-key action alongside it.

## Session name at the top of each pane

Requested: show the session/pane name as a header/title at the top of
each rendered pane (leaf), not just wherever it's currently surfaced
(short id badge, attach menu row, etc.) — not yet scoped against
`draw_leaf` (`src/tui/render.rs:328`).
