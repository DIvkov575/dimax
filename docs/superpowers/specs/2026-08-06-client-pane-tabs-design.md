# Client-Pane Tabs — Design

## Goal

Let a single client-pane (leaf in a workspace's split tree) hold several server-pane bindings ("tabs"), showing exactly one at a time, cyclable with a keybind — without changing how splitting, closing, or resizing a leaf works today.

## Architecture

Today, `ClientPane` carries `bound: Option<ServerPaneId>` — a leaf displays at most one server-pane, or none (an "unbound placeholder"). This changes to:

```rust
pub struct ClientPane {
    pub id: ClientPaneId,
    pub name: Option<String>,
    pub tabs: Vec<ServerPaneId>,
    pub active_tab: usize,
}
```

`tabs.is_empty()` is the new unbound-placeholder state — reachable only by spawning a client-pane with no `--bind`, or by unbinding (clearing the active tab down to zero tabs when it was the only one). It is **never** reached by closing tabs: closing the last remaining tab closes the leaf itself, exactly like today's `ClientClose` behavior for a single-tab leaf. This means the codebase only ever has to reason about one "empty" case, not two.

`active_tab` indexes `tabs` and is always in `0..tabs.len()` whenever `tabs` is non-empty — every mutation that changes `tabs`' length (`ClientAddTab`, `ClientCloseTab`) is responsible for keeping `active_tab` valid afterward (clamping down when the tab it pointed at is removed).

Everywhere in the daemon that currently reads `leaf.bound` (PTY-sizing's `viewed_size`, `bound_server_pane`, broadcast fan-out in `subscribers_for_server_pane`/`recompute_workspace_pty_sizes`, `Input`/`ScrollClientPane` routing) instead reads `leaf.tabs.get(leaf.active_tab)`. This is a pure rename-and-reindex at each of those sites; none of their surrounding logic changes, because "which server-pane does this leaf currently display" is still a single optional id from every caller's point of view — just computed differently. Sizing for a tab that exists but isn't currently active is unaffected: it's exactly as if that server-pane weren't bound to this leaf at all, which is correct (some *other* tab is what's on screen; a background tab's server-pane keeps whatever size its other viewers, if any, already give it, or its last-known size if none).

## Wire Protocol

Three new requests, alongside the field rename:

- **`Request::ClientAddTab { workspace, pane, target }`** — resolves `target` (name-or-id, same as `ClientBind`), appends it to `tabs`, sets `active_tab` to its new index. Errors if `target` doesn't resolve, same as `ClientBind` today.
- **`Request::ClientCycleTab { workspace, pane, forward: bool }`** — advances (or retreats) `active_tab` by one, wrapping. A no-op (`Ack`, not an error) if `tabs.len() <= 1` — nothing to cycle to.
- **`Request::ClientCloseTab { workspace, pane }`** — removes the tab at `active_tab`. If it was the last one, this closes the whole leaf via the existing `client_close` path (broadcasting a `LayoutDelta` exactly like `ClientClose` does) rather than leaving a 0-tab leaf behind. Otherwise, `active_tab` clamps to the new last index if it pointed past the end, and a `LayoutDelta` still broadcasts (the leaf's tab list changed, which every viewer needs to know about for rendering).

`Request::ClientBind` keeps its current meaning and signature: it **replaces** the tab at `active_tab` (or, on an unbound placeholder, sets `tabs = [target]`, `active_tab = 0` — today's exact "first bind" behavior, unchanged). `Request::ClientUnbind` clears `active_tab`'s tab specifically: if it's the only tab, `tabs` becomes empty (today's unbind-to-placeholder behavior); if there are others, the active one is removed and `active_tab` clamps.

No change to `Response`/`Event` shapes beyond `ClientPane`'s own field rename — `WorkspaceInfo`/`SplitTree`/`LayoutDelta` all carry `ClientPane` by value already, so the new fields ride along automatically.

## CLI Surface

- `dimux client bind <addr> <target>` — unchanged meaning (replace the active tab).
- `dimux client add-tab <addr> <target>` — new; wraps `ClientAddTab`.
- `dimux client unbind <addr>` / `dimux client close <addr>` — unchanged meaning, now phrased against "the active tab" / "the whole leaf" respectively, per the field rename.
- `dimux client ls` — its one-line-per-pane output format changes from `id  name  bound-server-pane-or-dash` to `id  name  active-tab-or-dash  tab-count`, e.g. `<id>  editor  <server-pane-id>  2/3`. (`format_client_pane_line` is the one function to update; its existing tests get updated alongside.)

No CLI surface for cycling — cycling is an interactive, momentary action with no scripting use case that a picker-through-a-list can't already serve (`add-tab`/`bind` let a script set up whatever tab layout it wants; which one happens to be "active" when a human is looking at the TUI isn't something worth a CLI verb for).

## TUI

**New chords** (see `keys.rs`'s chord table for the encoding convention — each gets a fresh single-letter tag):

| Chord | Action | Tag |
|---|---|---|
| `cmd-t` | Open the attach menu in **add-tab mode** for the focused leaf | `t` |
| `cmd-]` | Cycle the focused leaf's active tab forward (wrapping) | `]` |
| `cmd-[` | Cycle the focused leaf's active tab backward (wrapping) | `[` |

`cmd-w` (already `Action::CloseFocusedPane`) changes meaning: instead of always closing the whole leaf, it now sends `ClientCloseTab` — which itself degrades to closing the leaf when there's only one tab, so single-tab usage is behaviorally identical to today. `cmd-shift-w` (kill the focused server-pane) is unaffected — it kills whatever server-pane the active tab points at, same as today, tabs notwithstanding.

`cmd-shift-z` (`DetachAndAttach`) changes scope subtly: "detach" now means "clear the active tab specifically" (via the same `ClientUnbind`-then-reopen-picker flow, unchanged), and the picker's eventual `ClientBind` call replaces just that one tab. Other tabs already on the same leaf are untouched throughout — the user never loses tabs 2 and 3 by re-picking tab 1's binding.

**Add-tab mode** is a new field on `AttachMenu`: `adding_tab: bool`, set at construction time by whichever chord opened the menu (`cmd-shift-z` → `false`, `cmd-t` → `true`) and read only at the one place the picker's selection gets committed. `confirm_attach_menu`'s eventual bind call branches on it between sending `ClientBind` (replace, `adding_tab: false`) and `ClientAddTab` (append, `adding_tab: true`). Everything else about the picker (grouping, collapsing, pin markers, the per-group spawn field, preview panel) is identical in both modes — only that one terminal action differs. The per-group spawn field's own Enter/Shift+Enter behavior (spawn+bind vs. spawn+send-unbound) is unaffected either way; when `adding_tab` is true, its "bind" branch calls `ClientAddTab` instead of `ClientBind`, same substitution.

**Rendering** (`render::draw_leaf`): the title bar's text becomes `"<active tab's name-or-id>"` (unchanged) with `" (N/M)"` appended only when `tabs.len() > 1` — a single-tab leaf's title bar is pixel-for-pixel identical to today's. No per-tab names are shown, per explicit direction — this is a position/count indicator, not a named tab strip.

## Non-Goals

- Independent resizing/dragging of tabs — a leaf's tabs all share the leaf's one rect; only splits (the existing mechanism) create independently-resizable regions.
- Persisting which tab was active across a daemon restart — tabs live in `SplitTree`, which (like every other piece of `State` except pinned directories) doesn't survive a restart today, and this doesn't change that.
- A CLI verb for cycling (see "CLI Surface" above).
- Renaming individual tabs independently of the server-pane's own name — a tab's displayed name is always its bound server-pane's name (or short id), exactly like today's single-binding leaves; `dimux server rename` already covers naming a server-pane.

## Testing

- `daemon::state`: unit tests for `client_add_tab` (appends + activates, errors on unknown target), `client_cycle_tab` (wraps forward/backward, no-op on a single tab), `client_close_tab` (removes active tab and clamps `active_tab`; closes the whole leaf when it was the last tab, broadcasting the same way `client_close` does), and updated versions of every existing `bound`-touching test (`client_bind_rebinds_and_rejects_unknown_targets`, `client_unbind_detaches_and_leaves_server_pane_running`, the PTY-sizing suite) against the new field shape.
- `daemon::mod`: integration tests over the wire for the three new requests, plus updated existing tests wherever a test currently constructs/asserts on `ClientPane { bound, .. }` literally.
- `cli`: updated `format_client_pane_line` tests for the new `active-tab/count` format; new tests for the `add-tab` subcommand's argument parsing.
- `tui::mod`: new chord-parsing tests (`cmd-t`/`cmd-]`/`cmd-[`), tests for add-tab-mode threading through the attach menu's confirm path, and cycle wrapping at the `App` level.
- `tui::render`: new tests for the `(N/M)` title-bar suffix appearing only when `tabs.len() > 1`, and its exact absence for a single tab (pixel-identical-to-today assertion via the existing `buffer_contains`/`buffer_row_containing` helpers).
