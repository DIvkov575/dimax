//! Pure daemon state: the server-pane pool and the workspace/split-tree
//! pool, plus the subscriber bookkeeping needed for event broadcast and
//! smallest-viewer-wins PTY sizing.
//!
//! Deliberately free of sockets and async: every method here is
//! synchronous and unit testable as plain data-structure logic (see design
//! doc "Testing" — unit tests for `SplitTree` operations belong here,
//! exercised through `State`'s workspace methods rather than directly,
//! since `State` is what owns workspace lookup-by-name-or-id). The one
//! tokio type it touches is the [`ServerPaneEvent`] channel *sender* it
//! hands to each [`ServerPane`] it spawns; `tokio::sync::mpsc` senders
//! work outside a runtime, so tests need no reactor.
//!
//! The matching receiver is created in [`State::new`] and must be claimed
//! once at daemon start-up via [`State::take_pane_events`] — that stream
//! is the only way PTY output/exit ever reaches subscribers, so a daemon
//! that never claims it will render nothing.

use crate::protocol::{
    ClientPane, ClientPaneId, ServerPaneId, ServerPaneInfo, Size, SplitDir, SplitId, SplitTree,
    WorkspaceId, WorkspaceInfo,
};
use crate::term::{ServerPane, ServerPaneEvent};
use std::collections::HashMap;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use uuid::Uuid;

/// One live subscriber: a connection that has `Subscribe`d to a
/// workspace. Identified by an opaque id the connection handler mints
/// (e.g. a per-connection counter or the `UnixStream`'s file descriptor);
/// `State` itself never needs to know what it means, only that it's a
/// stable key for "this viewer" across subscribe/unsubscribe/broadcast.
pub type SubscriberId = u64;

/// What [`State::client_close_tab`] actually did: dropped one of several
/// tabs, or ran out of tabs and closed the client-pane itself. Callers
/// broadcast a different layout delta for each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseTabResult {
    TabRemoved,
    LeafClosed,
}

/// Size a freshly spawned server-pane's PTY starts at, before any
/// frontend reports the on-screen size of a client-pane bound to it
/// (design doc "PTY sizing" only defines sizing *once* there are
/// viewers). The conventional 80x24 default.
const DEFAULT_PTY_SIZE: Size = Size { rows: 24, cols: 80 };

pub struct State {
    server_panes: HashMap<ServerPaneId, ServerPane>,
    workspaces: HashMap<WorkspaceId, Workspace>,
    /// workspace -> set of subscribers currently viewing it. Drives both
    /// event broadcast and (via each workspace's bound server-panes)
    /// smallest-viewer-wins PTY sizing.
    subscribers: HashMap<WorkspaceId, Vec<SubscriberId>>,
    /// Last on-screen size reported for each client-pane
    /// (`Request::ResizeClientPane`). A pane absent from this map has no
    /// known size yet and is skipped when computing smallest-viewer-wins.
    client_pane_sizes: HashMap<ClientPaneId, Size>,
    /// Per-connection scroll position into a server-pane's history.
    /// Keyed by `(SubscriberId, ServerPaneId)`, NOT `ClientPaneId` --
    /// `GridSnapshot` carries only a `server_pane` id, so a connection
    /// can only ever see one grid per server-pane per broadcast
    /// regardless of how many client-panes it has bound to it; offset
    /// has to live at that same granularity to be deliverable over the
    /// wire at all. Absent means 0 (live). A zero-result store is
    /// removed rather than kept as an explicit `0` entry.
    scroll_offsets: HashMap<(SubscriberId, ServerPaneId), usize>,
    /// Cloned into every [`ServerPane::spawn`] so its reader thread can
    /// report output/exit.
    pane_events: UnboundedSender<ServerPaneEvent>,
    pane_events_rx: Option<UnboundedReceiver<ServerPaneEvent>>,
    /// Directory-group cwd strings the attach menu should sort first
    /// (in this order -- the earliest-pinned dir sorts above a
    /// later-pinned one, both above every unpinned dir), persisted to
    /// disk via `super::pinned_dirs` so pinning survives a daemon
    /// restart -- see that module's doc comment for why this is the
    /// one piece of `State` that isn't purely in-memory/ephemeral.
    /// Loaded once in `State::new`; every mutation
    /// (`toggle_pinned_dir`) re-saves immediately.
    pinned_dirs: Vec<String>,
}

struct Workspace {
    info_number: u8,
    name: Option<String>,
    tree: Option<SplitTree>,
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    pub fn new() -> Self {
        let (pane_events, pane_events_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            server_panes: HashMap::new(),
            workspaces: HashMap::new(),
            subscribers: HashMap::new(),
            client_pane_sizes: HashMap::new(),
            scroll_offsets: HashMap::new(),
            pane_events,
            pane_events_rx: Some(pane_events_rx),
            pinned_dirs: super::pinned_dirs::load(),
        }
    }

    /// Claim the stream of [`ServerPaneEvent`]s from every server-pane
    /// this `State` spawns, present and future. Returns `None` if already
    /// claimed — the daemon calls this exactly once at start-up and drives
    /// broadcast from it (see module doc).
    pub fn take_pane_events(&mut self) -> Option<UnboundedReceiver<ServerPaneEvent>> {
        self.pane_events_rx.take()
    }

    // -- server-panes --------------------------------------------------

    pub fn server_spawn(
        &mut self,
        name: Option<String>,
        cmd: Option<String>,
        cwd: Option<String>,
    ) -> anyhow::Result<ServerPaneInfo> {
        if let Some(name) = &name
            && self.find_server_pane_by_name(name).is_some()
        {
            anyhow::bail!("server-pane named {name:?} already exists");
        }
        let id = ServerPaneId::new_v4();
        let pane = ServerPane::spawn(
            id,
            name.clone(),
            cmd,
            cwd,
            DEFAULT_PTY_SIZE,
            self.pane_events.clone(),
        )?;
        let info = ServerPaneInfo {
            id,
            name,
            size: pane.size(),
            status: pane.status(),
            foreground: pane.foreground_info(),
        };
        self.server_panes.insert(id, pane);
        Ok(info)
    }

    /// Resolve `target` against both pane name and id-as-string, per the
    /// CLI surface's `<name-or-id>` addressing.
    pub fn resolve_server_pane(&self, target: &str) -> anyhow::Result<ServerPaneId> {
        if let Ok(id) = Uuid::parse_str(target)
            && self.server_panes.contains_key(&id)
        {
            return Ok(id);
        }
        self.find_server_pane_by_name(target)
            .ok_or_else(|| anyhow::anyhow!("unknown server-pane {target:?}"))
    }

    pub fn server_kill(&mut self, target: &str) -> anyhow::Result<()> {
        let id = self.resolve_server_pane(target)?;
        let mut pane = self
            .server_panes
            .remove(&id)
            .expect("resolve_server_pane only yields ids present in the pool");
        // The pane leaves the pool either way, so a kill error (typically
        // "process already exited") isn't worth failing the request over.
        let _ = pane.kill();
        // Design doc "Error handling": a killed server-pane leaves every
        // client-pane bound to it as an unbound placeholder — not closed,
        // not rebound.
        for ws in self.workspaces.values_mut() {
            if let Some(tree) = &mut ws.tree {
                unbind_all(tree, id);
            }
        }
        // No point remembering a scroll position into a server-pane
        // that no longer exists.
        self.scroll_offsets.retain(|(_, sp), _| *sp != id);
        Ok(())
    }

    pub fn server_rename(&mut self, target: &str, new_name: String) -> anyhow::Result<()> {
        let id = self.resolve_server_pane(target)?;
        if let Some(clash) = self.find_server_pane_by_name(&new_name)
            && clash != id
        {
            anyhow::bail!("server-pane named {new_name:?} already exists");
        }
        self.server_panes
            .get_mut(&id)
            .expect("resolve_server_pane only yields ids present in the pool")
            .set_name(Some(new_name));
        Ok(())
    }

    /// Give every still-unnamed server-pane whose foreground process is
    /// a Claude Code or Codex CLI session (see
    /// `term::session_name::derive_session_name`) that session's
    /// derived title as its real name -- the same `set_name` a manual
    /// `dimux server rename` would use, so once applied it's sticky:
    /// this only ever touches a pane while `name()` is still `None`,
    /// never overwrites one that already has a name (auto-derived or
    /// manual) even if the session's title later changes. Silently
    /// skips a pane whose derived name collides with one already taken
    /// -- worth quietly falling back to no name rather than erroring
    /// out of `server_list` entirely over a cosmetic naming clash.
    fn apply_pending_session_names(&mut self) {
        let to_name: Vec<(ServerPaneId, String)> = self
            .server_panes
            .iter()
            .filter(|(_, pane)| pane.name().is_none())
            .filter_map(|(&id, pane)| pane.derive_session_name().map(|name| (id, name)))
            .collect();
        for (id, name) in to_name {
            if self.find_server_pane_by_name(&name).is_some() {
                continue;
            }
            if let Some(pane) = self.server_panes.get_mut(&id) {
                pane.set_name(Some(name));
            }
        }
    }

    /// `&mut self`, not `&self`: this is the one place a still-unnamed
    /// pane picks up an auto-derived name from its foreground process
    /// (see [`Self::apply_pending_session_names`]) -- every caller of
    /// `server_list` (the attach menu, `dimux server ls`) already
    /// expects it to reflect the pool's current state, and applying the
    /// name here (rather than a separate poll) means it happens exactly
    /// on the same cadence those callers already re-fetch on, with no
    /// new background task.
    pub fn server_list(&mut self) -> Vec<ServerPaneInfo> {
        self.apply_pending_session_names();
        let mut panes: Vec<ServerPaneInfo> = self
            .server_panes
            .values()
            .map(|p| ServerPaneInfo {
                id: p.id(),
                name: p.name().map(str::to_string),
                size: p.size(),
                status: p.status(),
                foreground: p.foreground_info(),
            })
            .collect();
        // HashMap order is unspecified; sort so `dimux server ls` output
        // and tests are stable.
        panes.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        panes
    }

    /// Read access to a live pane, for the grid snapshots `Subscribe`
    /// answers with and the PTY writes `Request::Input` performs.
    /// `State` itself never reads grids or writes input.
    pub fn server_pane(&self, id: ServerPaneId) -> Option<&ServerPane> {
        self.server_panes.get(&id)
    }

    /// The current pin order (earliest-pinned first) -- what
    /// `tui::group_servers_by_cwd` sorts against. A plain accessor
    /// rather than something folded into `server_list`'s own return
    /// value: pinning is a directory-level concept independent of
    /// which (if any) server-panes currently exist for that directory,
    /// so it doesn't belong on `ServerPaneInfo` -- see `Request
    /// ::ServerList`'s dispatch arm, which fetches this alongside the
    /// pane list precisely because a client needs both to reproduce
    /// the attach menu's grouping.
    pub fn pinned_dirs(&self) -> &[String] {
        &self.pinned_dirs
    }

    /// Flip `dir`'s pinned state: pins it (appended to the end of the
    /// current pin order, so it sorts after every already-pinned dir
    /// but still above every unpinned one) if not already pinned,
    /// unpins it otherwise. Persists the new order to disk immediately
    /// via `pinned_dirs::save` -- see that module's doc comment for
    /// why this doesn't batch. Always succeeds (no validation: `dir` is
    /// just an opaque string from the caller's point of view, matched
    /// against whatever `ServerPaneInfo::foreground.cwd` values happen
    /// to be in play -- there's nothing to look up or fail on here).
    pub fn toggle_pinned_dir(&mut self, dir: String) {
        if let Some(pos) = self.pinned_dirs.iter().position(|d| *d == dir) {
            self.pinned_dirs.remove(pos);
        } else {
            self.pinned_dirs.push(dir);
        }
        super::pinned_dirs::save(&self.pinned_dirs);
    }

    fn find_server_pane_by_name(&self, name: &str) -> Option<ServerPaneId> {
        self.server_panes
            .values()
            .find(|p| p.name() == Some(name))
            .map(ServerPane::id)
    }

    // -- workspaces / client-panes --------------------------------------

    /// Resolve `target` against both workspace name and number-as-string
    /// (e.g. `"2"` or `"dev"`), creating an empty workspace on the fly if
    /// it doesn't exist yet — this is what makes `cmd-1..9` "create if
    /// absent" (design doc, Data model reference) work uniformly whether
    /// triggered by keybind or CLI, and what backs
    /// `Request::ClientSpawn`'s "workspace created if it doesn't exist".
    ///
    /// A newly created workspace addressed by a bare `1`-`9` takes that
    /// number; one addressed by name takes the lowest free number instead.
    /// A `target` that is all digits but outside `1`-`9` is rejected rather
    /// than silently becoming a *name* that looks like a number, and a
    /// well-formed but unknown workspace id is an error (ids are minted
    /// here, never supplied by callers).
    pub fn resolve_or_create_workspace(&mut self, target: &str) -> anyhow::Result<WorkspaceId> {
        if let Some(id) = self.find_workspace(target) {
            return Ok(id);
        }
        if Uuid::parse_str(target).is_ok() {
            anyhow::bail!("unknown workspace {target:?}");
        }
        let (info_number, name) = match chord_number(target) {
            Some(n) => (n, None),
            None => {
                if target.is_empty() {
                    anyhow::bail!("workspace name must not be empty");
                }
                if target.chars().all(|c| c.is_ascii_digit()) {
                    anyhow::bail!("workspace number must be 1-9, got {target:?}");
                }
                (self.lowest_free_number(), Some(target.to_string()))
            }
        };
        let id = WorkspaceId::new_v4();
        self.workspaces.insert(
            id,
            Workspace {
                info_number,
                name,
                tree: None,
            },
        );
        Ok(id)
    }

    pub fn resolve_workspace(&self, target: &str) -> anyhow::Result<WorkspaceId> {
        self.find_workspace(target)
            .ok_or_else(|| anyhow::anyhow!("unknown workspace {target:?}"))
    }

    pub fn workspace_info(&self, id: WorkspaceId) -> anyhow::Result<WorkspaceInfo> {
        let ws = self
            .workspaces
            .get(&id)
            .ok_or_else(|| anyhow::anyhow!("unknown workspace {id}"))?;
        Ok(WorkspaceInfo {
            id,
            number: ws.info_number,
            name: ws.name.clone(),
            tree: ws.tree.clone(),
        })
    }

    /// Implements `dimux client spawn`: create a new client-pane, either
    /// as the sole leaf of an empty workspace (`split_of: None`) or by
    /// splitting an existing leaf (`split_of: Some(pane)`). Errors if
    /// `split_of` is `None` but the workspace already has panes
    /// (ambiguous, per design doc "CLI surface").
    pub fn client_spawn(
        &mut self,
        workspace: WorkspaceId,
        split_of: Option<ClientPaneId>,
        dir: Option<SplitDir>,
        bind: Option<ServerPaneId>,
    ) -> anyhow::Result<ClientPaneId> {
        if let Some(server_pane) = bind
            && !self.server_panes.contains_key(&server_pane)
        {
            anyhow::bail!("unknown server-pane {server_pane}");
        }
        let ws = self
            .workspaces
            .get_mut(&workspace)
            .ok_or_else(|| anyhow::anyhow!("unknown workspace {workspace}"))?;
        let pane = ClientPane {
            id: ClientPaneId::new_v4(),
            name: None,
            tabs: bind.into_iter().collect(),
            active_tab: 0,
        };
        let id = pane.id;
        match split_of {
            None => {
                if ws.tree.is_some() {
                    anyhow::bail!(
                        "workspace {workspace} already has panes; name a pane to split instead"
                    );
                }
                ws.tree = Some(SplitTree::Leaf(pane));
            }
            Some(target) => {
                let tree = ws
                    .tree
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("workspace {workspace} has no panes to split"))?;
                // `split_leaf` validates `target` before mutating, so a
                // miss leaves the tree untouched (validate-then-apply).
                tree.split_leaf(target, dir.unwrap_or(SplitDir::Vertical), pane)?;
            }
        }
        if let Some(server_pane) = bind {
            self.apply_pty_size(server_pane);
        }
        Ok(id)
    }

    pub fn client_close(&mut self, workspace: WorkspaceId, pane: ClientPaneId) -> anyhow::Result<()> {
        let ws = self
            .workspaces
            .get_mut(&workspace)
            .ok_or_else(|| anyhow::anyhow!("unknown workspace {workspace}"))?;
        let tree = ws
            .tree
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("workspace {workspace} has no panes"))?;
        // Look the leaf up *before* `remove_leaf` consumes the tree, so a
        // miss can't destroy the layout on the error path.
        let bound = tree
            .find(pane)
            .ok_or_else(|| anyhow::anyhow!("client-pane {pane} not found in workspace {workspace}"))?
            .active_bound();
        let owned = ws.tree.take().expect("tree presence checked above");
        ws.tree = owned.remove_leaf(pane)?;
        self.client_pane_sizes.remove(&pane);
        // One fewer viewer of the pane it displayed.
        if let Some(server_pane) = bound {
            self.apply_pty_size(server_pane);
        }
        Ok(())
    }

    pub fn client_rename(
        &mut self,
        workspace: WorkspaceId,
        pane: ClientPaneId,
        new_name: String,
    ) -> anyhow::Result<()> {
        self.client_pane_mut(workspace, pane)?.name = Some(new_name);
        Ok(())
    }

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

    /// Detach `pane` from its current server-pane, if any (`cmd-shift-z`'s
    /// "detach" half — the server-pane keeps running, matching
    /// `client_bind`'s already-established "changing a binding recomputes
    /// the old target's PTY size" behavior rather than tearing it down).
    /// A no-op if `pane` was already unbound.
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

    /// Bind `target` as an *additional* tab of `pane`, made active, leaving
    /// the pane's existing tabs in place (contrast `client_bind`, which
    /// replaces the active tab).
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

    /// Move `pane`'s active tab one step (wrapping). A no-op when the pane
    /// has fewer than two tabs.
    pub fn client_cycle_tab(
        &mut self,
        workspace: WorkspaceId,
        pane: ClientPaneId,
        forward: bool,
    ) -> anyhow::Result<()> {
        // Scoped so the `&mut` leaf borrow ends before `apply_pty_size`.
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

    /// Drop `pane`'s active tab. Closes the whole client-pane when that was
    /// its last tab (or it had none), which the caller must know about to
    /// broadcast the right layout delta — hence [`CloseTabResult`].
    pub fn client_close_tab(
        &mut self,
        workspace: WorkspaceId,
        pane: ClientPaneId,
    ) -> anyhow::Result<CloseTabResult> {
        // Scoped so the `&mut` leaf borrow ends before `client_close`.
        let (removed, remaining_empty) = {
            let leaf = self.client_pane_mut(workspace, pane)?;
            if leaf.tabs.is_empty() {
                // Already unbound — treat as "close the leaf".
                return self
                    .client_close(workspace, pane)
                    .map(|()| CloseTabResult::LeafClosed);
            }
            let removed = leaf.tabs.remove(leaf.active_tab);
            let empty = leaf.tabs.is_empty();
            if !empty && leaf.active_tab >= leaf.tabs.len() {
                leaf.active_tab = leaf.tabs.len() - 1;
            }
            (removed, empty)
        };
        if remaining_empty {
            // `removed` is already gone from `leaf.tabs` by this point, so
            // `client_close`'s own resize logic (which reads the leaf's
            // *current* active_bound, now empty) recomputes nothing for
            // it -- apply it explicitly, same as the non-empty path below.
            let result = self.client_close(workspace, pane).map(|()| CloseTabResult::LeafClosed);
            self.apply_pty_size(removed);
            return result;
        }
        let new_active = self.bound_server_pane(pane);
        self.apply_pty_size(removed);
        if let Some(new) = new_active {
            self.apply_pty_size(new);
        }
        Ok(CloseTabResult::TabRemoved)
    }

    /// Set a split's ratio directly (mouse-drag resizing, design doc
    /// addendum). Does not touch PTY sizing on its own -- the caller
    /// (`daemon::dispatch`) is expected to also call `resize_client_pane`
    /// for whichever client-panes' on-screen size changed as a result,
    /// the same way a frontend reports any other resize.
    pub fn resize_split(
        &mut self,
        workspace: WorkspaceId,
        split: SplitId,
        new_ratio: f32,
    ) -> anyhow::Result<()> {
        self.workspaces
            .get_mut(&workspace)
            .ok_or_else(|| anyhow::anyhow!("unknown workspace {workspace}"))?
            .tree
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("workspace {workspace} has no panes"))?
            .resize_split(split, new_ratio)
    }

    pub fn client_list(&self, workspace: Option<WorkspaceId>) -> Vec<(WorkspaceId, ClientPane)> {
        let mut selected: Vec<(&WorkspaceId, &Workspace)> = self
            .workspaces
            .iter()
            .filter(|(id, _)| workspace.is_none_or(|wanted| wanted == **id))
            .collect();
        // Stable output for `dimux client ls` across all workspaces.
        selected.sort_by_key(|(id, ws)| (ws.info_number, **id));
        let mut out = Vec::new();
        for (id, ws) in selected {
            let Some(tree) = &ws.tree else { continue };
            for leaf in tree.leaves() {
                out.push((*id, leaf.clone()));
            }
        }
        out
    }

    fn client_pane_mut(
        &mut self,
        workspace: WorkspaceId,
        pane: ClientPaneId,
    ) -> anyhow::Result<&mut ClientPane> {
        self.workspaces
            .get_mut(&workspace)
            .ok_or_else(|| anyhow::anyhow!("unknown workspace {workspace}"))?
            .tree
            .as_mut()
            .and_then(|tree| tree.find_mut(pane))
            .ok_or_else(|| anyhow::anyhow!("client-pane {pane} not found in workspace {workspace}"))
    }

    /// The server-pane a client-pane currently displays, if any. Client-pane
    /// ids are unique across workspaces, so this searches all of them —
    /// `Request::Input`/`ResizeClientPane` address a pane without naming its
    /// workspace.
    pub fn bound_server_pane(&self, pane: ClientPaneId) -> Option<ServerPaneId> {
        self.workspaces
            .values()
            .filter_map(|ws| ws.tree.as_ref())
            .find_map(|tree| tree.find(pane))
            .and_then(|leaf| leaf.active_bound())
    }

    fn lowest_free_number(&self) -> u8 {
        let used: Vec<u8> = self.workspaces.values().map(|w| w.info_number).collect();
        // 0 means "not reachable by any cmd-N chord", used once all nine
        // numbers are taken. Named workspaces still address fine.
        (1..=9).find(|n| !used.contains(n)).unwrap_or(0)
    }

    fn find_workspace(&self, target: &str) -> Option<WorkspaceId> {
        if let Ok(id) = Uuid::parse_str(target) {
            // `dimux client spawn` prints `<workspace-uuid>/<pane-uuid>`,
            // so ids come straight back in as addresses.
            return self.workspaces.contains_key(&id).then_some(id);
        }
        if let Some(number) = chord_number(target) {
            return self
                .workspaces
                .iter()
                .find(|(_, ws)| ws.info_number == number)
                .map(|(id, _)| *id);
        }
        self.workspaces
            .iter()
            .find(|(_, ws)| ws.name.as_deref() == Some(target))
            .map(|(id, _)| *id)
    }

    // -- subscription / PTY sizing --------------------------------------

    pub fn subscribe(&mut self, subscriber: SubscriberId, workspace: WorkspaceId) {
        let subs = self.subscribers.entry(workspace).or_default();
        if !subs.contains(&subscriber) {
            subs.push(subscriber);
        }
        self.recompute_workspace_pty_sizes(workspace);
    }

    /// Also used to clean up a connection that dropped without sending
    /// an explicit `Unsubscribe` (design doc doesn't distinguish the two
    /// cases — see `daemon::handle_connection`'s post-loop cleanup).
    pub fn unsubscribe(&mut self, subscriber: SubscriberId, workspace: WorkspaceId) {
        if let Some(subs) = self.subscribers.get_mut(&workspace) {
            subs.retain(|s| *s != subscriber);
            if subs.is_empty() {
                self.subscribers.remove(&workspace);
            }
        }
        self.recompute_workspace_pty_sizes(workspace);
    }

    /// Recompute and apply smallest-viewer-wins sizing (design doc "PTY
    /// sizing") for one server-pane, given the current on-screen size of
    /// every client-pane bound to it across every workspace a subscriber
    /// is currently viewing.
    pub fn resize_client_pane(&mut self, pane: ClientPaneId, size: Size) {
        self.client_pane_sizes.insert(pane, size);
        if let Some(server_pane) = self.bound_server_pane(pane) {
            self.apply_pty_size(server_pane);
        }
    }

    /// Adjust `subscriber`'s scroll position into `server_pane`'s
    /// history by `delta` rows (positive = further back, negative =
    /// toward live), clamped to `0..=server_pane.scrollback_rows()`.
    /// Returns the resulting (already-clamped) offset.
    pub fn scroll_server_pane(&mut self, subscriber: SubscriberId, server_pane: ServerPaneId, delta: i32) -> usize {
        let Some(pane) = self.server_panes.get(&server_pane) else {
            return 0;
        };
        let max = pane.scrollback_rows();
        let current = self
            .scroll_offsets
            .get(&(subscriber, server_pane))
            .copied()
            .unwrap_or(0);
        let new_offset = (current as i64 + delta as i64).clamp(0, max as i64) as usize;
        if new_offset == 0 {
            self.scroll_offsets.remove(&(subscriber, server_pane));
        } else {
            self.scroll_offsets.insert((subscriber, server_pane), new_offset);
        }
        new_offset
    }

    /// The offset a fresh `GridSnapshot` for `server_pane` should be
    /// built at for `subscriber` -- 0 if they've never scrolled it.
    pub fn scroll_offset_for(&self, subscriber: SubscriberId, server_pane: ServerPaneId) -> usize {
        self.scroll_offsets.get(&(subscriber, server_pane)).copied().unwrap_or(0)
    }

    /// Remove every scroll-offset entry belonging to `subscriber` --
    /// called from `daemon::mod`'s `handle_connection` teardown path
    /// once a connection closes.
    pub fn clear_scroll_offsets_for_subscriber(&mut self, subscriber: SubscriberId) {
        self.scroll_offsets.retain(|(sub, _), _| *sub != subscriber);
    }

    /// Dimension-wise minimum on-screen size across every client-pane
    /// bound to `server_pane` in a workspace someone is currently viewing.
    /// `None` when nobody is viewing it (or no viewer has reported a size
    /// yet), which the design doc defines as "keep the last size".
    fn viewed_size(&self, server_pane: ServerPaneId) -> Option<Size> {
        let mut smallest: Option<Size> = None;
        for (ws_id, ws) in &self.workspaces {
            let viewed = self
                .subscribers
                .get(ws_id)
                .is_some_and(|subs| !subs.is_empty());
            if !viewed {
                continue;
            }
            let Some(tree) = &ws.tree else { continue };
            for leaf in tree.leaves() {
                if leaf.active_bound() != Some(server_pane) {
                    continue;
                }
                let Some(size) = self.client_pane_sizes.get(&leaf.id) else {
                    continue;
                };
                smallest = Some(match smallest {
                    None => *size,
                    Some(current) => Size {
                        rows: current.rows.min(size.rows),
                        cols: current.cols.min(size.cols),
                    },
                });
            }
        }
        smallest
    }

    /// `&self` rather than `&mut self`: resizing goes through
    /// `ServerPane`'s internal mutex, which keeps this callable from the
    /// middle of a `&mut self` workspace mutation without fighting the
    /// borrow checker.
    fn apply_pty_size(&self, server_pane: ServerPaneId) {
        let Some(size) = self.viewed_size(server_pane) else {
            return;
        };
        let Some(pane) = self.server_panes.get(&server_pane) else {
            return;
        };
        if pane.size() != size {
            // A failed ioctl (e.g. the PTY is gone because the process
            // exited) leaves the recorded size alone; nothing a
            // subscribe/resize caller can do about it.
            let _ = pane.resize(size);
        }
    }

    fn recompute_workspace_pty_sizes(&self, workspace: WorkspaceId) {
        let Some(ws) = self.workspaces.get(&workspace) else {
            return;
        };
        let Some(tree) = &ws.tree else { return };
        let mut seen: Vec<ServerPaneId> = Vec::new();
        for leaf in tree.leaves() {
            if let Some(server_pane) = leaf.active_bound()
                && !seen.contains(&server_pane)
            {
                seen.push(server_pane);
            }
        }
        for server_pane in seen {
            self.apply_pty_size(server_pane);
        }
    }

    // -- broadcast fan-out (used by daemon::dispatch, kept out of State so
    //    State never performs I/O — see module doc comment) -------------

    /// Every subscriber currently viewing `workspace`. `daemon::dispatch`
    /// calls this after a layout mutation succeeds to know which
    /// connections to push a `LayoutDelta` to.
    pub fn subscribers_for_workspace(&self, workspace: WorkspaceId) -> Vec<SubscriberId> {
        self.subscribers
            .get(&workspace)
            .cloned()
            .unwrap_or_default()
    }

    /// Every subscriber viewing any workspace that currently binds
    /// `server_pane`. `daemon::dispatch` calls this after PTY output
    /// changes (or the pane dies) to know which connections to push a
    /// `GridDelta`/`ServerPaneDied` to, and it's also the input to
    /// smallest-viewer-wins PTY sizing.
    pub fn subscribers_for_server_pane(&self, server_pane: ServerPaneId) -> Vec<SubscriberId> {
        let mut out: Vec<SubscriberId> = Vec::new();
        for (ws_id, ws) in &self.workspaces {
            let binds = ws.tree.as_ref().is_some_and(|tree| {
                tree.leaves()
                    .iter()
                    .any(|leaf| leaf.active_bound() == Some(server_pane))
            });
            if !binds {
                continue;
            }
            if let Some(subs) = self.subscribers.get(ws_id) {
                out.extend(subs.iter().copied());
            }
        }
        // The same subscriber can view two workspaces that both bind the
        // pane; callers want one push, not two.
        out.sort_unstable();
        out.dedup();
        out
    }
}

/// `1`-`9`, the workspace numbers `cmd-1`..`cmd-9` address. Any other
/// numeric string is *not* a workspace number, so number addressing stays
/// unambiguous.
fn chord_number(target: &str) -> Option<u8> {
    match target.parse::<u8>() {
        Ok(n @ 1..=9) => Some(n),
        _ => None,
    }
}

fn unbind_all(tree: &mut SplitTree, server_pane: ServerPaneId) {
    match tree {
        SplitTree::Leaf(pane) => {
            // Background tabs bind the killed pane just as much as the
            // active one does, so this drops every occurrence, not one.
            // `active_tab` is an *index*: removing an occurrence that
            // sits before it must shift it down by one, or the leaf
            // ends up silently displaying a different tab than the one
            // the user was looking at.
            let removed_before = pane.tabs[..pane.active_tab.min(pane.tabs.len())]
                .iter()
                .filter(|&&id| id == server_pane)
                .count();
            pane.tabs.retain(|&id| id != server_pane);
            pane.active_tab = pane.active_tab.saturating_sub(removed_before);
            if !pane.tabs.is_empty() && pane.active_tab >= pane.tabs.len() {
                pane.active_tab = pane.tabs.len() - 1;
            }
        }
        SplitTree::Split { a, b, .. } => {
            unbind_all(a, server_pane);
            unbind_all(b, server_pane);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ServerPaneStatus;

    /// Every server-pane in these tests runs `cat`: it stays alive with no
    /// output until its PTY master is dropped, so pane bookkeeping is
    /// exercised without depending on process timing.
    fn spawn_pane(state: &mut State, name: &str) -> ServerPaneId {
        state
            .server_spawn(Some(name.to_string()), Some("cat".to_string()), None)
            .unwrap()
            .id
    }

    /// A workspace with one leaf bound to `server_pane`, returning the leaf.
    fn workspace_with_bound_pane(
        state: &mut State,
        target: &str,
        server_pane: ServerPaneId,
    ) -> (WorkspaceId, ClientPaneId) {
        let ws = state.resolve_or_create_workspace(target).unwrap();
        let pane = state
            .client_spawn(ws, None, None, Some(server_pane))
            .unwrap();
        (ws, pane)
    }

    fn size(rows: u16, cols: u16) -> Size {
        Size { rows, cols }
    }

    fn pane_size(state: &State, server_pane: ServerPaneId) -> Size {
        state.server_pane(server_pane).unwrap().size()
    }

    // -- server-panes --------------------------------------------------

    #[test]
    fn server_spawn_returns_info_and_lists_it() {
        let mut state = State::new();
        let info = state
            .server_spawn(Some("shell".to_string()), Some("cat".to_string()), None)
            .unwrap();
        assert_eq!(info.name.as_deref(), Some("shell"));
        assert_eq!(info.size, DEFAULT_PTY_SIZE);
        assert_eq!(info.status, ServerPaneStatus::Running);
        let listed = state.server_list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, info.id);
    }

    #[test]
    fn server_spawn_rejects_duplicate_name() {
        let mut state = State::new();
        spawn_pane(&mut state, "shell");
        let err = state
            .server_spawn(Some("shell".to_string()), Some("cat".to_string()), None)
            .unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
        assert_eq!(state.server_list().len(), 1);
    }

    #[test]
    fn server_spawn_allows_repeated_anonymous_panes() {
        let mut state = State::new();
        state.server_spawn(None, Some("cat".to_string()), None).unwrap();
        state.server_spawn(None, Some("cat".to_string()), None).unwrap();
        assert_eq!(state.server_list().len(), 2);
    }

    #[test]
    fn resolve_server_pane_by_name_and_by_id() {
        let mut state = State::new();
        let id = spawn_pane(&mut state, "editor");
        assert_eq!(state.resolve_server_pane("editor").unwrap(), id);
        assert_eq!(state.resolve_server_pane(&id.to_string()).unwrap(), id);
    }

    #[test]
    fn resolve_server_pane_unknown_errors() {
        let state = State::new();
        assert!(state.resolve_server_pane("nope").is_err());
        assert!(state.resolve_server_pane(&Uuid::new_v4().to_string()).is_err());
    }

    #[test]
    fn server_rename_then_resolvable_under_new_name_only() {
        let mut state = State::new();
        let id = spawn_pane(&mut state, "old");
        state.server_rename("old", "new".to_string()).unwrap();
        assert_eq!(state.resolve_server_pane("new").unwrap(), id);
        assert!(state.resolve_server_pane("old").is_err());
    }

    #[test]
    fn server_rename_to_existing_name_errors_and_keeps_old_name() {
        let mut state = State::new();
        spawn_pane(&mut state, "a");
        let b = spawn_pane(&mut state, "b");
        let err = state.server_rename("b", "a".to_string()).unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
        assert_eq!(state.resolve_server_pane("b").unwrap(), b);
    }

    #[test]
    fn server_rename_to_same_name_is_allowed() {
        let mut state = State::new();
        let id = spawn_pane(&mut state, "a");
        state.server_rename("a", "a".to_string()).unwrap();
        assert_eq!(state.resolve_server_pane("a").unwrap(), id);
    }

    #[test]
    fn server_rename_unknown_errors() {
        let mut state = State::new();
        assert!(state.server_rename("ghost", "x".to_string()).is_err());
    }

    #[test]
    fn server_kill_removes_from_pool_and_unbinds_every_client_pane() {
        let mut state = State::new();
        let sp = spawn_pane(&mut state, "shared");
        let (ws_a, pane_a) = workspace_with_bound_pane(&mut state, "1", sp);
        let (ws_b, pane_b) = workspace_with_bound_pane(&mut state, "2", sp);

        state.server_kill("shared").unwrap();

        assert!(state.server_list().is_empty());
        assert!(state.server_pane(sp).is_none());
        // Design doc: the client-panes survive as unbound placeholders.
        for (ws, pane) in [(ws_a, pane_a), (ws_b, pane_b)] {
            let tree = state.workspace_info(ws).unwrap().tree.unwrap();
            assert_eq!(tree.find(pane).unwrap().active_bound(), None);
        }
    }

    #[test]
    fn server_kill_unknown_errors() {
        let mut state = State::new();
        assert!(state.server_kill("ghost").is_err());
    }

    /// `server_list`'s auto-naming pass must leave an unnamed pane whose
    /// foreground process isn't Claude Code or Codex (here: `cat`, same
    /// as every other test pane in this file) alone -- a real
    /// Claude/Codex-session test would need one of those actually
    /// running, out of scope for a unit test; this covers the "doesn't
    /// touch anything it doesn't recognize" side, which is the
    /// overwhelmingly common case in practice.
    #[test]
    fn server_list_leaves_unnamed_non_session_panes_unnamed() {
        let mut state = State::new();
        state.server_spawn(None, Some("cat".to_string()), None).unwrap();
        let names: Vec<Option<String>> = state.server_list().into_iter().map(|i| i.name).collect();
        assert_eq!(names, vec![None]);
    }

    #[test]
    fn server_list_is_sorted_by_name() {
        let mut state = State::new();
        spawn_pane(&mut state, "c");
        spawn_pane(&mut state, "a");
        spawn_pane(&mut state, "b");
        let names: Vec<Option<String>> = state.server_list().into_iter().map(|i| i.name).collect();
        assert_eq!(
            names,
            vec![
                Some("a".to_string()),
                Some("b".to_string()),
                Some("c".to_string())
            ]
        );
    }

    // -- directory pinning -----------------------------------------------

    /// Serializes every test in this section: `toggle_pinned_dir` saves
    /// to disk via `pinned_dirs::save`, which reads `$XDG_CONFIG_HOME`
    /// (process-global), so two such tests running concurrently (the
    /// default under `cargo test`) could otherwise stomp on each
    /// other's env state or on-disk file mid-test.
    static PIN_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_fake_config_home<T>(dir: &std::path::Path, f: impl FnOnce() -> T) -> T {
        let _guard = PIN_ENV_LOCK.lock().unwrap();
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", dir);
        }
        let result = f();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
        result
    }

    #[test]
    fn toggle_pinned_dir_pins_then_unpins() {
        let dir = std::env::temp_dir().join(format!("dmx-state-pin-test-{}", std::process::id()));
        with_fake_config_home(&dir, || {
            let mut state = State::new();
            assert_eq!(state.pinned_dirs(), &[] as &[String]);

            state.toggle_pinned_dir("/home/dev/api".to_string());
            assert_eq!(state.pinned_dirs(), &["/home/dev/api".to_string()]);

            state.toggle_pinned_dir("/home/dev/api".to_string());
            assert_eq!(state.pinned_dirs(), &[] as &[String]);
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn toggle_pinned_dir_appends_new_pins_after_existing_ones() {
        let dir = std::env::temp_dir().join(format!("dmx-state-pin-test-{}", std::process::id() + 1));
        with_fake_config_home(&dir, || {
            let mut state = State::new();
            state.toggle_pinned_dir("/a".to_string());
            state.toggle_pinned_dir("/b".to_string());
            assert_eq!(state.pinned_dirs(), &["/a".to_string(), "/b".to_string()]);
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn toggle_pinned_dir_unpinning_the_first_of_several_preserves_the_rest_in_order() {
        let dir = std::env::temp_dir().join(format!("dmx-state-pin-test-{}", std::process::id() + 2));
        with_fake_config_home(&dir, || {
            let mut state = State::new();
            state.toggle_pinned_dir("/a".to_string());
            state.toggle_pinned_dir("/b".to_string());
            state.toggle_pinned_dir("/c".to_string());
            state.toggle_pinned_dir("/a".to_string()); // unpin
            assert_eq!(state.pinned_dirs(), &["/b".to_string(), "/c".to_string()]);
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn toggle_pinned_dir_persists_across_a_fresh_state_load() {
        let dir = std::env::temp_dir().join(format!("dmx-state-pin-test-{}", std::process::id() + 3));
        with_fake_config_home(&dir, || {
            let mut state = State::new();
            state.toggle_pinned_dir("/home/dev/api".to_string());

            // A brand-new `State` (as a daemon restart would construct)
            // must pick the pin back up from disk.
            let reloaded = State::new();
            assert_eq!(reloaded.pinned_dirs(), &["/home/dev/api".to_string()]);
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- workspace resolution ------------------------------------------

    #[test]
    fn resolve_or_create_workspace_creates_numbered_workspace_once() {
        let mut state = State::new();
        let first = state.resolve_or_create_workspace("3").unwrap();
        let again = state.resolve_or_create_workspace("3").unwrap();
        assert_eq!(first, again);
        let info = state.workspace_info(first).unwrap();
        assert_eq!(info.number, 3);
        assert_eq!(info.name, None);
        assert_eq!(info.tree, None);
    }

    #[test]
    fn resolve_or_create_workspace_creates_named_workspace_with_lowest_free_number() {
        let mut state = State::new();
        state.resolve_or_create_workspace("1").unwrap();
        let dev = state.resolve_or_create_workspace("dev").unwrap();
        assert_eq!(state.workspace_info(dev).unwrap().number, 2);
        assert_eq!(
            state.workspace_info(dev).unwrap().name.as_deref(),
            Some("dev")
        );
        assert_eq!(state.resolve_workspace("dev").unwrap(), dev);
        assert_eq!(state.resolve_workspace("2").unwrap(), dev);
    }

    #[test]
    fn resolve_or_create_workspace_rejects_out_of_range_numbers() {
        let mut state = State::new();
        let err = state.resolve_or_create_workspace("42").unwrap_err();
        assert!(err.to_string().contains("1-9"), "{err}");
        assert!(state.resolve_or_create_workspace("0").is_err());
        assert!(state.resolve_or_create_workspace("").is_err());
    }

    #[test]
    fn resolve_or_create_workspace_rejects_unknown_id() {
        let mut state = State::new();
        let unknown = Uuid::new_v4().to_string();
        assert!(state.resolve_or_create_workspace(&unknown).is_err());
    }

    #[test]
    fn resolve_workspace_by_id_never_creates() {
        let mut state = State::new();
        let ws = state.resolve_or_create_workspace("dev").unwrap();
        assert_eq!(state.resolve_workspace(&ws.to_string()).unwrap(), ws);
        assert!(state.resolve_workspace("other").is_err());
        assert!(state.resolve_workspace("4").is_err());
    }

    #[test]
    fn workspace_info_unknown_errors() {
        let state = State::new();
        assert!(state.workspace_info(Uuid::new_v4()).is_err());
    }

    // -- client-panes ---------------------------------------------------

    #[test]
    fn client_spawn_without_split_creates_sole_leaf() {
        let mut state = State::new();
        let ws = state.resolve_or_create_workspace("1").unwrap();
        let pane = state.client_spawn(ws, None, None, None).unwrap();
        let tree = state.workspace_info(ws).unwrap().tree.unwrap();
        assert_eq!(tree.leaves().len(), 1);
        assert_eq!(tree.find(pane).unwrap().active_bound(), None);
    }

    #[test]
    fn client_spawn_without_split_into_occupied_workspace_errors() {
        let mut state = State::new();
        let ws = state.resolve_or_create_workspace("1").unwrap();
        state.client_spawn(ws, None, None, None).unwrap();
        let err = state.client_spawn(ws, None, None, None).unwrap_err();
        assert!(err.to_string().contains("already has panes"), "{err}");
        assert_eq!(
            state.workspace_info(ws).unwrap().tree.unwrap().leaves().len(),
            1
        );
    }

    #[test]
    fn client_spawn_splits_existing_leaf_vertically_by_default() {
        let mut state = State::new();
        let ws = state.resolve_or_create_workspace("1").unwrap();
        let first = state.client_spawn(ws, None, None, None).unwrap();
        let second = state.client_spawn(ws, Some(first), None, None).unwrap();
        match state.workspace_info(ws).unwrap().tree.unwrap() {
            SplitTree::Split { dir, ratio, .. } => {
                assert_eq!(dir, SplitDir::Vertical);
                assert_eq!(ratio, 0.5);
            }
            other => panic!("expected a split, got {other:?}"),
        }
        let tree = state.workspace_info(ws).unwrap().tree.unwrap();
        assert!(tree.find(first).is_some());
        assert!(tree.find(second).is_some());
    }

    #[test]
    fn client_spawn_honors_explicit_split_direction() {
        let mut state = State::new();
        let ws = state.resolve_or_create_workspace("1").unwrap();
        let first = state.client_spawn(ws, None, None, None).unwrap();
        state
            .client_spawn(ws, Some(first), Some(SplitDir::Horizontal), None)
            .unwrap();
        match state.workspace_info(ws).unwrap().tree.unwrap() {
            SplitTree::Split { dir, .. } => assert_eq!(dir, SplitDir::Horizontal),
            other => panic!("expected a split, got {other:?}"),
        }
    }

    #[test]
    fn resize_split_updates_ratio_and_rejects_unknown_workspace_or_split() {
        let mut state = State::new();
        let ws = state.resolve_or_create_workspace("1").unwrap();
        let first = state.client_spawn(ws, None, None, None).unwrap();
        state.client_spawn(ws, Some(first), Some(SplitDir::Vertical), None).unwrap();
        let split_id = match state.workspace_info(ws).unwrap().tree.unwrap() {
            SplitTree::Split { id, .. } => id,
            other => panic!("expected a split, got {other:?}"),
        };

        state.resize_split(ws, split_id, 0.3).unwrap();
        match state.workspace_info(ws).unwrap().tree.unwrap() {
            SplitTree::Split { ratio, .. } => assert_eq!(ratio, 0.3),
            other => panic!("expected a split, got {other:?}"),
        }

        assert!(state.resize_split(Uuid::new_v4(), split_id, 0.5).is_err());
        assert!(state.resize_split(ws, Uuid::new_v4(), 0.5).is_err());
    }

    #[test]
    fn client_spawn_with_unknown_split_target_leaves_tree_untouched() {
        let mut state = State::new();
        let ws = state.resolve_or_create_workspace("1").unwrap();
        let first = state.client_spawn(ws, None, None, None).unwrap();
        let before = state.workspace_info(ws).unwrap().tree;
        assert!(state.client_spawn(ws, Some(Uuid::new_v4()), None, None).is_err());
        assert_eq!(state.workspace_info(ws).unwrap().tree, before);
        assert!(state
            .workspace_info(ws)
            .unwrap()
            .tree
            .unwrap()
            .find(first)
            .is_some());
    }

    #[test]
    fn client_spawn_with_split_into_empty_workspace_errors() {
        let mut state = State::new();
        let ws = state.resolve_or_create_workspace("1").unwrap();
        let err = state
            .client_spawn(ws, Some(Uuid::new_v4()), None, None)
            .unwrap_err();
        assert!(err.to_string().contains("no panes to split"), "{err}");
    }

    #[test]
    fn client_spawn_with_unknown_bind_adds_nothing() {
        let mut state = State::new();
        let ws = state.resolve_or_create_workspace("1").unwrap();
        let err = state
            .client_spawn(ws, None, None, Some(Uuid::new_v4()))
            .unwrap_err();
        assert!(err.to_string().contains("unknown server-pane"), "{err}");
        assert_eq!(state.workspace_info(ws).unwrap().tree, None);
    }

    #[test]
    fn client_spawn_into_unknown_workspace_errors() {
        let mut state = State::new();
        assert!(state.client_spawn(Uuid::new_v4(), None, None, None).is_err());
    }

    #[test]
    fn client_close_collapses_split_and_then_empties_workspace() {
        let mut state = State::new();
        let ws = state.resolve_or_create_workspace("1").unwrap();
        let first = state.client_spawn(ws, None, None, None).unwrap();
        let second = state.client_spawn(ws, Some(first), None, None).unwrap();

        state.client_close(ws, first).unwrap();
        match state.workspace_info(ws).unwrap().tree.unwrap() {
            SplitTree::Leaf(pane) => assert_eq!(pane.id, second),
            other => panic!("expected the surviving leaf, got {other:?}"),
        }

        state.client_close(ws, second).unwrap();
        assert_eq!(state.workspace_info(ws).unwrap().tree, None);
    }

    #[test]
    fn client_close_leaves_bound_server_pane_running() {
        let mut state = State::new();
        let sp = spawn_pane(&mut state, "shell");
        let (ws, pane) = workspace_with_bound_pane(&mut state, "1", sp);
        state.client_close(ws, pane).unwrap();
        assert_eq!(
            state.server_pane(sp).unwrap().status(),
            ServerPaneStatus::Running
        );
    }

    #[test]
    fn client_close_unknown_pane_or_empty_workspace_errors() {
        let mut state = State::new();
        let ws = state.resolve_or_create_workspace("1").unwrap();
        assert!(state.client_close(ws, Uuid::new_v4()).is_err());
        let pane = state.client_spawn(ws, None, None, None).unwrap();
        assert!(state.client_close(ws, Uuid::new_v4()).is_err());
        assert!(state.client_close(Uuid::new_v4(), pane).is_err());
        // The failed closes left the layout intact.
        assert!(state
            .workspace_info(ws)
            .unwrap()
            .tree
            .unwrap()
            .find(pane)
            .is_some());
    }

    #[test]
    fn client_rename_sets_name() {
        let mut state = State::new();
        let ws = state.resolve_or_create_workspace("1").unwrap();
        let pane = state.client_spawn(ws, None, None, None).unwrap();
        state.client_rename(ws, pane, "editor".to_string()).unwrap();
        let tree = state.workspace_info(ws).unwrap().tree.unwrap();
        assert_eq!(tree.find(pane).unwrap().name.as_deref(), Some("editor"));
    }

    #[test]
    fn client_rename_unknown_errors() {
        let mut state = State::new();
        let ws = state.resolve_or_create_workspace("1").unwrap();
        assert!(state
            .client_rename(ws, Uuid::new_v4(), "x".to_string())
            .is_err());
    }

    #[test]
    fn client_bind_rebinds_and_rejects_unknown_targets() {
        let mut state = State::new();
        let first = spawn_pane(&mut state, "a");
        let second = spawn_pane(&mut state, "b");
        let (ws, pane) = workspace_with_bound_pane(&mut state, "1", first);

        state.client_bind(ws, pane, second).unwrap();
        let tree = state.workspace_info(ws).unwrap().tree.unwrap();
        assert_eq!(tree.find(pane).unwrap().active_bound(), Some(second));

        assert!(state.client_bind(ws, pane, Uuid::new_v4()).is_err());
        assert!(state.client_bind(ws, Uuid::new_v4(), second).is_err());
        let tree = state.workspace_info(ws).unwrap().tree.unwrap();
        assert_eq!(tree.find(pane).unwrap().active_bound(), Some(second));
    }

    #[test]
    fn client_unbind_detaches_and_leaves_server_pane_running() {
        let mut state = State::new();
        let server = spawn_pane(&mut state, "a");
        let (ws, pane) = workspace_with_bound_pane(&mut state, "1", server);

        state.client_unbind(ws, pane).unwrap();
        let tree = state.workspace_info(ws).unwrap().tree.unwrap();
        assert_eq!(tree.find(pane).unwrap().active_bound(), None);
        // Detaching isn't killing: the server-pane is still in the pool.
        assert!(state.server_list().iter().any(|p| p.id == server));

        // Unbinding an already-unbound pane is a no-op, not an error.
        state.client_unbind(ws, pane).unwrap();

        assert!(state.client_unbind(ws, Uuid::new_v4()).is_err());
        assert!(state.client_unbind(Uuid::new_v4(), pane).is_err());
    }

    // -- client-pane tabs ------------------------------------------------

    /// The leaf `pane` currently is, cloned so assertions don't hold a
    /// borrow of `state`.
    fn leaf_of(state: &State, workspace: WorkspaceId, pane: ClientPaneId) -> ClientPane {
        state
            .workspace_info(workspace)
            .unwrap()
            .tree
            .unwrap()
            .find(pane)
            .unwrap()
            .clone()
    }

    #[test]
    fn client_add_tab_appends_and_activates() {
        let mut state = State::new();
        let sp1 = spawn_pane(&mut state, "a");
        let sp2 = spawn_pane(&mut state, "b");
        let (ws, pane) = workspace_with_bound_pane(&mut state, "1", sp1);

        state.client_add_tab(ws, pane, sp2).unwrap();

        let leaf = leaf_of(&state, ws, pane);
        assert_eq!(leaf.tabs, vec![sp1, sp2]);
        assert_eq!(leaf.active_tab, 1);
    }

    #[test]
    fn client_add_tab_unknown_target_errors() {
        let mut state = State::new();
        let sp = spawn_pane(&mut state, "a");
        let (ws, pane) = workspace_with_bound_pane(&mut state, "1", sp);

        assert!(state.client_add_tab(ws, pane, Uuid::new_v4()).is_err());
        assert!(state.client_add_tab(ws, Uuid::new_v4(), sp).is_err());
        // Rejected adds leave the tab list untouched.
        assert_eq!(leaf_of(&state, ws, pane).tabs, vec![sp]);
    }

    #[test]
    fn client_bind_replaces_only_the_active_tab() {
        let mut state = State::new();
        let sp1 = spawn_pane(&mut state, "a");
        let sp2 = spawn_pane(&mut state, "b");
        let sp3 = spawn_pane(&mut state, "c");
        let (ws, pane) = workspace_with_bound_pane(&mut state, "1", sp1);
        state.client_add_tab(ws, pane, sp2).unwrap();

        // active_tab is 1 (sp2); binding swaps that slot, not sp1's.
        state.client_bind(ws, pane, sp3).unwrap();

        let leaf = leaf_of(&state, ws, pane);
        assert_eq!(leaf.tabs, vec![sp1, sp3]);
        assert_eq!(leaf.active_tab, 1);
    }

    #[test]
    fn client_unbind_drops_only_the_active_tab_when_others_remain() {
        let mut state = State::new();
        let sp1 = spawn_pane(&mut state, "a");
        let sp2 = spawn_pane(&mut state, "b");
        let (ws, pane) = workspace_with_bound_pane(&mut state, "1", sp1);
        state.client_add_tab(ws, pane, sp2).unwrap();

        state.client_unbind(ws, pane).unwrap();

        let leaf = leaf_of(&state, ws, pane);
        assert_eq!(leaf.tabs, vec![sp1]);
        assert_eq!(leaf.active_tab, 0);
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
        assert_eq!(leaf_of(&state, ws, pane).active_tab, 2);

        // Forward off the end wraps to the first tab...
        state.client_cycle_tab(ws, pane, true).unwrap();
        assert_eq!(leaf_of(&state, ws, pane).active_tab, 0);
        // ...and backward off the front wraps to the last.
        state.client_cycle_tab(ws, pane, false).unwrap();
        assert_eq!(leaf_of(&state, ws, pane).active_tab, 2);
    }

    #[test]
    fn client_cycle_tab_noop_on_single_tab() {
        let mut state = State::new();
        let sp = spawn_pane(&mut state, "a");
        let (ws, pane) = workspace_with_bound_pane(&mut state, "1", sp);

        state.client_cycle_tab(ws, pane, true).unwrap();

        assert_eq!(leaf_of(&state, ws, pane).active_tab, 0);
        assert!(state.client_cycle_tab(ws, Uuid::new_v4(), true).is_err());
    }

    #[test]
    fn client_close_tab_removes_active_and_clamps() {
        let mut state = State::new();
        let sp1 = spawn_pane(&mut state, "a");
        let sp2 = spawn_pane(&mut state, "b");
        let (ws, pane) = workspace_with_bound_pane(&mut state, "1", sp1);
        state.client_add_tab(ws, pane, sp2).unwrap();

        // active_tab is the last tab (sp2), so closing it must clamp.
        assert_eq!(
            state.client_close_tab(ws, pane).unwrap(),
            CloseTabResult::TabRemoved
        );

        let leaf = leaf_of(&state, ws, pane);
        assert_eq!(leaf.tabs, vec![sp1]);
        assert_eq!(leaf.active_tab, 0);
        // Closing a tab isn't killing: sp2 keeps running.
        assert!(state.server_list().iter().any(|p| p.id == sp2));
    }

    #[test]
    fn client_close_tab_last_tab_closes_the_leaf() {
        let mut state = State::new();
        let sp = spawn_pane(&mut state, "a");
        let (ws, pane) = workspace_with_bound_pane(&mut state, "1", sp);

        assert_eq!(
            state.client_close_tab(ws, pane).unwrap(),
            CloseTabResult::LeafClosed
        );

        assert_eq!(state.workspace_info(ws).unwrap().tree, None);
    }

    /// Closing a leaf's last tab must still recompute the freed
    /// server-pane's PTY size for its *other* viewers -- the `LeafClosed`
    /// path removes the tab from `leaf.tabs` before delegating to
    /// `client_close`, so `client_close`'s own resize (which reads the
    /// leaf's now-empty `active_bound()`) sees nothing to resize; this
    /// regresses unless the removed pane's size is recomputed explicitly.
    #[test]
    fn client_close_tab_last_tab_recomputes_pty_size_for_remaining_viewers() {
        let mut state = State::new();
        let shared = spawn_pane(&mut state, "shared");
        let (ws_a, small) = workspace_with_bound_pane(&mut state, "1", shared);
        let (ws_b, _big) = workspace_with_bound_pane(&mut state, "2", shared);
        state.subscribe(1, ws_a);
        state.subscribe(2, ws_b);
        state.resize_client_pane(small, size(10, 20));
        let big_pane = state.workspace_info(ws_b).unwrap().tree.unwrap().leaves()[0].id;
        state.resize_client_pane(big_pane, size(40, 100));
        assert_eq!(pane_size(&state, shared), size(10, 20), "smallest viewer should win before closing");

        assert_eq!(state.client_close_tab(ws_a, small).unwrap(), CloseTabResult::LeafClosed);

        assert_eq!(
            pane_size(&state, shared),
            size(40, 100),
            "closing the small viewer's only tab should hand the PTY back to the remaining viewer"
        );
    }

    #[test]
    fn client_close_tab_on_unbound_pane_closes_the_leaf() {
        let mut state = State::new();
        let ws = state.resolve_or_create_workspace("1").unwrap();
        let pane = state.client_spawn(ws, None, None, None).unwrap();

        assert_eq!(
            state.client_close_tab(ws, pane).unwrap(),
            CloseTabResult::LeafClosed
        );

        assert_eq!(state.workspace_info(ws).unwrap().tree, None);
    }

    #[test]
    fn unbind_all_removes_killed_server_pane_from_background_tabs() {
        let mut state = State::new();
        let sp1 = spawn_pane(&mut state, "a");
        let sp2 = spawn_pane(&mut state, "b");
        let (ws, pane) = workspace_with_bound_pane(&mut state, "1", sp1);
        state.client_add_tab(ws, pane, sp2).unwrap();

        // active_tab is 1 (sp2); sp1 is a *background* tab.
        state.server_kill("a").unwrap();

        let leaf = leaf_of(&state, ws, pane);
        assert_eq!(leaf.tabs, vec![sp2]);
        assert_eq!(leaf.active_tab, 0);
    }

    /// A background tab *before* the active one is removed: `active_tab`
    /// must shift down to keep pointing at the same server-pane, not just
    /// get clamped (which happens to be a no-op here since the index stays
    /// in range after the shift -- the exact case `..from_background_tabs`
    /// above can't catch, since there the active tab is last).
    #[test]
    fn unbind_all_removes_a_background_tab_before_the_active_one_without_shifting_the_active_pane() {
        let mut state = State::new();
        let sp1 = spawn_pane(&mut state, "a");
        let sp2 = spawn_pane(&mut state, "b");
        let sp3 = spawn_pane(&mut state, "c");
        let (ws, pane) = workspace_with_bound_pane(&mut state, "1", sp1);
        state.client_add_tab(ws, pane, sp2).unwrap();
        state.client_add_tab(ws, pane, sp3).unwrap();
        // active_tab is 2 (sp3); step back to sp2 so the killed pane (sp1)
        // sits *before* the active index.
        state.client_cycle_tab(ws, pane, false).unwrap();

        state.server_kill("a").unwrap();

        let leaf = leaf_of(&state, ws, pane);
        assert_eq!(leaf.tabs, vec![sp2, sp3]);
        assert_eq!(leaf.active_bound(), Some(sp2), "killing a background tab must not change which tab is displayed");
    }

    #[test]
    fn client_list_all_workspaces_or_one() {
        let mut state = State::new();
        let ws1 = state.resolve_or_create_workspace("1").unwrap();
        let ws2 = state.resolve_or_create_workspace("2").unwrap();
        let a = state.client_spawn(ws1, None, None, None).unwrap();
        let b = state.client_spawn(ws2, None, None, None).unwrap();

        let all = state.client_list(None);
        assert_eq!(
            all.iter().map(|(ws, p)| (*ws, p.id)).collect::<Vec<_>>(),
            vec![(ws1, a), (ws2, b)]
        );

        let only = state.client_list(Some(ws2));
        assert_eq!(
            only.iter().map(|(ws, p)| (*ws, p.id)).collect::<Vec<_>>(),
            vec![(ws2, b)]
        );

        assert!(state.client_list(Some(Uuid::new_v4())).is_empty());
    }

    // -- subscription ---------------------------------------------------

    #[test]
    fn subscribe_is_idempotent_and_unsubscribe_removes() {
        let mut state = State::new();
        let ws = state.resolve_or_create_workspace("1").unwrap();
        state.subscribe(7, ws);
        state.subscribe(7, ws);
        assert_eq!(state.subscribers_for_workspace(ws), vec![7]);

        state.subscribe(8, ws);
        assert_eq!(state.subscribers_for_workspace(ws), vec![7, 8]);

        state.unsubscribe(7, ws);
        assert_eq!(state.subscribers_for_workspace(ws), vec![8]);
        state.unsubscribe(8, ws);
        assert!(state.subscribers_for_workspace(ws).is_empty());
        // Unsubscribing a subscriber that isn't there is a no-op, since
        // the disconnect path can't tell whether Unsubscribe was sent.
        state.unsubscribe(8, ws);
        assert!(state.subscribers_for_workspace(ws).is_empty());
    }

    #[test]
    fn subscribers_for_server_pane_spans_workspaces_without_duplicates() {
        let mut state = State::new();
        let sp = spawn_pane(&mut state, "shared");
        let other = spawn_pane(&mut state, "lonely");
        let (ws_a, _) = workspace_with_bound_pane(&mut state, "1", sp);
        let (ws_b, _) = workspace_with_bound_pane(&mut state, "2", sp);

        state.subscribe(1, ws_a);
        state.subscribe(2, ws_b);
        // Subscriber 1 views both workspaces binding `sp`, but should be
        // pushed to once.
        state.subscribe(1, ws_b);

        assert_eq!(state.subscribers_for_server_pane(sp), vec![1, 2]);
        assert!(state.subscribers_for_server_pane(other).is_empty());
    }

    #[test]
    fn subscribers_for_server_pane_ignores_unbound_layouts() {
        let mut state = State::new();
        let sp = spawn_pane(&mut state, "shell");
        let ws = state.resolve_or_create_workspace("1").unwrap();
        state.client_spawn(ws, None, None, None).unwrap();
        state.subscribe(1, ws);
        assert!(state.subscribers_for_server_pane(sp).is_empty());
    }

    // -- PTY sizing -----------------------------------------------------

    #[test]
    fn pty_size_is_smallest_viewer_dimension_wise() {
        let mut state = State::new();
        let sp = spawn_pane(&mut state, "shared");
        let (ws_a, pane_a) = workspace_with_bound_pane(&mut state, "1", sp);
        let (ws_b, pane_b) = workspace_with_bound_pane(&mut state, "2", sp);

        state.subscribe(1, ws_a);
        state.subscribe(2, ws_b);
        // Neither dimension comes from a single viewer: rows from B,
        // cols from A.
        state.resize_client_pane(pane_a, size(30, 60));
        state.resize_client_pane(pane_b, size(20, 100));

        assert_eq!(pane_size(&state, sp), size(20, 60));
    }

    #[test]
    fn pty_size_ignores_client_panes_nobody_is_viewing() {
        let mut state = State::new();
        let sp = spawn_pane(&mut state, "shared");
        let (ws_a, pane_a) = workspace_with_bound_pane(&mut state, "1", sp);
        let (_ws_b, pane_b) = workspace_with_bound_pane(&mut state, "2", sp);

        state.subscribe(1, ws_a);
        state.resize_client_pane(pane_a, size(30, 60));
        // `ws_b` has no subscriber, so its (smaller) pane must not shrink
        // the PTY.
        state.resize_client_pane(pane_b, size(5, 5));

        assert_eq!(pane_size(&state, sp), size(30, 60));
    }

    #[test]
    fn subscribing_and_unsubscribing_recomputes_pty_size() {
        let mut state = State::new();
        let sp = spawn_pane(&mut state, "shared");
        let (ws_a, pane_a) = workspace_with_bound_pane(&mut state, "1", sp);
        let (ws_b, pane_b) = workspace_with_bound_pane(&mut state, "2", sp);
        state.resize_client_pane(pane_a, size(30, 60));
        state.resize_client_pane(pane_b, size(10, 20));

        state.subscribe(1, ws_a);
        assert_eq!(pane_size(&state, sp), size(30, 60));

        // A second, smaller viewer appears.
        state.subscribe(2, ws_b);
        assert_eq!(pane_size(&state, sp), size(10, 20));

        // It leaves again: the remaining viewer is now the smallest.
        state.unsubscribe(2, ws_b);
        assert_eq!(pane_size(&state, sp), size(30, 60));
    }

    #[test]
    fn pty_keeps_last_size_when_the_last_viewer_leaves() {
        let mut state = State::new();
        let sp = spawn_pane(&mut state, "shell");
        let (ws, pane) = workspace_with_bound_pane(&mut state, "1", sp);
        state.subscribe(1, ws);
        state.resize_client_pane(pane, size(12, 40));
        assert_eq!(pane_size(&state, sp), size(12, 40));

        state.unsubscribe(1, ws);
        assert_eq!(pane_size(&state, sp), size(12, 40));
    }

    #[test]
    fn binding_and_closing_recompute_pty_size() {
        let mut state = State::new();
        let sp = spawn_pane(&mut state, "shared");
        let ws = state.resolve_or_create_workspace("1").unwrap();
        let first = state.client_spawn(ws, None, None, Some(sp)).unwrap();
        let second = state.client_spawn(ws, Some(first), None, None).unwrap();
        state.subscribe(1, ws);
        state.resize_client_pane(first, size(30, 60));
        state.resize_client_pane(second, size(10, 20));
        assert_eq!(pane_size(&state, sp), size(30, 60));

        // A second, smaller client-pane starts displaying the same
        // server-pane.
        state.client_bind(ws, second, sp).unwrap();
        assert_eq!(pane_size(&state, sp), size(10, 20));

        // Closing it hands the PTY back to the remaining viewer.
        state.client_close(ws, second).unwrap();
        assert_eq!(pane_size(&state, sp), size(30, 60));
    }

    #[test]
    fn resize_of_an_unbound_client_pane_is_recorded_for_a_later_bind() {
        let mut state = State::new();
        let sp = spawn_pane(&mut state, "shell");
        let ws = state.resolve_or_create_workspace("1").unwrap();
        let pane = state.client_spawn(ws, None, None, None).unwrap();
        state.subscribe(1, ws);
        state.resize_client_pane(pane, size(8, 30));
        assert_eq!(pane_size(&state, sp), DEFAULT_PTY_SIZE);

        state.client_bind(ws, pane, sp).unwrap();
        assert_eq!(pane_size(&state, sp), size(8, 30));
    }

    // -- scroll offsets ---------------------------------------------------

    #[test]
    fn scroll_server_pane_clamps_at_zero() {
        let mut state = State::new();
        let info = state.server_spawn(None, Some("cat".to_string()), None).unwrap();
        let offset = state.scroll_server_pane(1, info.id, 5);
        assert_eq!(offset, 0);
        assert_eq!(state.scroll_offsets.get(&(1, info.id)), None);
    }

    #[test]
    fn scroll_server_pane_absent_entry_defaults_to_zero_before_first_call() {
        let mut state = State::new();
        let info = state.server_spawn(None, Some("cat".to_string()), None).unwrap();
        let offset = state.scroll_server_pane(1, info.id, -5);
        assert_eq!(offset, 0);
    }

    // -- pane-event plumbing --------------------------------------------

    #[test]
    fn pane_events_can_be_claimed_exactly_once() {
        let mut state = State::new();
        assert!(state.take_pane_events().is_some());
        assert!(state.take_pane_events().is_none());
    }

    #[test]
    fn spawned_pane_output_reaches_the_claimed_event_stream() {
        let mut state = State::new();
        let mut events = state.take_pane_events().unwrap();
        let info = state
            .server_spawn(Some("greeter".to_string()), Some("printf hi".to_string()), None)
            .unwrap();
        // `blocking_recv` outside a runtime is explicitly supported by
        // tokio, so this stays a plain synchronous unit test.
        let event = events.blocking_recv().expect("pane reader thread sent nothing");
        match event {
            ServerPaneEvent::Changed(id) | ServerPaneEvent::Died(id) => assert_eq!(id, info.id),
        }
    }
}
