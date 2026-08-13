# Client-Pane Tabs Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a single client-pane hold multiple server-pane bindings ("tabs"), showing one at a time, cyclable via keybind.

**Architecture:** Replace `ClientPane.bound: Option<ServerPaneId>` with `tabs: Vec<ServerPaneId>` + `active_tab: usize`. Every site that reads `leaf.bound` switches to `leaf.tabs.get(leaf.active_tab).copied()`. Three new wire requests (`ClientAddTab`, `ClientCycleTab`, `ClientCloseTab`) plus three new TUI chords (`cmd-t`, `cmd-]`, `cmd-[`). One PR, one branch (`feat/client-pane-tabs`).

**Tech Stack:** Rust, tokio, serde, ratatui, clap 4, uuid

---

### Task 1: Change `ClientPane` fields in `protocol.rs`

**Files:**
- Modify: `src/protocol.rs:30-35` (struct), `src/protocol.rs:510-511` (test helper `pane()`)

- [ ] **Step 1: Write failing test — construct a `ClientPane` with `tabs`/`active_tab` fields**

The existing test helper `pane()` at line 510 constructs `ClientPane { id, name: None, bound: None }`. Change it to the new shape — this won't compile until the struct is updated:

```rust
fn pane(id: ClientPaneId) -> ClientPane {
    ClientPane { id, name: None, tabs: vec![], active_tab: 0 }
}
```

Run: `cargo check 2>&1 | head -30`
Expected: compile error about `bound` field not existing / `tabs`/`active_tab` not being fields.

- [ ] **Step 2: Update `ClientPane` struct**

Replace lines 30-35:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientPane {
    pub id: ClientPaneId,
    pub name: Option<String>,
    pub tabs: Vec<ServerPaneId>,
    pub active_tab: usize,
}
```

- [ ] **Step 3: Add `active_bound()` helper method**

Below the struct (before `SplitTree`), add a convenience method every call site will use:

```rust
impl ClientPane {
    pub fn active_bound(&self) -> Option<ServerPaneId> {
        self.tabs.get(self.active_tab).copied()
    }
}
```

- [ ] **Step 4: Add three new `Request` variants**

After `ClientList` (line 339), add:

```rust
    ClientAddTab {
        workspace: String,
        pane: ClientPaneId,
        target: String,
    },
    ClientCycleTab {
        workspace: String,
        pane: ClientPaneId,
        forward: bool,
    },
    ClientCloseTab {
        workspace: String,
        pane: ClientPaneId,
    },
```

- [ ] **Step 5: Run `cargo check` to verify protocol compiles (many downstream errors expected)**

Run: `cargo check 2>&1 | grep "^error" | wc -l`
Expected: Many errors in `daemon/state.rs`, `daemon/mod.rs`, `cli.rs`, `tui/mod.rs`, `tui/render.rs` about `bound` field not existing. This is expected — we fix them in subsequent tasks.

- [ ] **Step 6: Commit**

```bash
git add src/protocol.rs
git commit -m "$(cat <<'EOF'
refactor(protocol): replace ClientPane.bound with tabs/active_tab

Breaking change to the wire protocol's ClientPane shape. Every
downstream consumer will be updated in subsequent commits.
EOF
)"
```

---

### Task 2: Update `daemon/state.rs` — core state methods

**Files:**
- Modify: `src/daemon/state.rs`

- [ ] **Step 1: Fix `client_spawn` — construct with new fields**

Line 378, change:
```rust
let pane = ClientPane {
    id: ClientPaneId::new_v4(),
    name: None,
    bound: bind,
};
```
to:
```rust
let pane = ClientPane {
    id: ClientPaneId::new_v4(),
    name: None,
    tabs: bind.into_iter().collect(),
    active_tab: 0,
};
```

- [ ] **Step 2: Fix `client_close` — read from `active_bound()`**

Line 420-423, change:
```rust
let bound = tree
    .find(pane)
    .ok_or_else(|| anyhow::anyhow!("client-pane {pane} not found in workspace {workspace}"))?
    .bound;
```
to:
```rust
let bound = tree
    .find(pane)
    .ok_or_else(|| anyhow::anyhow!("client-pane {pane} not found in workspace {workspace}"))?
    .active_bound();
```

- [ ] **Step 3: Fix `client_bind` — replace active tab**

Lines 444-461, replace:
```rust
pub fn client_bind(
    &mut self,
    workspace: WorkspaceId,
    pane: ClientPaneId,
    target: ServerPaneId,
) -> anyhow::Result<()> {
    if !self.server_panes.contains_key(&target) {
        anyhow::bail!("unknown server-pane {target}");
    }
    let leaf = self.client_pane_mut(workspace, pane)?;
    let previous = leaf.bound;
    leaf.bound = Some(target);
    if let Some(previous) = previous.filter(|p| *p != target) {
        self.apply_pty_size(previous);
    }
    self.apply_pty_size(target);
    Ok(())
}
```
with:
```rust
pub fn client_bind(
    &mut self,
    workspace: WorkspaceId,
    pane: ClientPaneId,
    target: ServerPaneId,
) -> anyhow::Result<()> {
    if !self.server_panes.contains_key(&target) {
        anyhow::bail!("unknown server-pane {target}");
    }
    let leaf = self.client_pane_mut(workspace, pane)?;
    let previous = leaf.active_bound();
    if leaf.tabs.is_empty() {
        leaf.tabs.push(target);
        leaf.active_tab = 0;
    } else {
        leaf.tabs[leaf.active_tab] = target;
    }
    if let Some(previous) = previous.filter(|p| *p != target) {
        self.apply_pty_size(previous);
    }
    self.apply_pty_size(target);
    Ok(())
}
```

- [ ] **Step 4: Fix `client_unbind` — remove active tab or clear all**

Lines 468-475, replace:
```rust
pub fn client_unbind(&mut self, workspace: WorkspaceId, pane: ClientPaneId) -> anyhow::Result<()> {
    let leaf = self.client_pane_mut(workspace, pane)?;
    let Some(previous) = leaf.bound.take() else {
        return Ok(());
    };
    self.apply_pty_size(previous);
    Ok(())
}
```
with:
```rust
pub fn client_unbind(&mut self, workspace: WorkspaceId, pane: ClientPaneId) -> anyhow::Result<()> {
    let leaf = self.client_pane_mut(workspace, pane)?;
    let Some(previous) = leaf.active_bound() else {
        return Ok(());
    };
    if leaf.tabs.len() <= 1 {
        leaf.tabs.clear();
        leaf.active_tab = 0;
    } else {
        leaf.tabs.remove(leaf.active_tab);
        if leaf.active_tab >= leaf.tabs.len() {
            leaf.active_tab = leaf.tabs.len() - 1;
        }
    }
    self.apply_pty_size(previous);
    Ok(())
}
```

- [ ] **Step 5: Fix `bound_server_pane`**

Line 538, change:
```rust
.and_then(|leaf| leaf.bound)
```
to:
```rust
.and_then(|leaf| leaf.active_bound())
```

- [ ] **Step 6: Fix `viewed_size`**

Line 653, change:
```rust
if leaf.bound != Some(server_pane) {
```
to:
```rust
if leaf.active_bound() != Some(server_pane) {
```

- [ ] **Step 7: Fix `recompute_workspace_pty_sizes`**

Line 697, change:
```rust
if let Some(server_pane) = leaf.bound
```
to:
```rust
if let Some(server_pane) = leaf.active_bound()
```

- [ ] **Step 8: Fix `subscribers_for_server_pane`**

Line 730, change:
```rust
tree.leaves().iter().any(|leaf| leaf.bound == Some(server_pane))
```
to:
```rust
tree.leaves().iter().any(|leaf| leaf.active_bound() == Some(server_pane))
```

- [ ] **Step 9: Fix `unbind_all` — remove from `tabs`, close leaf if last**

Lines 757-769, replace:
```rust
fn unbind_all(tree: &mut SplitTree, server_pane: ServerPaneId) {
    match tree {
        SplitTree::Leaf(pane) => {
            if pane.bound == Some(server_pane) {
                pane.bound = None;
            }
        }
        SplitTree::Split { a, b, .. } => {
            unbind_all(a, server_pane);
            unbind_all(b, server_pane);
        }
    }
}
```
with:
```rust
fn unbind_all(tree: &mut SplitTree, server_pane: ServerPaneId) {
    match tree {
        SplitTree::Leaf(pane) => {
            pane.tabs.retain(|&id| id != server_pane);
            if pane.active_tab >= pane.tabs.len() && !pane.tabs.is_empty() {
                pane.active_tab = pane.tabs.len() - 1;
            }
        }
        SplitTree::Split { a, b, .. } => {
            unbind_all(a, server_pane);
            unbind_all(b, server_pane);
        }
    }
}
```

Note: `server_kill` already handles leaf-removal for the "tabs became empty" case by iterating leaves, but `unbind_all` doesn't close leaves — it only mutates bindings. The existing `server_kill` flow already handles "client-pane left as unbound placeholder" behavior, which now means `tabs` can be empty. No leaf closing happens here; empty `tabs` is a valid state after a kill.

- [ ] **Step 10: Add `client_add_tab` method**

After `client_unbind`, add:
```rust
pub fn client_add_tab(
    &mut self,
    workspace: WorkspaceId,
    pane: ClientPaneId,
    target: ServerPaneId,
) -> anyhow::Result<()> {
    if !self.server_panes.contains_key(&target) {
        anyhow::bail!("unknown server-pane {target}");
    }
    let leaf = self.client_pane_mut(workspace, pane)?;
    leaf.tabs.push(target);
    leaf.active_tab = leaf.tabs.len() - 1;
    self.apply_pty_size(target);
    Ok(())
}
```

- [ ] **Step 11: Add `client_cycle_tab` method**

```rust
pub fn client_cycle_tab(
    &mut self,
    workspace: WorkspaceId,
    pane: ClientPaneId,
    forward: bool,
) -> anyhow::Result<()> {
    let (old_active, new_active) = {
        let leaf = self.client_pane_mut(workspace, pane)?;
        let len = leaf.tabs.len();
        if len <= 1 {
            return Ok(());
        }
        let old = leaf.active_bound();
        leaf.active_tab = if forward {
            (leaf.active_tab + 1) % len
        } else {
            (leaf.active_tab + len - 1) % len
        };
        let new = leaf.active_bound();
        (old, new)
    };
    if let Some(old) = old_active {
        self.apply_pty_size(old);
    }
    if let Some(new) = new_active {
        self.apply_pty_size(new);
    }
    Ok(())
}
```

- [ ] **Step 12: Add `client_close_tab` method**

```rust
pub fn client_close_tab(
    &mut self,
    workspace: WorkspaceId,
    pane: ClientPaneId,
) -> anyhow::Result<CloseTabResult> {
    // First pass: mutate the leaf, extract what we need, release borrow.
    let (removed, remaining_empty) = {
        let leaf = self.client_pane_mut(workspace, pane)?;
        if leaf.tabs.is_empty() {
            // Already unbound — treat as "close the leaf".
            return self.client_close(workspace, pane).map(|()| CloseTabResult::LeafClosed);
        }
        let removed = leaf.tabs.remove(leaf.active_tab);
        let empty = leaf.tabs.is_empty();
        if !empty && leaf.active_tab >= leaf.tabs.len() {
            leaf.active_tab = leaf.tabs.len() - 1;
        }
        (removed, empty)
    };
    if remaining_empty {
        return self.client_close(workspace, pane).map(|()| CloseTabResult::LeafClosed);
    }
    let new_active = self.bound_server_pane(pane);
    self.apply_pty_size(removed);
    if let Some(new) = new_active {
        self.apply_pty_size(new);
    }
    Ok(CloseTabResult::TabRemoved)
}
```
```

Add the result enum near the top of the file (after the `use` statements):
```rust
pub enum CloseTabResult {
    TabRemoved,
    LeafClosed,
}
```

- [ ] **Step 13: Fix test assertions that reference `.bound`**

Update every test in `state.rs` that reads/writes `.bound`:
- `client_spawn_without_split_creates_sole_leaf` (line 1096): `.bound` → `.active_bound()`
- `server_kill_unbinds_client_panes_across_workspaces` (line 904): same
- `client_bind_rebinds_and_rejects_unknown_targets` (lines 1284, 1289): same
- `client_unbind_detaches_and_leaves_server_pane_running` (line 1300): same
- `workspace_with_bound_pane` helper (line 787-797): the `bind` param is already `Some(server_pane)` which goes through `client_spawn` — no change needed since we updated `client_spawn`.

- [ ] **Step 14: Run tests**

Run: `cargo test -p dimax --lib daemon::state 2>&1 | tail -20`
Expected: All existing tests pass with the new field shape.

- [ ] **Step 15: Write new unit tests for `client_add_tab`/`client_cycle_tab`/`client_close_tab`**

Add in the test module:
```rust
#[test]
fn client_add_tab_appends_and_activates() {
    let mut state = State::new();
    let sp1 = spawn_pane(&mut state, "a");
    let sp2 = spawn_pane(&mut state, "b");
    let (ws, pane) = workspace_with_bound_pane(&mut state, "1", sp1);
    state.client_add_tab(ws, pane, sp2).unwrap();
    let tree = state.workspace_info(ws).unwrap().tree.unwrap();
    let leaf = tree.find(pane).unwrap();
    assert_eq!(leaf.tabs, vec![sp1, sp2]);
    assert_eq!(leaf.active_tab, 1);
}

#[test]
fn client_add_tab_unknown_target_errors() {
    let mut state = State::new();
    let sp = spawn_pane(&mut state, "a");
    let (ws, pane) = workspace_with_bound_pane(&mut state, "1", sp);
    assert!(state.client_add_tab(ws, pane, Uuid::new_v4()).is_err());
}

#[test]
fn client_cycle_tab_wraps_forward_and_backward() {
    let mut state = State::new();
    let sp1 = spawn_pane(&mut state, "a");
    let sp2 = spawn_pane(&mut state, "b");
    let sp3 = spawn_pane(&mut state, "c");
    let (ws, pane) = workspace_with_bound_pane(&mut state, "1", sp1);
    state.client_add_tab(ws, pane, sp2).unwrap();
    state.client_add_tab(ws, pane, sp3).unwrap();
    // active_tab is now 2 (sp3)
    state.client_cycle_tab(ws, pane, true).unwrap();
    assert_eq!(state.workspace_info(ws).unwrap().tree.unwrap().find(pane).unwrap().active_tab, 0);
    state.client_cycle_tab(ws, pane, false).unwrap();
    assert_eq!(state.workspace_info(ws).unwrap().tree.unwrap().find(pane).unwrap().active_tab, 2);
}

#[test]
fn client_cycle_tab_noop_on_single_tab() {
    let mut state = State::new();
    let sp = spawn_pane(&mut state, "a");
    let (ws, pane) = workspace_with_bound_pane(&mut state, "1", sp);
    state.client_cycle_tab(ws, pane, true).unwrap();
    assert_eq!(state.workspace_info(ws).unwrap().tree.unwrap().find(pane).unwrap().active_tab, 0);
}

#[test]
fn client_close_tab_removes_active_and_clamps() {
    let mut state = State::new();
    let sp1 = spawn_pane(&mut state, "a");
    let sp2 = spawn_pane(&mut state, "b");
    let (ws, pane) = workspace_with_bound_pane(&mut state, "1", sp1);
    state.client_add_tab(ws, pane, sp2).unwrap();
    // active_tab=1 (sp2), close it
    let result = state.client_close_tab(ws, pane).unwrap();
    assert!(matches!(result, CloseTabResult::TabRemoved));
    let leaf = state.workspace_info(ws).unwrap().tree.unwrap().find(pane).unwrap().clone();
    assert_eq!(leaf.tabs, vec![sp1]);
    assert_eq!(leaf.active_tab, 0);
}

#[test]
fn client_close_tab_last_tab_closes_the_leaf() {
    let mut state = State::new();
    let sp = spawn_pane(&mut state, "a");
    let (ws, pane) = workspace_with_bound_pane(&mut state, "1", sp);
    let result = state.client_close_tab(ws, pane).unwrap();
    assert!(matches!(result, CloseTabResult::LeafClosed));
    assert_eq!(state.workspace_info(ws).unwrap().tree, None);
}

#[test]
fn unbind_all_removes_killed_server_pane_from_background_tabs() {
    let mut state = State::new();
    let sp1 = spawn_pane(&mut state, "a");
    let sp2 = spawn_pane(&mut state, "b");
    let (ws, pane) = workspace_with_bound_pane(&mut state, "1", sp1);
    state.client_add_tab(ws, pane, sp2).unwrap();
    // active_tab=1 (sp2). Kill sp1 (a background tab).
    state.server_kill("a").unwrap();
    let leaf = state.workspace_info(ws).unwrap().tree.unwrap().find(pane).unwrap().clone();
    assert_eq!(leaf.tabs, vec![sp2]);
    assert_eq!(leaf.active_tab, 0);
}
```

- [ ] **Step 16: Run tests**

Run: `cargo test -p dimax --lib daemon::state 2>&1 | tail -20`
Expected: PASS

- [ ] **Step 17: Commit**

```bash
git add src/daemon/state.rs
git commit -m "$(cat <<'EOF'
feat(daemon/state): implement tabs data model and new tab methods

client_add_tab, client_cycle_tab, client_close_tab plus updates to
every existing method that read .bound to use .active_bound() instead.
unbind_all now removes killed panes from background tabs too.
EOF
)"
```

---

### Task 3: Update `daemon/mod.rs` — dispatch arms

**Files:**
- Modify: `src/daemon/mod.rs`

- [ ] **Step 1: Add `use state::CloseTabResult;` to imports**

Near line 19, add to the `use` block:
```rust
use state::CloseTabResult;
```

- [ ] **Step 2: Add dispatch arms for the three new requests**

After the `ClientUnbind` arm (line ~393), add:

```rust
        Request::ClientAddTab { workspace, pane, target } => {
            let mut state = state.lock().await;
            let ws_id = match state.resolve_workspace(&workspace) {
                Ok(id) => id,
                Err(err) => return Response::Error { message: err.to_string() },
            };
            let target_id = match state.resolve_server_pane(&target) {
                Ok(id) => id,
                Err(err) => return Response::Error { message: err.to_string() },
            };
            match state.client_add_tab(ws_id, pane, target_id) {
                Ok(()) => {
                    broadcast_layout(&state, registry, ws_id).await;
                    Response::Ack
                }
                Err(err) => Response::Error { message: err.to_string() },
            }
        }

        Request::ClientCycleTab { workspace, pane, forward } => {
            let mut state = state.lock().await;
            let ws_id = match state.resolve_workspace(&workspace) {
                Ok(id) => id,
                Err(err) => return Response::Error { message: err.to_string() },
            };
            match state.client_cycle_tab(ws_id, pane, forward) {
                Ok(()) => {
                    broadcast_layout(&state, registry, ws_id).await;
                    Response::Ack
                }
                Err(err) => Response::Error { message: err.to_string() },
            }
        }

        Request::ClientCloseTab { workspace, pane } => {
            let mut state = state.lock().await;
            let ws_id = match state.resolve_workspace(&workspace) {
                Ok(id) => id,
                Err(err) => return Response::Error { message: err.to_string() },
            };
            match state.client_close_tab(ws_id, pane) {
                Ok(_) => {
                    broadcast_layout(&state, registry, ws_id).await;
                    Response::Ack
                }
                Err(err) => Response::Error { message: err.to_string() },
            }
        }
```

- [ ] **Step 3: Fix `Subscribe` arm — `leaf.bound` → `leaf.active_bound()`**

Line 459, change:
```rust
.filter_map(|leaf| leaf.bound)
```
to:
```rust
.filter_map(|leaf| leaf.active_bound())
```

- [ ] **Step 4: Fix test assertions referencing `.bound`**

Lines 1032, 1237: change `leaf.bound` to `leaf.active_bound()`:
```rust
assert_eq!(leaf.active_bound(), None, "client-pane should be unbound after server_kill");
```
```rust
assert_eq!(leaf.active_bound(), None, "client-pane should be unbound");
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p dimax --lib daemon::mod 2>&1 | tail -30`
Expected: PASS (may still have compile errors in cli/tui — that's fine, `--lib` filters).

Actually run: `cargo test -p dimax --lib daemon 2>&1 | tail -30`

- [ ] **Step 6: Commit**

```bash
git add src/daemon/mod.rs
git commit -m "$(cat <<'EOF'
feat(daemon): wire dispatch for ClientAddTab/CycleTab/CloseTab
EOF
)"
```

---

### Task 4: Update `cli.rs` — new `add-tab` subcommand and format change

**Files:**
- Modify: `src/cli.rs`

- [ ] **Step 1: Add `AddTab` variant to `ClientCmd`**

After `Unbind` (line 115):
```rust
    AddTab { addr: String, target: String },
```

- [ ] **Step 2: Add dispatch arm in `run_client`**

After the `Unbind` match arm, add:
```rust
        ClientCmd::AddTab { addr, target } => {
            let (workspace, pane) = parse_pane_addr(&addr)?;
            let req = Request::ClientAddTab { workspace, pane, target: target.clone() };
            match client.request(req).await? {
                Response::Ack => {
                    println!("added tab {target} to {addr}");
                    Ok(())
                }
                other => Err(unexpected_response("client add-tab", other)),
            }
        }
```

- [ ] **Step 3: Update `format_client_pane_line`**

Replace:
```rust
fn format_client_pane_line(pane: &ClientPane) -> String {
    let name = pane.name.as_deref().unwrap_or("-");
    let bound = pane
        .bound
        .map(|id| id.to_string())
        .unwrap_or_else(|| "-".to_string());
    format!("{}\t{}\t{}", pane.id, name, bound)
}
```
with:
```rust
fn format_client_pane_line(pane: &ClientPane) -> String {
    let name = pane.name.as_deref().unwrap_or("-");
    let active = pane
        .active_bound()
        .map(|id| id.to_string())
        .unwrap_or_else(|| "-".to_string());
    let count = if pane.tabs.is_empty() {
        "-".to_string()
    } else {
        format!("{}/{}", pane.active_tab + 1, pane.tabs.len())
    };
    format!("{}\t{}\t{}\t{}", pane.id, name, active, count)
}
```

- [ ] **Step 4: Update tests**

Replace:
```rust
#[test]
fn format_client_pane_line_bound() {
    let server_pane = Uuid::new_v4();
    let pane = ClientPane {
        id: Uuid::nil(),
        name: Some("shell".to_string()),
        bound: Some(server_pane),
    };
    let line = format_client_pane_line(&pane);
    assert_eq!(
        line,
        format!("{}\tshell\t{server_pane}", Uuid::nil())
    );
}

#[test]
fn format_client_pane_line_unbound_unnamed() {
    let pane = ClientPane { id: Uuid::nil(), name: None, bound: None };
    let line = format_client_pane_line(&pane);
    assert_eq!(line, format!("{}\t-\t-", Uuid::nil()));
}
```
with:
```rust
#[test]
fn format_client_pane_line_single_tab() {
    let server_pane = Uuid::new_v4();
    let pane = ClientPane {
        id: Uuid::nil(),
        name: Some("shell".to_string()),
        tabs: vec![server_pane],
        active_tab: 0,
    };
    let line = format_client_pane_line(&pane);
    assert_eq!(
        line,
        format!("{}\tshell\t{server_pane}\t1/1", Uuid::nil())
    );
}

#[test]
fn format_client_pane_line_multiple_tabs() {
    let sp1 = Uuid::new_v4();
    let sp2 = Uuid::new_v4();
    let pane = ClientPane {
        id: Uuid::nil(),
        name: Some("editor".to_string()),
        tabs: vec![sp1, sp2],
        active_tab: 1,
    };
    let line = format_client_pane_line(&pane);
    assert_eq!(
        line,
        format!("{}\teditor\t{sp2}\t2/2", Uuid::nil())
    );
}

#[test]
fn format_client_pane_line_unbound_unnamed() {
    let pane = ClientPane { id: Uuid::nil(), name: None, tabs: vec![], active_tab: 0 };
    let line = format_client_pane_line(&pane);
    assert_eq!(line, format!("{}\t-\t-\t-", Uuid::nil()));
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p dimax --lib cli 2>&1 | tail -20`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add src/cli.rs
git commit -m "$(cat <<'EOF'
feat(cli): add client add-tab subcommand, update ls format to show tab count
EOF
)"
```

---

### Task 5: Update `tui/keys.rs` and `tui/kitty_setup.rs` — new chords

**Files:**
- Modify: `src/tui/keys.rs`, `src/tui/kitty_setup.rs`

- [ ] **Step 1: Add three new `Action` variants**

In `src/tui/mod.rs`, after `DetachAndAttach` (line 197):
```rust
    AddTab,
    CycleTabForward,
    CycleTabBackward,
```

- [ ] **Step 2: Add three new `Chord` variants and wire them in `keys.rs`**

In `enum Chord` (after `FocusRight`):
```rust
    AddTab,
    CycleTabForward,
    CycleTabBackward,
```

In `from_tag`:
```rust
b't' => Some(Chord::AddTab),
b']' => Some(Chord::CycleTabForward),
b'[' => Some(Chord::CycleTabBackward),
```

In `into_action`:
```rust
Chord::AddTab => Action::AddTab,
Chord::CycleTabForward => Action::CycleTabForward,
Chord::CycleTabBackward => Action::CycleTabBackward,
```

- [ ] **Step 3: Add to `CHORDS` table in `kitty_setup.rs`**

After the `("cmd+l", b'l')` entry (line 43), add:
```rust
    ("cmd+t", b't'),
    ("cmd+]", b']'),
    ("cmd+[", b'['),
```

- [ ] **Step 4: Update the chord table in `keys.rs` module doc comment**

Add three rows to the table (after `shift-enter`):
```
//! | `cmd-t`       | `t`  | `\x1b_Dt\x1b\\`               |
//! | `cmd-]`       | `]`  | `\x1b_D]\x1b\\`               |
//! | `cmd-[`       | `[`  | `\x1b_D[\x1b\\`               |
```

- [ ] **Step 5: Add tests for the new chords**

In `keys.rs` tests:
```rust
#[test]
fn add_tab() {
    assert_eq!(parse(&chord_bytes(b't')), Action::AddTab);
}

#[test]
fn cycle_tab_forward() {
    assert_eq!(parse(&chord_bytes(b']')), Action::CycleTabForward);
}

#[test]
fn cycle_tab_backward() {
    assert_eq!(parse(&chord_bytes(b'[')), Action::CycleTabBackward);
}
```

- [ ] **Step 6: Run tests**

Run: `cargo test -p dimax --lib tui::keys 2>&1 | tail -20`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add src/tui/keys.rs src/tui/kitty_setup.rs src/tui/mod.rs
git commit -m "$(cat <<'EOF'
feat(tui): add cmd-t/cmd-]/cmd-[ chords for tab management
EOF
)"
```

---

### Task 6: Update `tui/mod.rs` — action handlers, attach menu `adding_tab` mode

**Files:**
- Modify: `src/tui/mod.rs`

- [ ] **Step 1: Add `adding_tab: bool` field to `AttachMenu`**

After `spawn_in_group` field (line 249):
```rust
    adding_tab: bool,
```

- [ ] **Step 2: Fix all `AttachMenu { ... }` construction sites**

Every `AttachMenu { ... }` literal in this file needs `adding_tab: false` (or `true` for the `cmd-t` path) appended. The one at line 606 (`detach_and_open_menu`) gets `adding_tab: false`. All test helpers get `adding_tab: false`.

- [ ] **Step 3: Add `open_add_tab_menu` method to `App`**

This is `cmd-t`'s handler — similar to `detach_and_open_menu` but does NOT unbind first:
```rust
async fn open_add_tab_menu(
    &mut self,
    write_half: &mut OwnedWriteHalf,
    reader: &mut FrameReader,
) -> anyhow::Result<()> {
    let Some(_pane) = self.focused else { return Ok(()) };
    if let Response::PinnedDirsList(pinned) =
        self.request(write_half, reader, Request::PinnedDirsList).await?
    {
        self.pinned_dirs = pinned;
    }
    if let Response::ServerPaneList(servers) =
        self.request(write_half, reader, Request::ServerList).await?
    {
        self.attach_menu = Some(AttachMenu {
            servers: group_servers_by_cwd(servers, &self.pinned_dirs),
            selected: 0,
            pending_delete: None,
            rename: None,
            previously_bound: None,
            spawn_in_group: None,
            adding_tab: true,
        });
    }
    Ok(())
}
```

- [ ] **Step 4: Add `cycle_tab` and `close_tab` methods**

```rust
async fn cycle_tab(
    &mut self,
    forward: bool,
    write_half: &mut OwnedWriteHalf,
    reader: &mut FrameReader,
) -> anyhow::Result<()> {
    let Some(pane) = self.focused else { return Ok(()) };
    let req = Request::ClientCycleTab {
        workspace: self.workspace.id.to_string(),
        pane,
        forward,
    };
    let _ = self.request(write_half, reader, req).await?;
    Ok(())
}

async fn close_tab(
    &mut self,
    write_half: &mut OwnedWriteHalf,
    reader: &mut FrameReader,
) -> anyhow::Result<()> {
    let Some(pane) = self.focused else { return Ok(()) };
    let req = Request::ClientCloseTab {
        workspace: self.workspace.id.to_string(),
        pane,
    };
    let _ = self.request(write_half, reader, req).await?;
    Ok(())
}
```

- [ ] **Step 5: Update `handle_action` — wire new actions**

Change `CloseFocusedPane` from calling `close_focused` to `close_tab`:
```rust
Action::CloseFocusedPane => self.close_tab(write_half, reader).await?,
```

Add cases for the new actions:
```rust
Action::AddTab => self.open_add_tab_menu(write_half, reader).await?,
Action::CycleTabForward => self.cycle_tab(true, write_half, reader).await?,
Action::CycleTabBackward => self.cycle_tab(false, write_half, reader).await?,
```

- [ ] **Step 6: Update `confirm_attach_menu` — branch on `adding_tab`**

In `confirm_attach_menu` (line ~1028), replace:
```rust
let req = Request::ClientBind { workspace: self.workspace.id.to_string(), pane, target };
let _ = self.request(write_half, reader, req).await?;
```
with:
```rust
let req = if menu.adding_tab {
    Request::ClientAddTab { workspace: self.workspace.id.to_string(), pane, target }
} else {
    Request::ClientBind { workspace: self.workspace.id.to_string(), pane, target }
};
let _ = self.request(write_half, reader, req).await?;
```

Note: `menu` was already `take()`n above — capture `adding_tab` before the take:
```rust
let Some(menu) = self.attach_menu.take() else { return Ok(()) };
let adding_tab = menu.adding_tab;
```
Then use `adding_tab` in the branch.

- [ ] **Step 7: Update `confirm_spawn_in_group` — same branch**

In `confirm_spawn_in_group` (line ~1069-1078), the `if bind` block sends `ClientBind`. Change to:
```rust
if bind
    && let Some(pane) = self.focused
{
    let adding_tab = self.attach_menu.as_ref().is_some_and(|m| m.adding_tab);
    let req = if adding_tab {
        Request::ClientAddTab {
            workspace: self.workspace.id.to_string(),
            pane,
            target: server_pane.to_string(),
        }
    } else {
        Request::ClientBind {
            workspace: self.workspace.id.to_string(),
            pane,
            target: server_pane.to_string(),
        }
    };
    let _ = self.request(write_half, reader, req).await?;
}
```

- [ ] **Step 8: Update `kill_focused_server_pane` — use `active_bound()`**

Line 571, change:
```rust
let Some(bound) = self.workspace.tree.as_ref().and_then(|t| t.find(pane)).and_then(|p| p.bound)
```
to:
```rust
let Some(bound) = self.workspace.tree.as_ref().and_then(|t| t.find(pane)).and_then(|p| p.active_bound())
```

- [ ] **Step 9: Update `detach_and_open_menu` — use `active_bound()`**

Line 595, change:
```rust
let previously_bound =
    self.workspace.tree.as_ref().and_then(|tree| tree.find(pane)).and_then(|leaf| leaf.bound);
```
to:
```rust
let previously_bound =
    self.workspace.tree.as_ref().and_then(|tree| tree.find(pane)).and_then(|leaf| leaf.active_bound());
```

- [ ] **Step 10: Fix tests in this file that construct `AttachMenu` or read `.bound`**

- Every `AttachMenu { ... }` literal: append `adding_tab: false`.
- Line 2655 `bound_pane.bound.expect(...)` → `bound_pane.active_bound().expect(...)`
- Line 2733 `bound_pane.bound` → `bound_pane.active_bound()`
- The `leaf()` test helper (line 1845): change to `ClientPane { id, name: None, tabs: vec![], active_tab: 0 }`

- [ ] **Step 11: Run tests**

Run: `cargo test -p dimax --lib tui 2>&1 | tail -30`
Expected: PASS (render.rs may still fail — that's the next task).

- [ ] **Step 12: Commit**

```bash
git add src/tui/mod.rs
git commit -m "$(cat <<'EOF'
feat(tui): implement tab cycling, close-tab, and add-tab menu mode
EOF
)"
```

---

### Task 7: Update `tui/render.rs` — title bar `(N/M)` suffix and test fixes

**Files:**
- Modify: `src/tui/render.rs`

- [ ] **Step 1: Update `draw_leaf` — use `active_bound()` and add tab suffix**

Replace lines 293-314:
```rust
let snapshot = pane.bound.and_then(|server_pane_id| grids.get(&server_pane_id));
let mut title = pane
    .name
    .clone()
    .unwrap_or_else(|| short_id(pane.id));
if snapshot.is_some_and(|s| s.scroll_offset > 0) {
    title.push_str(" [scrollback]");
}
```
with:
```rust
let active = pane.active_bound();
let snapshot = active.and_then(|server_pane_id| grids.get(&server_pane_id));
let mut title = pane
    .name
    .clone()
    .unwrap_or_else(|| short_id(pane.id));
if pane.tabs.len() > 1 {
    title.push_str(&format!(" ({}/{})", pane.active_tab + 1, pane.tabs.len()));
}
if snapshot.is_some_and(|s| s.scroll_offset > 0) {
    title.push_str(" [scrollback]");
}
```

Replace line 314:
```rust
match pane.bound {
    None => {
```
with:
```rust
match active {
    None => {
```

- [ ] **Step 2: Fix the `leaf_pane` test helper**

```rust
fn leaf_pane(bound: Option<ServerPaneId>) -> ClientPane {
    ClientPane {
        id: Uuid::new_v4(),
        name: None,
        tabs: bound.into_iter().collect(),
        active_tab: 0,
    }
}
```

- [ ] **Step 3: Fix every `ClientPane { ... bound: ... }` literal in tests**

For every occurrence (there are ~14), change from:
```rust
ClientPane { id: ..., name: ..., bound: Some(server_id) }
```
to:
```rust
ClientPane { id: ..., name: ..., tabs: vec![server_id], active_tab: 0 }
```
And unbound ones:
```rust
ClientPane { id: ..., name: ..., bound: None }
```
to:
```rust
ClientPane { id: ..., name: ..., tabs: vec![], active_tab: 0 }
```

- [ ] **Step 4: Fix every `AttachMenu { ... }` literal in tests**

Append `adding_tab: false` to all 17 occurrences.

- [ ] **Step 5: Add test for the `(N/M)` suffix**

```rust
#[test]
fn draw_leaf_shows_tab_count_only_when_multiple_tabs() {
    let pane_id = Uuid::new_v4();
    let sp1 = Uuid::new_v4();
    let sp2 = Uuid::new_v4();
    let pane = ClientPane {
        id: pane_id,
        name: Some("editor".to_string()),
        tabs: vec![sp1, sp2],
        active_tab: 0,
    };
    let grids = HashMap::new();
    let backend = TestBackend::new(40, 6);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| draw_leaf(frame, &pane, frame.area(), &grids, None))
        .unwrap();
    assert!(buffer_contains(&terminal, "(1/2)"));
}

#[test]
fn draw_leaf_hides_tab_count_for_single_tab() {
    let pane_id = Uuid::new_v4();
    let sp = Uuid::new_v4();
    let pane = ClientPane {
        id: pane_id,
        name: Some("shell".to_string()),
        tabs: vec![sp],
        active_tab: 0,
    };
    let grids = HashMap::new();
    let backend = TestBackend::new(40, 6);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| draw_leaf(frame, &pane, frame.area(), &grids, None))
        .unwrap();
    assert!(!buffer_contains(&terminal, "(1/1)"));
    assert!(buffer_contains(&terminal, "shell"));
}
```

- [ ] **Step 6: Run all tests**

Run: `cargo test 2>&1 | tail -30`
Expected: ALL PASS

- [ ] **Step 7: Commit**

```bash
git add src/tui/render.rs
git commit -m "$(cat <<'EOF'
feat(tui/render): show (N/M) tab indicator when multiple tabs exist
EOF
)"
```

---

### Task 8: Final build, test, and ship

**Files:** None new — verification only.

- [ ] **Step 1: Full test suite**

Run: `cargo test 2>&1 | tail -40`
Expected: ALL PASS

- [ ] **Step 2: Clippy**

Run: `cargo clippy -- -D warnings 2>&1 | tail -20`
Expected: No warnings

- [ ] **Step 3: Build release**

Run: `cargo build --release 2>&1 | tail -5`
Expected: Success

- [ ] **Step 4: Push and open draft PR**

```bash
git push -u origin feat/client-pane-tabs
gh pr create --draft --title "feat: cyclable client-pane tabs" --body "$(cat <<'EOF'
## Summary
- Replace `ClientPane.bound: Option<ServerPaneId>` with `tabs: Vec<ServerPaneId>` + `active_tab: usize`
- Add `ClientAddTab`, `ClientCycleTab`, `ClientCloseTab` wire requests
- New TUI chords: `cmd-t` (add tab via menu), `cmd-]`/`cmd-[` (cycle), `cmd-w` (close tab)
- `dimax client add-tab` CLI subcommand; `client ls` output now shows `N/M` tab position
- Title bar shows `(N/M)` only when >1 tab; single-tab leaves are pixel-identical to before

## Test plan
- [ ] `cargo test` passes
- [ ] Manual: `dimax attach`, split pane, `cmd-t` to add a second tab, `cmd-]`/`cmd-[` to cycle, `cmd-w` to close one tab
- [ ] Manual: `dimax client add-tab <addr> <target>` from CLI works
- [ ] Manual: killing a background tab's server-pane removes it from the tab list cleanly
EOF
)"
```

- [ ] **Step 5: Report PR URL**
