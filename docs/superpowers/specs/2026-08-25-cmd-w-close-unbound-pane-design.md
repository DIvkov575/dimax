# cmd-w closes an unbound pane — design

Date: 2026-08-25
Status: approved by owner (DIvkov575)

## Summary

`cmd-w` / `Ctrl-Space w` on a focused client-pane with no bound
server-pane closes that pane (removes the leaf from the layout) instead
of the current deliberate no-op.

## Background

`close_tab` (src/tui/mod.rs) currently returns early when the focused
pane's `active_bound()` is `None`, documented as protecting the user
from "losing a grid slot you never asked to close". The owner has
reversed that call: after detaching a session (`cmd-shift-z`) the
left-behind placeholder pane is clutter, and `cmd-w` should clean it
up.

`active_bound()` returns `None` only when the leaf's `tabs` list is
empty (every leaf with a non-empty `tabs` has a valid `active_tab`
index), so "unbound" is unambiguous: it means the empty-tabs
placeholder.

## Behavior after the change

- Bound tab + `cmd-w`: unchanged — `ClientCloseTab` + `ServerKill`
  (PR #29 semantics).
- Multi-tab pane + `cmd-w`: unchanged — closes the active tab only,
  never the layout slot.
- Unbound pane + `cmd-w`: **new** — send `ClientCloseTab`; the daemon
  already closes an empty leaf in response
  (`client_close_tab_on_unbound_pane_closes_the_leaf`). No
  `ServerKill` (nothing to kill).
- Last pane + `cmd-w`: workspace tree becomes `None`; the TUI renders
  the existing "empty workspace — press cmd-d to spawn a pane"
  placeholder (src/tui/render.rs). dimax keeps running; it does not
  exit.
- Focus: handled by the existing `LayoutDelta` → `reconcile_focus`
  path, same machinery the single-tab bound case already uses.
- Portable-mode parity is automatic: the chord maps to the same
  `Action::CloseFocusedPane`.

## Implementation

TUI-only change; no daemon or protocol changes.

In `close_tab`: when `active_bound()` is `None`, send `ClientCloseTab`
(skipping the `ServerKill` that only makes sense with a bound id).
Rewrite the method's doc comment — it currently documents the no-op as
a deliberate decision this feature supersedes; keep the multi-tab
guard rationale, which still holds.

## Tests

TUI-level tests against a real daemon (the codebase's established
pattern):

1. Unbound pane + `cmd-w` → the pane is gone from the workspace tree,
   a sibling pane survives.
2. Unbound last pane + `cmd-w` → tree is `None`, the app reaches the
   empty-workspace state without erroring, and a subsequent spawn
   (cmd-d path) still works.

## Verification

Full local suite (macOS) + CI green on `dev`, then fast-forward
`main`/`stable` per the established branch flow.
