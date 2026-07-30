//! Pure daemon state: the server-pane pool and the workspace/split-tree
//! pool, plus the subscriber bookkeeping needed for event broadcast and
//! smallest-viewer-wins PTY sizing.
//!
//! Deliberately free of tokio/socket types so it can be unit tested as
//! plain data-structure logic (see design doc "Testing" — unit tests for
//! `SplitTree` operations belong here, exercised through `State`'s
//! workspace methods rather than directly, since `State` is what owns
//! workspace lookup-by-name-or-id).

use crate::protocol::{
    ClientPane, ClientPaneId, ServerPaneId, ServerPaneInfo, Size, SplitDir, SplitTree,
    WorkspaceId, WorkspaceInfo,
};
use crate::term::ServerPane;
use std::collections::HashMap;

/// One live subscriber: a connection that has `Subscribe`d to a
/// workspace. Identified by an opaque id the connection handler mints
/// (e.g. a per-connection counter or the `UnixStream`'s file descriptor);
/// `State` itself never needs to know what it means, only that it's a
/// stable key for "this viewer" across subscribe/unsubscribe/broadcast.
pub type SubscriberId = u64;

pub struct State {
    server_panes: HashMap<ServerPaneId, ServerPane>,
    workspaces: HashMap<WorkspaceId, Workspace>,
    /// workspace -> set of subscribers currently viewing it. Drives both
    /// event broadcast and (via each workspace's bound server-panes)
    /// smallest-viewer-wins PTY sizing.
    subscribers: HashMap<WorkspaceId, Vec<SubscriberId>>,
}

struct Workspace {
    info_number: u8,
    name: Option<String>,
    tree: Option<SplitTree>,
}

impl State {
    pub fn new() -> Self {
        Self {
            server_panes: HashMap::new(),
            workspaces: HashMap::new(),
            subscribers: HashMap::new(),
        }
    }

    // -- server-panes --------------------------------------------------

    pub fn server_spawn(&mut self, name: Option<String>, cmd: Option<String>) -> anyhow::Result<ServerPaneInfo> {
        let _ = (name, cmd);
        todo!("mint id, ServerPane::spawn with a default size, insert, return info")
    }

    /// Resolve `target` against both pane name and id-as-string, per the
    /// CLI surface's `<name-or-id>` addressing.
    pub fn resolve_server_pane(&self, target: &str) -> anyhow::Result<ServerPaneId> {
        let _ = target;
        todo!()
    }

    pub fn server_kill(&mut self, target: &str) -> anyhow::Result<()> {
        let _ = target;
        todo!()
    }

    pub fn server_rename(&mut self, target: &str, new_name: String) -> anyhow::Result<()> {
        let _ = (target, new_name);
        todo!()
    }

    pub fn server_list(&self) -> Vec<ServerPaneInfo> {
        todo!()
    }

    // -- workspaces / client-panes --------------------------------------

    /// Resolve `target` against both workspace name and number-as-string
    /// (e.g. `"2"` or `"dev"`), creating an empty workspace on the fly if
    /// `target` is a bare number 1-9 that doesn't exist yet — this is
    /// what makes `cmd-1..9` "create if absent" (design doc, Data model
    /// reference) work uniformly whether triggered by keybind or CLI.
    pub fn resolve_or_create_workspace(&mut self, target: &str) -> anyhow::Result<WorkspaceId> {
        let _ = target;
        todo!()
    }

    pub fn resolve_workspace(&self, target: &str) -> anyhow::Result<WorkspaceId> {
        let _ = target;
        todo!()
    }

    pub fn workspace_info(&self, id: WorkspaceId) -> anyhow::Result<WorkspaceInfo> {
        let _ = id;
        todo!()
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
        let _ = (workspace, split_of, dir, bind);
        todo!()
    }

    pub fn client_close(&mut self, workspace: WorkspaceId, pane: ClientPaneId) -> anyhow::Result<()> {
        let _ = (workspace, pane);
        todo!()
    }

    pub fn client_rename(
        &mut self,
        workspace: WorkspaceId,
        pane: ClientPaneId,
        new_name: String,
    ) -> anyhow::Result<()> {
        let _ = (workspace, pane, new_name);
        todo!()
    }

    pub fn client_bind(
        &mut self,
        workspace: WorkspaceId,
        pane: ClientPaneId,
        target: ServerPaneId,
    ) -> anyhow::Result<()> {
        let _ = (workspace, pane, target);
        todo!()
    }

    pub fn client_list(&self, workspace: Option<WorkspaceId>) -> Vec<(WorkspaceId, ClientPane)> {
        let _ = workspace;
        todo!()
    }

    // -- subscription / PTY sizing --------------------------------------

    pub fn subscribe(&mut self, subscriber: SubscriberId, workspace: WorkspaceId) {
        let _ = (subscriber, workspace);
        todo!("record subscriber, recompute PTY sizes for now-viewed server-panes")
    }

    /// Also used to clean up a connection that dropped without sending
    /// an explicit `Unsubscribe` (design doc doesn't distinguish the two
    /// cases — see `daemon::handle_connection`'s post-loop cleanup).
    pub fn unsubscribe(&mut self, subscriber: SubscriberId, workspace: WorkspaceId) {
        let _ = (subscriber, workspace);
        todo!("drop subscriber, recompute PTY sizes for now-unviewed server-panes")
    }

    /// Recompute and apply smallest-viewer-wins sizing (design doc "PTY
    /// sizing") for one server-pane, given the current on-screen size of
    /// every client-pane bound to it across every workspace a subscriber
    /// is currently viewing.
    pub fn resize_client_pane(&mut self, pane: ClientPaneId, size: Size) {
        let _ = (pane, size);
        todo!()
    }

    // -- broadcast fan-out (used by daemon::dispatch, kept out of State so
    //    State never performs I/O — see module doc comment) -------------

    /// Every subscriber currently viewing `workspace`. `daemon::dispatch`
    /// calls this after a layout mutation succeeds to know which
    /// connections to push a `LayoutDelta` to.
    pub fn subscribers_for_workspace(&self, workspace: WorkspaceId) -> Vec<SubscriberId> {
        let _ = workspace;
        todo!()
    }

    /// Every subscriber viewing any workspace that currently binds
    /// `server_pane`. `daemon::dispatch` calls this after PTY output
    /// changes (or the pane dies) to know which connections to push a
    /// `GridDelta`/`ServerPaneDied` to, and it's also the input to
    /// smallest-viewer-wins PTY sizing.
    pub fn subscribers_for_server_pane(&self, server_pane: ServerPaneId) -> Vec<SubscriberId> {
        let _ = server_pane;
        todo!()
    }
}
