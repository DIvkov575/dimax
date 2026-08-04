# Attach menu: cwd grouping, delete, and rename — design

## Summary

Extends the existing `cmd-shift-z` attach menu (`tui::mod::AttachMenu`,
rendered by `tui::render::draw_attach_menu`) with three additions:

1. **Group server-pane rows by working directory.** Rows sharing a `cwd`
   are listed together under a non-selectable header line naming that
   `cwd`; panes with no resolvable `cwd` fall into a synthetic `"Unknown"`
   group, always sorted last.
2. **Delete** (`x`): kill the selected server-pane from inside the menu,
   with a two-keypress arm/confirm step (killing a process is hard to
   undo).
3. **Rename** (`r`): rename the selected server-pane from inside the
   menu via an inline, fully-editable text field (cursor movement,
   Home/End, insert/delete at cursor).

No new keybind is introduced. The menu's only entry point remains
`cmd-shift-z` (detach the focused client-pane, then open this menu to
pick — or now manage — a server-pane), exactly as today.

## Non-goals

- No standalone keybind to open the menu without going through
  `cmd-shift-z`'s detach-first flow — out of scope per explicit
  direction; this spec only changes what the menu can do once open, not
  how it's opened.
- No "clear name back to id fallback" affordance for rename — submitting
  an empty name is a no-op.
- No undo for delete beyond the arm/confirm step itself.
- No change to `dimux server ls`'s CLI output — grouping/delete/rename
  from this spec are TUI-menu-only. (`ServerKill`/`ServerRename` already
  exist as CLI commands and daemon requests; this spec wires the
  existing menu to call them, not adds new daemon behavior.)

## Data model

`AttachMenu` (`tui/mod.rs`) gains one field:

```rust
struct AttachMenu {
    servers: Vec<ServerPaneInfo>,
    selected: usize,
    /// `Some(index into servers)` while that row's delete is armed
    /// (first `x` pressed, awaiting a confirming `x`/Enter or a
    /// cancelling any-other-key). Distinct from `rename` below — the
    /// two modes are mutually exclusive; opening one clears the other.
    pending_delete: Option<usize>,
    /// `Some` while the inline rename field is focused for the row at
    /// `.index`. `text`/`cursor` are the field's live edit buffer and
    /// cursor position (byte offset into `text`, always on a char
    /// boundary); `error` holds the daemon's last rejection message
    /// (e.g. a name collision) to render under the field, cleared on
    /// the next keystroke.
    rename: Option<RenameState>,
}

struct RenameState {
    index: usize,
    text: String,
    cursor: usize,
    error: Option<String>,
}
```

`selected` keeps its existing meaning and range (`0..=servers.len()`,
where `servers.len()` is the trailing "spawn new" row) — **grouping is a
rendering concern only**. `servers` is sorted once, right after every
fetch (menu open, post-delete refresh, post-rename refresh) into
cwd-bucket order:

- Group key: `server.foreground.as_ref().and_then(|f| f.cwd.as_deref())`,
  falling back to a synthetic `"Unknown"` group when `None` (a `Dead`
  pane, or a live one whose lookup failed).
- Groups sort ascending by key, except `"Unknown"` is always forced last
  regardless of string ordering.
- Within a group, panes keep their existing relative order (whatever
  `ServerPaneList`/`ServerList` returned — no additional secondary sort).

Because the sort happens once on fetch and index order then matches
render order top-to-bottom, `move_attach_menu_selection`'s existing
wrapping-modulo nav logic needs no changes — moving "down" from the last
row of one group already lands on the first row of the next group's
first pane, header rows simply aren't part of `servers` so they're never
a nav target.

## Rendering (`tui::render::draw_attach_menu`)

Walks the sorted `servers`, emitting a header line (styled distinctly,
e.g. dim/bold, non-selectable) whenever the group key changes from the
previous row, with each pane's row indented two spaces underneath its
header. The trailing "spawn new" row stays last, outside any group,
unindented (unchanged from today).

**Column change:** since `cwd` is now shown once per group instead of
once per row, the per-row `cwd` column is dropped. Rows become `name |
process | id | status` (previously `name | cwd | process | id |
status`). This is a deliberate declutter + width reclaim for the rename
field (below), not an accidental side effect — call it out in the PR
description.

**Delete-armed row:** rendered with a distinct style (e.g. reversed +
red/warning color) and a trailing hint like `[x/Enter: confirm]`.

**Rename-active row:** the row's name field is replaced by a bordered
editable text span showing `rename.text` with a visible cursor at
`rename.cursor`; if `rename.error` is `Some`, render it as a second line
directly under that row before continuing the list.

## Input handling (`tui::mod::App`)

`handle_attach_menu_input`'s dispatch grows two new branches, and its
existing ones gain "clear other pending state first" semantics:

```rust
enum AttachMenuAction {
    Up,
    Down,
    Confirm,       // existing: bind selected / spawn new
    Cancel,        // existing: Esc, only when not in rename/pending-delete
    Delete,        // new: 'x'
    StartRename,   // new: 'r'
    Ignore,
}
```

Routing in `handle_attach_menu_input`, in priority order:

1. **If `rename.is_some()`:** bytes go to a dedicated rename-editing
   parser (below), not the table above at all — the menu is modal
   within a modal while renaming.
2. **Else if `pending_delete.is_some()`:**
   - `x` or `\r`/`\n` → send `Request::ServerKill { target:
     servers[pending_delete].id }`, on `Ack` re-fetch `ServerList`
     (re-sort into groups), clear `pending_delete`, clamp `selected`
     into the new (shorter) list — if the deleted row was the last
     selectable row, move to the new last row, otherwise `selected`
     stays numerically in place (which now names whatever slid up into
     that slot).
   - any other byte → clear `pending_delete`, then fall through to
     normal `AttachMenuAction` handling for that same byte (a cancelling
     keystroke isn't swallowed — e.g. `j` both cancels the pending
     delete *and* moves the selection down, in one keypress).
3. **Else (normal browsing):** existing `parse_attach_menu_input` table,
   extended with `x` → `Delete` and `r` → `StartRename`.
   - `Delete` on the "spawn new" row (`selected == servers.len()`) is a
     no-op.
   - `StartRename` on the "spawn new" row is a no-op.
   - `Delete` on a real row sets `pending_delete = Some(selected)`.
   - `StartRename` on a real row sets `rename = Some(RenameState {
     index: selected, text: servers[selected].name.clone()
     .unwrap_or_default(), cursor: text.len(), error: None })` — cursor
     starts at the end of the pre-filled text.

**Rename-editing parser** (new, byte-level, mirrors the minimal style of
`parse_attach_menu_input`/`keys::parse` rather than pulling in a text-input
crate):

| Input | Effect |
|---|---|
| `\r` / `\n` | If `text` is empty: no-op (menu stays in rename mode). Otherwise send `Request::ServerRename { target: servers[index].id, new_name: text }`; on `Ack`, re-fetch `ServerList`, clear `rename`. On `Response::Error`, set `rename.error = Some(message)`, stay in rename mode. |
| `\x1b` (bare Esc) | Clear `rename` (cancel), no request sent. |
| `\x7f` / `\x08` (Backspace) | Remove the char immediately before `cursor`, move `cursor` back one char; no-op at `cursor == 0`. |
| `\x1b[3~` (Delete) | Remove the char immediately at/after `cursor`; no-op at end of text. |
| `\x1b[D` (Left) | Move `cursor` back one char; no-op at `cursor == 0`. |
| `\x1b[C` (Right) | Move `cursor` forward one char; no-op at end of text. |
| `\x1b[H` / `\x1b[1~` (Home) | `cursor = 0`. |
| `\x1b[F` / `\x1b[4~` (End) | `cursor = text.len()`. |
| any other printable UTF-8 byte sequence | Insert at `cursor`, advance `cursor` past the inserted text. |
| any other unrecognized escape sequence | Ignored (defense in depth, same rationale as `mouse::parse`'s "Ignored" case — don't let a stray sequence get inserted as literal garbage text). |

Every branch that mutates `text`/`cursor` also clears `rename.error` (a
fresh edit dismisses the last error rather than leaving stale text stuck
on screen).

## Testing

**Unit (`tui::render` / `tui::mod` test modules, following existing
patterns):**

- Grouping/sort helper: panes with matching `cwd` end up contiguous;
  distinct `cwd`s sort ascending; `None`-cwd panes land in a last-sorted
  `"Unknown"` bucket regardless of how many other groups exist.
- `draw_attach_menu` smoke tests (extend the existing three): header
  rows appear once per group with correct indentation of member rows; a
  delete-armed row renders its confirm hint; a rename-active row renders
  the live text + cursor; none of these panic on an empty `servers` list
  or a single-pane list (no headers needed run).
- Pending-delete state machine: `x` then `x` clears the row from
  `servers` and clamps selection; `x` then `j` cancels the pending state
  *and* moves selection down (both effects from one keystroke); `x` on
  the spawn-new row is a no-op.
- Rename-editing byte parser, isolated from any daemon round-trip:
  insert-at-cursor, backspace/delete-at-cursor, Left/Right/Home/End all
  produce the expected `(text, cursor)`; empty-text Enter is a no-op;
  bare Esc clears rename state without mutating `servers`.

**Integration (`daemon::tests` pattern — real daemon, real socket,
`TestConn`):** confirm `Request::ServerKill`/`Request::ServerRename`
issued via this new menu path behave identically to the existing
CLI-driven request tests already covering those two request kinds (no
new daemon-side behavior is introduced by this spec, so these are
regression coverage for the wiring, not new daemon logic).

## Open questions / risks

None outstanding — every mechanic above was resolved during the design
dialogue (grouping style, menu scope, delete confirmation, rename
editing capability, no-cwd bucket, post-delete menu behavior all
explicitly decided). One deliberate product call worth flagging in the
PR description: dropping the per-row `cwd` column is scoped entirely to
the TUI attach menu's `draw_attach_menu` — `dimux server ls`'s CLI
output is untouched (see Non-goals).
