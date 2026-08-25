# cmd-w Closes an Unbound Pane — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `cmd-w` / `Ctrl-Space w` on a focused client-pane with no bound server-pane closes that pane (removes the leaf) instead of being a no-op.

**Architecture:** One TUI-only change in `App::close_tab` (src/tui/mod.rs): when the focused pane has no active binding, still send `ClientCloseTab` (the daemon already closes an empty leaf in response) and skip the `ServerKill`. No daemon or protocol changes. The existing `LayoutDelta` → `reconcile_focus` machinery handles focus after the leaf closes.

**Tech Stack:** Rust (edition 2024), tokio, existing dimax TUI/daemon test harness (`app_against_real_daemon`, real in-process daemon over a Unix socket).

**Spec:** `docs/superpowers/specs/2026-08-25-cmd-w-close-unbound-pane-design.md`

## Global Constraints

- TUI-only change; no daemon (`src/daemon/`), protocol (`src/protocol.rs`), or CLI changes.
- Bound-tab and multi-tab `cmd-w` semantics stay exactly as they are (tests `close_tab_kills_the_bound_server_pane` and `close_tab_on_a_multi_tab_pane_drops_only_the_active_tab` must remain green and unmodified).
- Last pane + `cmd-w` → workspace `tree == None`, existing "empty workspace" render path; dimax does NOT exit.
- Every test spawn passes an explicit cwd (`Some(test_cwd())`) — never `None` (unset cwd makes portable-pty chdir `$HOME`, which races other tests' faked `HOME`s into ENOENT).
- `cargo fmt --check` clean; `cargo clippy --locked --all-targets` introduces no new warnings (it is advisory in CI but must not regress).
- Conventional commit messages, matching repo history style (`feat:`, `test:`, `docs:`).
- Branch flow: work on `dev`; after CI is green fast-forward `main` and `stable` (`git push origin dev:main && git push origin dev:stable`).

---

### Task 1: Flip `cmd-w` on an unbound pane from no-op to close-the-pane

**Files:**
- Modify: `src/tui/mod.rs` — `App::close_tab` (currently ~line 942) and its doc comment (~lines 919-941)
- Test: `src/tui/mod.rs` — rewrite `close_tab_on_an_already_unbound_pane_is_a_no_op` (~line 4796), same file

**Interfaces:**
- Consumes: `Request::ClientCloseTab { workspace: String, pane: ClientPaneId }`, `Request::ServerKill { target: String }`, `Response::ClientPaneList { panes, .. }` (panes items have `pub id: ClientPaneId`), existing test helpers `app_against_real_daemon()` and `test_cwd()`.
- Produces: unchanged public surface — `close_tab(&mut self, write_half: &mut OwnedWriteHalf, reader: &mut FrameReader) -> anyhow::Result<()>`. Behavior change only.

- [ ] **Step 1: Rewrite the no-op test into the new-behavior test (failing)**

In `src/tui/mod.rs`, replace the entire test `close_tab_on_an_already_unbound_pane_is_a_no_op` (doc comment + `#[tokio::test]` fn, ~lines 4792-4848) with:

```rust
    /// `cmd-w` on an unbound pane (no tabs at all -- e.g. right after
    /// detaching via `cmd-shift-z`) must close the pane itself: the
    /// empty placeholder is clutter to clean up, not a grid slot to
    /// preserve. Same daemon path as a scripted `ClientCloseTab` on an
    /// empty leaf (see `state::client_close_tab`), minus the
    /// `ServerKill` -- nothing is bound, so nothing is killed.
    #[tokio::test]
    async fn close_tab_on_an_unbound_pane_closes_the_pane() {
        let (mut app, mut write_half, mut reader) = app_against_real_daemon().await;

        let Response::ClientPaneCreated { pane, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ClientSpawn {
                    workspace: "1".to_string(),
                    split_of: None,
                    dir: None,
                    bind: None,
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ClientPaneCreated");
        };
        app.focused = Some(pane);
        if let Response::Snapshot { workspace, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::Subscribe {
                    workspace: "1".to_string(),
                },
            )
            .await
            .unwrap()
        {
            app.workspace = workspace;
        }

        app.close_tab(&mut write_half, &mut reader).await.unwrap();

        let Response::ClientPaneList { panes, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ClientList {
                    workspace: Some("1".to_string()),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ClientPaneList");
        };
        assert!(
            !panes.iter().any(|p| p.id == pane),
            "cmd-w on an unbound pane must close the pane, removing it from the layout"
        );
    }
```

- [ ] **Step 2: Run the rewritten test — verify it fails**

Run: `cargo test --lib close_tab_on_an_unbound_pane_closes_the_pane`
Expected: FAIL with "cmd-w on an unbound pane must close the pane, removing it from the layout" (the pane still survives today's no-op behavior).

- [ ] **Step 3: Implement the `close_tab` change**

In `src/tui/mod.rs`, replace `close_tab`'s doc comment and body (from the `/// `cmd-w`: drop the focused client-pane's active tab...` comment through the end of the function, ~lines 919-978) with:

```rust
    /// `cmd-w`: drop the focused client-pane's active tab AND kill its
    /// bound server-pane -- unlike `dimax client close`/`ClientCloseTab`
    /// alone (which deliberately leaves the server-pane running for
    /// scripted callers), this chord is meant to fully clean up what it
    /// was looking at. When that was the pane's only tab the daemon
    /// closes the whole leaf instead (see `state::client_close_tab`),
    /// so this degrades to closing the pane itself on a single-tab
    /// leaf. A pane that's *already* unbound (no tabs at all -- e.g.
    /// right after detaching via `cmd-shift-z` and backing out of the
    /// picker) closes the same way: `ClientCloseTab` on an empty leaf
    /// removes the pane from the layout (the owner's call -- the empty
    /// placeholder is clutter to clean up, not a grid slot to
    /// preserve), with no `ServerKill` since nothing is bound to kill.
    /// Closing the workspace's last pane lands in the empty-workspace
    /// render state rather than exiting. In every leaf-closing case
    /// focus reassignment happens via `reconcile_focus` once the
    /// resulting `LayoutDelta` lands, rather than being computed here
    /// from a tree this function doesn't have an up-to-date copy of
    /// yet. The one layout this chord still never touches is a
    /// multi-tab pane: closing the active tab there must not remove
    /// the pane from the grid.
    async fn close_tab(
        &mut self,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        let Some(pane) = self.focused else {
            return Ok(());
        };
        // Read the active tab's binding *before* closing it -- once
        // `ClientCloseTab` lands, the tab (and, if it was the last one,
        // the leaf itself) is already gone, so there's no later point
        // at which this could still be read back (same reasoning as
        // `AttachMenu.previously_bound`'s doc comment). `None` here
        // means the leaf has no tabs at all, not just that the active
        // one happens to be unbound (every leaf with a non-empty
        // `tabs` has a valid `active_tab` index, so `active_bound`
        // only returns `None` when `tabs` is empty) -- in that case
        // the pane itself is what closes, and there is no bound
        // server-pane to kill afterward.
        let bound = self
            .workspace
            .tree
            .as_ref()
            .and_then(|t| t.find(pane))
            .and_then(|p| p.active_bound());
        let req = Request::ClientCloseTab {
            workspace: self.workspace.id.to_string(),
            pane,
        };
        let _ = self.request(write_half, reader, req).await?;
        if let Some(bound) = bound {
            let req = Request::ServerKill {
                target: bound.to_string(),
            };
            let _ = self.request(write_half, reader, req).await?;
        }
        Ok(())
    }
```

- [ ] **Step 4: Run the new test plus both guard-rail tests**

Run: `cargo test --lib close_tab`
Expected: PASS for all of:
- `close_tab_on_an_unbound_pane_closes_the_pane`
- `close_tab_kills_the_bound_server_pane` (unmodified)
- `close_tab_on_a_multi_tab_pane_drops_only_the_active_tab` (unmodified)

- [ ] **Step 5: Add the last-pane test**

Immediately after `close_tab_on_an_unbound_pane_closes_the_pane`, add:

```rust
    /// `cmd-w` closing the workspace's *last* pane must land in the
    /// empty-workspace state (tree `None`, spec: "dimax keeps running;
    /// it does not exit") and leave the workspace usable -- a
    /// subsequent spawn still creates a fresh leaf.
    #[tokio::test]
    async fn close_tab_on_an_unbound_last_pane_empties_the_workspace() {
        let (mut app, mut write_half, mut reader) = app_against_real_daemon().await;

        let Response::ClientPaneCreated { pane, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ClientSpawn {
                    workspace: "1".to_string(),
                    split_of: None,
                    dir: None,
                    bind: None,
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ClientPaneCreated");
        };
        app.focused = Some(pane);
        if let Response::Snapshot { workspace, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::Subscribe {
                    workspace: "1".to_string(),
                },
            )
            .await
            .unwrap()
        {
            app.workspace = workspace;
        }

        app.close_tab(&mut write_half, &mut reader).await.unwrap();

        let Response::Snapshot { workspace, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::Subscribe {
                    workspace: "1".to_string(),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected Snapshot");
        };
        assert!(
            workspace.tree.is_none(),
            "closing the last pane must leave the empty-workspace state, not exit or error"
        );

        // The workspace must still be usable: a fresh spawn recreates
        // a leaf.
        let Response::ServerPane(server) = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ServerSpawn {
                    name: None,
                    cmd: Some("cat".to_string()),
                    cwd: Some(test_cwd()),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ServerPane");
        };
        let Response::ClientPaneCreated { pane, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::ClientSpawn {
                    workspace: "1".to_string(),
                    split_of: None,
                    dir: None,
                    bind: Some(server.id.to_string()),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected ClientPaneCreated");
        };
        let Response::Snapshot { workspace, .. } = app
            .request(
                &mut write_half,
                &mut reader,
                Request::Subscribe {
                    workspace: "1".to_string(),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected Snapshot");
        };
        assert!(
            workspace.tree.is_some(),
            "a spawn after emptying the workspace must recreate the tree"
        );
        let _ = pane; // pane id only needed for the response shape above
    }
```

- [ ] **Step 6: Run the full focused set**

Run: `cargo test --lib close_tab`
Expected: PASS — 4 tests (2 new, 2 unmodified guard-rails).

- [ ] **Step 7: Format, lint, full suite**

Run: `cargo fmt && cargo fmt --check && cargo clippy --locked --all-targets 2>&1 | tail -3 && cargo test --locked 2>&1 | grep -E '^test result'`
Expected: fmt clean, no new clippy warnings vs `HEAD` (one pre-existing `derivable_impls` warning in `src/tui/keys.rs` is acceptable), all suites `ok` (408 passing + 2 new).

- [ ] **Step 8: Commit**

```bash
git add src/tui/mod.rs
git commit -m "feat: cmd-w on an unbound pane closes the pane

Reverses the old deliberate no-op: after detaching, the empty
placeholder pane is clutter to clean up, not a grid slot to
preserve. The unbound case now sends ClientCloseTab (the daemon
closes an empty leaf the same way it closes a just-emptied one)
and skips the ServerKill, since nothing is bound to kill. Closing
the last pane lands in the existing empty-workspace render state
rather than exiting. Bound-tab and multi-tab cmd-w semantics are
unchanged; see docs/superpowers/specs/2026-08-25-cmd-w-close-unbound-pane-design.md."
```

### Task 2: Ship — CI green, fast-forward main and stable

**Files:**
- None (git/CI operations only)

**Interfaces:**
- Consumes: Task 1's commit on `dev`.
- Produces: `main` and `stable` at the feature commit, CI green on all three branches.

- [ ] **Step 1: Push dev**

Run: `git push origin dev`
Expected: push succeeds; CI run starts for `dev`.

- [ ] **Step 2: Wait for CI on dev (both OSes)**

Run: `gh run watch $(gh run list --branch dev --limit 1 --json databaseId --jq '.[0].databaseId') --exit-status`
Expected: exit 0 — `test (ubuntu-latest)` and `test (macos-latest)` both succeed.

- [ ] **Step 3: Fast-forward main and stable**

Run: `git push origin dev:main && git push origin dev:stable`
Expected: both fast-forward pushes succeed.

- [ ] **Step 4: Confirm CI on main and stable**

Run: `sleep 240 && gh run list --limit 4`
Expected: latest runs for `main` and `stable` show `success`.

---

## Self-Review

1. **Spec coverage:** unbound close (Task 1 steps 1-4), no-ServerKill (Task 1 step 3 code), last-pane empty workspace + still-usable (Task 1 step 5), doc-comment rewrite (Task 1 step 3), no daemon/protocol change (Global Constraints + Task 1 touches only `src/tui/mod.rs`), bound/multi-tab unchanged (Task 1 step 4 guard-rails), CI + branch flow (Task 2). Spec's "portable-mode parity is automatic" needs no task — same `Action`, no code distinction.
2. **Placeholder scan:** every step carries exact code or an exact command; no TBD/TODO/"add tests" stubs.
3. **Type consistency:** `ClientCloseTab { workspace: String, pane }`, `ServerKill { target }`, `ClientPaneList { panes }` with `p.id`, `Snapshot { workspace }` with `workspace.tree` — all mirrored from existing tests in the same file; `close_tab` signature unchanged.
