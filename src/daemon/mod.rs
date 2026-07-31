//! The dimux daemon: owns the global server-pane pool and the global
//! workspace/split-tree state, and serves both over the Unix socket
//! defined in [`crate::protocol`].
//!
//! Module split:
//! - `state` — pure, non-async data structures and mutation logic for
//!   server-panes and workspaces (testable without tokio or a socket).
//! - `mod.rs` (here) — the async socket listener and per-connection
//!   request/subscription handling that drives `state`.
//!
//! Concurrency model: a single `tokio::sync::Mutex<State>` guards all
//! daemon state. Every request handler locks it for the duration of one
//! request; this is fine at the pane counts dimux targets (see design doc
//! non-goals) and keeps the logic in `state` free of async concerns.

pub mod state;

use crate::protocol::{self, ClientPane, Event, Request, Response, ServerMessage, ServerPaneId, WorkspaceId};
use crate::term::ServerPaneEvent;
use state::{State, SubscriberId};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::UnixListener;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Mutex;
use uuid::Uuid;

/// Maps each connection's [`SubscriberId`] to a channel that pushes
/// `ServerMessage`s onto its socket-writer task. `State` only tracks
/// *which* subscriber ids are viewing what (see
/// `subscribers_for_workspace`/`subscribers_for_server_pane`) — it never
/// touches a socket. This registry is the thing that turns "subscriber
/// id 7" back into an actual `Event` delivery, kept here rather than in
/// `state` to keep `state` I/O-free (module doc comment).
type SubscriberRegistry = Arc<Mutex<HashMap<SubscriberId, UnboundedSender<ServerMessage>>>>;

/// Handle to a running daemon, returned by [`run`] for tests that want to
/// drive the daemon in-process (see design doc "Testing" — integration
/// tests spin this up against a temp socket path rather than exec'ing a
/// real subprocess).
pub struct Daemon {
    pub socket_path: std::path::PathBuf,
}

/// Bind `socket_path` and serve forever (or until the process is killed).
/// Used both by `dimux attach`'s auto-spawn path and directly by
/// integration tests with a temp path.
pub async fn run(socket_path: std::path::PathBuf) -> anyhow::Result<Daemon> {
    let listener = UnixListener::bind(&socket_path)?;
    let mut inner_state = State::new();
    // Claim the pane-event stream exactly once, right here at start-up
    // (module doc / `state::State::take_pane_events` doc comment) — this
    // is the *only* caller, so `expect` is warranted: a `None` here would
    // mean this function ran twice against the same `State`, which can't
    // happen since `State::new()` is constructed fresh above.
    let mut pane_events = inner_state
        .take_pane_events()
        .expect("freshly constructed State always has its pane-event receiver available");
    let state = Arc::new(Mutex::new(inner_state));
    let registry: SubscriberRegistry = Arc::new(Mutex::new(HashMap::new()));
    let next_subscriber_id = Arc::new(std::sync::atomic::AtomicU64::new(0));

    // Drains every ServerPane's background reader thread output (via the
    // one `pane_events` channel the daemon claims at start-up) and turns
    // each `Changed`/`Died` notification into a `GridDelta` push to every
    // subscriber currently viewing that server-pane. Without this task,
    // PTY output would update the grid in `state` but no frontend would
    // ever hear about it outside of a fresh `Subscribe`.
    let drain_state = state.clone();
    let drain_registry = registry.clone();
    tokio::spawn(async move {
        while let Some(event) = pane_events.recv().await {
            let id = match event {
                ServerPaneEvent::Changed(id) | ServerPaneEvent::Died(id) => id,
            };
            let state = drain_state.lock().await;
            broadcast_grid(&state, &drain_registry, id).await;
        }
    });

    let path_for_task = socket_path.clone();
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let state = state.clone();
                    let registry = registry.clone();
                    let id = next_subscriber_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    tokio::spawn(handle_connection(stream, state, registry, id));
                }
                Err(err) => {
                    tracing_lite_log(&format!("accept error on {path_for_task:?}: {err}"));
                }
            }
        }
    });

    Ok(Daemon { socket_path })
}

/// Auto-spawn the daemon as a detached background process if
/// `socket_path` isn't reachable, per design doc "Error handling". Used
/// by `dimux attach` and every CLI subcommand before connecting.
///
/// This does a plain detached `std::process::Command::spawn` of `dimux
/// daemon` (current exe re-exec, per `main.rs`'s `Cli::Daemon` arm) with
/// stdio wired to `/dev/null`, rather than a true double-fork via a
/// process-control crate. On Unix, a child spawned this way is already
/// reparented to init (or the nearest subreaper) once this process exits
/// — it never depended on this process's session/job-control the way a
/// shell-launched background job would — so a real double-fork buys
/// nothing extra here and would need a new dependency (`libc`/`nix`) this
/// crate doesn't have budget for. Known limitation: unlike a true
/// double-fork, the child stays in this process's process group until it
/// exits, so a signal sent to the whole group (e.g. a shell's Ctrl-C to
/// its foreground job) could reach the daemon too; acceptable for v1.
pub fn ensure_running(socket_path: &std::path::Path) -> anyhow::Result<()> {
    use std::process::Stdio;

    let exe = std::env::current_exe()?;
    std::process::Command::new(exe)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    // The caller (`Client::connect`) immediately retries the connection
    // once ensure_running returns, so poll briefly rather than making that
    // retry racy against however long the daemon takes to bind its
    // listener.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if std::os::unix::net::UnixStream::connect(socket_path).is_ok() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    anyhow::bail!("daemon did not become reachable at {socket_path:?} within 2s of spawning it")
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    state: Arc<Mutex<State>>,
    registry: SubscriberRegistry,
    subscriber_id: SubscriberId,
) {
    let (mut reader, writer) = stream.into_split();
    let mut subscribed_workspace: Option<protocol::WorkspaceId> = None;

    // Events pushed to this connection (via `registry`) are interleaved
    // onto the same writer as request/response frames — a subscribed
    // connection can receive either at any time (protocol.rs
    // `ServerMessage`). `push_rx` is drained by a second task below.
    let (push_tx, mut push_rx) = tokio::sync::mpsc::unbounded_channel::<ServerMessage>();
    registry.lock().await.insert(subscriber_id, push_tx);

    let writer = Arc::new(Mutex::new(writer));
    let push_writer = writer.clone();
    let push_task = tokio::spawn(async move {
        while let Some(msg) = push_rx.recv().await {
            if protocol::framing::write_frame(&mut *push_writer.lock().await, &msg)
                .await
                .is_err()
            {
                break;
            }
        }
    });

    loop {
        let request: Request = match protocol::framing::read_frame(&mut reader).await {
            Ok(r) => r,
            Err(_) => break, // connection closed
        };

        let response = dispatch(
            &state,
            &registry,
            subscriber_id,
            &mut subscribed_workspace,
            request,
        )
        .await;
        if protocol::framing::write_frame(&mut *writer.lock().await, &ServerMessage::Response(response))
            .await
            .is_err()
        {
            break;
        }
    }

    push_task.abort();
    registry.lock().await.remove(&subscriber_id);
    if let Some(ws) = subscribed_workspace {
        state.lock().await.unsubscribe(subscriber_id, ws);
    }
}

/// Apply one request to `state` and produce its response, pushing any
/// resulting `Event`s to other subscribers via `registry`. Split out
/// from `handle_connection` so it can be unit/integration tested without
/// a real socket (call it directly with a `State` and a `Request`).
async fn dispatch(
    state: &Arc<Mutex<State>>,
    registry: &SubscriberRegistry,
    subscriber_id: SubscriberId,
    subscribed_workspace: &mut Option<protocol::WorkspaceId>,
    request: Request,
) -> Response {
    let _ = subscriber_id;
    match request {
        Request::ServerSpawn { name, cmd } => {
            let mut state = state.lock().await;
            ok_or_err(state.server_spawn(name, cmd), Response::ServerPane)
        }

        Request::ServerKill { target } => {
            let mut state = state.lock().await;
            ok_or_err(state.server_kill(&target), |()| Response::Ack)
        }

        Request::ServerRename { target, new_name } => {
            let mut state = state.lock().await;
            ok_or_err(state.server_rename(&target, new_name), |()| Response::Ack)
        }

        Request::ServerList => {
            let state = state.lock().await;
            Response::ServerPaneList(state.server_list())
        }

        Request::ClientSpawn { workspace, split_of, dir, bind } => {
            let mut state = state.lock().await;
            let ws_id = match state.resolve_or_create_workspace(&workspace) {
                Ok(id) => id,
                Err(err) => return Response::Error { message: err.to_string() },
            };
            let resolved_bind = match bind {
                Some(target) => match state.resolve_server_pane(&target) {
                    Ok(id) => Some(id),
                    Err(err) => return Response::Error { message: err.to_string() },
                },
                None => None,
            };
            match state.client_spawn(ws_id, split_of, dir, resolved_bind) {
                Ok(pane) => {
                    broadcast_layout(&state, registry, ws_id).await;
                    Response::ClientPaneCreated { workspace: ws_id, pane }
                }
                Err(err) => Response::Error { message: err.to_string() },
            }
        }

        Request::ClientClose { workspace, pane } => {
            let mut state = state.lock().await;
            let ws_id = match state.resolve_workspace(&workspace) {
                Ok(id) => id,
                Err(err) => return Response::Error { message: err.to_string() },
            };
            match state.client_close(ws_id, pane) {
                Ok(()) => {
                    broadcast_layout(&state, registry, ws_id).await;
                    Response::Ack
                }
                Err(err) => Response::Error { message: err.to_string() },
            }
        }

        Request::ClientRename { workspace, pane, new_name } => {
            let mut state = state.lock().await;
            let ws_id = match state.resolve_workspace(&workspace) {
                Ok(id) => id,
                Err(err) => return Response::Error { message: err.to_string() },
            };
            match state.client_rename(ws_id, pane, new_name) {
                Ok(()) => {
                    broadcast_layout(&state, registry, ws_id).await;
                    Response::Ack
                }
                Err(err) => Response::Error { message: err.to_string() },
            }
        }

        Request::ClientBind { workspace, pane, target } => {
            let mut state = state.lock().await;
            let ws_id = match state.resolve_workspace(&workspace) {
                Ok(id) => id,
                Err(err) => return Response::Error { message: err.to_string() },
            };
            let target_id = match state.resolve_server_pane(&target) {
                Ok(id) => id,
                Err(err) => return Response::Error { message: err.to_string() },
            };
            match state.client_bind(ws_id, pane, target_id) {
                Ok(()) => {
                    broadcast_layout(&state, registry, ws_id).await;
                    Response::Ack
                }
                Err(err) => Response::Error { message: err.to_string() },
            }
        }

        Request::ClientUnbind { workspace, pane } => {
            let mut state = state.lock().await;
            let ws_id = match state.resolve_workspace(&workspace) {
                Ok(id) => id,
                Err(err) => return Response::Error { message: err.to_string() },
            };
            match state.client_unbind(ws_id, pane) {
                Ok(()) => {
                    broadcast_layout(&state, registry, ws_id).await;
                    Response::Ack
                }
                Err(err) => Response::Error { message: err.to_string() },
            }
        }

        Request::ClientList { workspace } => {
            let state = state.lock().await;
            match workspace {
                Some(w) => {
                    let ws_id = match state.resolve_workspace(&w) {
                        Ok(id) => id,
                        Err(err) => return Response::Error { message: err.to_string() },
                    };
                    let panes: Vec<ClientPane> = state
                        .client_list(Some(ws_id))
                        .into_iter()
                        .map(|(_, pane)| pane)
                        .collect();
                    Response::ClientPaneList { workspace: ws_id, panes }
                }
                None => {
                    // PROTOCOL SHAPE MISMATCH (flagged, not fixed here —
                    // `protocol.rs` is out of scope): `client_list(None)`
                    // spans every workspace (one `WorkspaceId` per
                    // returned pane), but `Response::ClientPaneList` has
                    // room for exactly one `WorkspaceId` for the whole
                    // response. There's no faithful way to represent
                    // "these panes belong to N different workspaces" in
                    // this shape. Using `Uuid::nil()` here as a sentinel:
                    // it's the least misleading option (unlike picking an
                    // arbitrary real workspace id, it can't be mistaken
                    // for "all these panes are in workspace X" — real ids
                    // come from `WorkspaceId::new_v4()`, which never
                    // yields nil) but callers that only look at `panes`
                    // and ignore `workspace` are unaffected. `dimux
                    // client ls` (no arg) is exactly this path; a caller
                    // that cares which workspace each pane belongs to
                    // needs a per-workspace `ClientList` call instead.
                    // Revisit by giving `Response::ClientPaneList` a
                    // `Vec<(WorkspaceId, ClientPane)>` shape, matching
                    // `state::client_list`'s own return type.
                    let panes: Vec<ClientPane> = state
                        .client_list(None)
                        .into_iter()
                        .map(|(_, pane)| pane)
                        .collect();
                    Response::ClientPaneList { workspace: Uuid::nil(), panes }
                }
            }
        }

        Request::Subscribe { workspace } => {
            let mut state = state.lock().await;
            let ws_id = match state.resolve_or_create_workspace(&workspace) {
                Ok(id) => id,
                Err(err) => return Response::Error { message: err.to_string() },
            };
            state.subscribe(subscriber_id, ws_id);
            *subscribed_workspace = Some(ws_id);
            let info = match state.workspace_info(ws_id) {
                Ok(info) => info,
                Err(err) => return Response::Error { message: err.to_string() },
            };
            let grids = info
                .tree
                .as_ref()
                .map(|tree| {
                    tree.leaves()
                        .into_iter()
                        .filter_map(|leaf| leaf.bound)
                        .filter_map(|bound| state.server_pane(bound))
                        .map(|pane| pane.snapshot())
                        .collect()
                })
                .unwrap_or_default();
            Response::Snapshot { workspace: info, grids }
        }

        Request::Unsubscribe { workspace } => {
            let mut state = state.lock().await;
            state.unsubscribe(subscriber_id, workspace);
            if *subscribed_workspace == Some(workspace) {
                *subscribed_workspace = None;
            }
            Response::Ack
        }

        Request::ResizeClientPane { pane, size } => {
            let mut state = state.lock().await;
            let affected = state.bound_server_pane(pane);
            state.resize_client_pane(pane, size);
            if let Some(server_pane) = affected {
                broadcast_grid(&state, registry, server_pane).await;
            }
            Response::Ack
        }

        Request::ResizeSplit { workspace, split, new_ratio } => {
            let mut state = state.lock().await;
            let ws_id = match state.resolve_workspace(&workspace) {
                Ok(id) => id,
                Err(err) => return Response::Error { message: err.to_string() },
            };
            match state.resize_split(ws_id, split, new_ratio) {
                Ok(()) => {
                    broadcast_layout(&state, registry, ws_id).await;
                    Response::Ack
                }
                Err(err) => Response::Error { message: err.to_string() },
            }
        }

        Request::Input { pane, bytes } => {
            let state = state.lock().await;
            let Some(server_pane_id) = state.bound_server_pane(pane) else {
                return Response::Error {
                    message: format!("client-pane {pane} is unbound or does not exist"),
                };
            };
            let Some(server_pane) = state.server_pane(server_pane_id) else {
                return Response::Error {
                    message: format!("server-pane {server_pane_id} no longer exists"),
                };
            };
            ok_or_err(server_pane.write_input(&bytes), |()| Response::Ack)
        }
    }
}

/// Convert a `state::State` mutation's `anyhow::Result<T>` into the
/// `Response` a handler should return: `f(value)` on success, or
/// `Response::Error{message}` on failure. Every `Request` arm above that
/// forwards straight to a fallible `State` method goes through this so
/// the Err-to-`Response::Error` mapping only needs writing once.
fn ok_or_err<T>(result: anyhow::Result<T>, f: impl FnOnce(T) -> Response) -> Response {
    match result {
        Ok(value) => f(value),
        Err(err) => Response::Error { message: err.to_string() },
    }
}

/// Push a `LayoutDelta` for `workspace`'s current tree to every connection
/// subscribed to it. Called after any request that mutates a workspace's
/// split tree (spawn/close/rename/bind), per design doc "Protocol" — a
/// subscribed frontend must see these changes live, including ones it
/// triggered itself (the CLI-triggered-change-appears-live design intent).
async fn broadcast_layout(state: &State, registry: &SubscriberRegistry, workspace: WorkspaceId) {
    let Ok(info) = state.workspace_info(workspace) else {
        return;
    };
    let event = ServerMessage::Event(Event::LayoutDelta { workspace, tree: info.tree });
    let subscribers = state.subscribers_for_workspace(workspace);
    push_to_subscribers(registry, &subscribers, &event).await;
}

/// Push a `GridDelta` for `server_pane`'s current snapshot to every
/// connection viewing it (i.e. subscribed to some workspace that binds
/// it). Called both from `dispatch` (`ResizeClientPane`) and from the
/// pane-event drain task in `run` (`ServerPaneEvent::Changed`/`Died`).
async fn broadcast_grid(state: &State, registry: &SubscriberRegistry, server_pane: ServerPaneId) {
    let Some(pane) = state.server_pane(server_pane) else {
        return;
    };
    let event = ServerMessage::Event(Event::GridDelta { snapshot: pane.snapshot() });
    let subscribers = state.subscribers_for_server_pane(server_pane);
    push_to_subscribers(registry, &subscribers, &event).await;
}

async fn push_to_subscribers(
    registry: &SubscriberRegistry,
    subscribers: &[SubscriberId],
    event: &ServerMessage,
) {
    if subscribers.is_empty() {
        return;
    }
    let registry = registry.lock().await;
    for sub in subscribers {
        if let Some(tx) = registry.get(sub) {
            // A closed receiver just means that connection is mid-teardown
            // (its `push_task` already exited); nothing for the sender to
            // do about it, and no other subscriber's delivery should be
            // aborted over it.
            let _ = tx.send(event.clone());
        }
    }
}

fn tracing_lite_log(msg: &str) {
    eprintln!("[dimux-daemon] {msg}");
}

/// Integration tests per design doc "Testing" > Integration tier: the
/// daemon spun up in-process against a temp socket path, driven through
/// raw `Request`/`ServerMessage` frames (no `cli.rs`/`Client` — this crate
/// deliberately doesn't depend on that module, and the point of these
/// tests is to exercise the wire protocol directly).
#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::SplitDir;
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
    use tokio::net::UnixStream;

    /// Owns the socket file path `run()` binds and removes it on drop.
    /// `UnixListener::bind` requires the path not to already exist, and
    /// nothing un-links the file once the daemon's accept-loop task is
    /// gone (dropping a `UnixListener` closes the fd but does not remove
    /// the path from the filesystem).
    struct SocketGuard(PathBuf);

    impl Drop for SocketGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    async fn start_daemon() -> SocketGuard {
        let path = std::env::temp_dir().join(format!("dimux-test-{}.sock", Uuid::new_v4()));
        run(path.clone()).await.expect("daemon should bind and start");
        SocketGuard(path)
    }

    /// A minimal test-local stand-in for `cli::Client`: connects, and lets
    /// a test send `Request`s / read `Response`s / read pushed `Event`s
    /// directly, since `cli.rs` is out of this module's scope to depend
    /// on.
    struct TestConn {
        reader: OwnedReadHalf,
        writer: OwnedWriteHalf,
    }

    impl TestConn {
        async fn connect(path: &Path) -> Self {
            let stream = UnixStream::connect(path)
                .await
                .expect("connect to in-process test daemon");
            let (reader, writer) = stream.into_split();
            Self { reader, writer }
        }

        /// Send one request and return its response. A subscribed
        /// connection can have a pushed `Event` land on the wire ahead of
        /// the response to an unrelated request (e.g. the background
        /// pane-event drain task races the response writer for the same
        /// socket) — exactly the `ServerMessage` interleaving the design
        /// doc's Protocol section calls out, so any real client must skip
        /// over `Event` frames while waiting for its response, same as
        /// here.
        async fn request(&mut self, req: Request) -> Response {
            protocol::framing::write_frame(&mut self.writer, &req)
                .await
                .expect("write request frame");
            loop {
                match protocol::framing::read_frame(&mut self.reader)
                    .await
                    .expect("read response frame")
                {
                    ServerMessage::Response(r) => return r,
                    ServerMessage::Event(_) => continue,
                }
            }
        }

        /// Read one pushed `Event`, or `None` if none arrives within
        /// `timeout`. Used by subscribed connections waiting on a
        /// broadcast triggered by another connection's request.
        async fn read_event(&mut self, timeout: Duration) -> Option<Event> {
            let frame = tokio::time::timeout(
                timeout,
                protocol::framing::read_frame::<_, ServerMessage>(&mut self.reader),
            )
            .await
            .ok()?
            .ok()?;
            match frame {
                ServerMessage::Event(e) => Some(e),
                ServerMessage::Response(r) => panic!("expected Event, got Response: {r:?}"),
            }
        }
    }

    /// Spawn a `printf hello` server-pane, bind it into a fresh workspace,
    /// then `Subscribe` and confirm the returned `Snapshot`'s grid already
    /// shows "hello" — exercising `ServerSpawn` -> `ClientSpawn` ->
    /// `Subscribe` end-to-end over the wire, including the fact that
    /// `Subscribe`'s snapshot is built fresh from live pane state rather
    /// than from a cached broadcast.
    #[tokio::test]
    async fn subscribe_snapshot_reflects_pty_output() {
        let guard = start_daemon().await;
        let mut conn = TestConn::connect(&guard.0).await;

        let server_pane = match conn
            .request(Request::ServerSpawn {
                name: None,
                cmd: Some("printf hello".to_string()),
            })
            .await
        {
            Response::ServerPane(info) => info.id,
            other => panic!("expected ServerPane, got {other:?}"),
        };

        let workspace = match conn
            .request(Request::ClientSpawn {
                workspace: "1".to_string(),
                split_of: None,
                dir: None,
                bind: Some(server_pane.to_string()),
            })
            .await
        {
            Response::ClientPaneCreated { workspace, .. } => workspace,
            other => panic!("expected ClientPaneCreated, got {other:?}"),
        };

        // `printf` writes near-instantly but its background reader thread
        // still runs concurrently with this test, so retry the whole
        // Subscribe request briefly rather than assume the write already
        // landed by the time we ask.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let grids = loop {
            let grids = match conn
                .request(Request::Subscribe {
                    workspace: workspace.to_string(),
                })
                .await
            {
                Response::Snapshot { grids, .. } => grids,
                other => panic!("expected Snapshot, got {other:?}"),
            };
            let found = grids.iter().any(|g| {
                g.lines.iter().any(|row| {
                    row.iter()
                        .map(|c| c.text.as_str())
                        .collect::<String>()
                        .contains("hello")
                })
            });
            if found || std::time::Instant::now() > deadline {
                break grids;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };

        assert_eq!(grids.len(), 1, "expected exactly one grid for the bound server-pane");
        let text: String = grids[0]
            .lines
            .iter()
            .flat_map(|row| row.iter().map(|c| c.text.as_str()))
            .collect();
        assert!(text.contains("hello"), "grid text did not contain \"hello\": {text:?}");
    }

    /// Two connections `Subscribe` to the same workspace; a third,
    /// unsubscribed connection then sends a `ClientSpawn` split targeting
    /// that workspace (simulating a bare CLI command). Both subscribers
    /// must receive the resulting `LayoutDelta` push.
    #[tokio::test]
    async fn client_spawn_broadcasts_layout_delta_to_subscribers() {
        let guard = start_daemon().await;

        let mut cli = TestConn::connect(&guard.0).await;
        let (workspace, first_pane) = match cli
            .request(Request::ClientSpawn {
                workspace: "1".to_string(),
                split_of: None,
                dir: None,
                bind: None,
            })
            .await
        {
            Response::ClientPaneCreated { workspace, pane } => (workspace, pane),
            other => panic!("expected ClientPaneCreated, got {other:?}"),
        };

        let mut viewer_a = TestConn::connect(&guard.0).await;
        let mut viewer_b = TestConn::connect(&guard.0).await;
        for viewer in [&mut viewer_a, &mut viewer_b] {
            match viewer
                .request(Request::Subscribe {
                    workspace: workspace.to_string(),
                })
                .await
            {
                Response::Snapshot { .. } => {}
                other => panic!("expected Snapshot, got {other:?}"),
            }
        }

        // A bare CLI-style request from a third, never-subscribed
        // connection — simulates `dimux client spawn <ws> --split <pane>`.
        match cli
            .request(Request::ClientSpawn {
                workspace: workspace.to_string(),
                split_of: Some(first_pane),
                dir: Some(SplitDir::Vertical),
                bind: None,
            })
            .await
        {
            Response::ClientPaneCreated { .. } => {}
            other => panic!("expected ClientPaneCreated, got {other:?}"),
        }

        for viewer in [&mut viewer_a, &mut viewer_b] {
            let event = viewer
                .read_event(Duration::from_secs(2))
                .await
                .expect("expected a LayoutDelta push within 2s");
            match event {
                Event::LayoutDelta { workspace: ws, tree } => {
                    assert_eq!(ws, workspace);
                    assert!(tree.is_some(), "split should have produced a non-empty tree");
                }
                other => panic!("expected LayoutDelta, got {other:?}"),
            }
        }
    }

    /// A server-pane bound into client-panes in two different workspaces;
    /// `ServerKill` should unbind both, exercising
    /// `state::server_kill`'s unbind-on-kill behavior through the wire
    /// protocol (not just directly against `State`).
    #[tokio::test]
    async fn server_kill_unbinds_client_panes_across_workspaces() {
        let guard = start_daemon().await;
        let mut conn = TestConn::connect(&guard.0).await;

        let server_pane = match conn
            .request(Request::ServerSpawn {
                name: None,
                cmd: Some("cat".to_string()),
            })
            .await
        {
            Response::ServerPane(info) => info.id,
            other => panic!("expected ServerPane, got {other:?}"),
        };

        let (ws1, pane1) = match conn
            .request(Request::ClientSpawn {
                workspace: "1".to_string(),
                split_of: None,
                dir: None,
                bind: Some(server_pane.to_string()),
            })
            .await
        {
            Response::ClientPaneCreated { workspace, pane } => (workspace, pane),
            other => panic!("expected ClientPaneCreated, got {other:?}"),
        };
        let (ws2, pane2) = match conn
            .request(Request::ClientSpawn {
                workspace: "2".to_string(),
                split_of: None,
                dir: None,
                bind: Some(server_pane.to_string()),
            })
            .await
        {
            Response::ClientPaneCreated { workspace, pane } => (workspace, pane),
            other => panic!("expected ClientPaneCreated, got {other:?}"),
        };

        match conn
            .request(Request::ServerKill {
                target: server_pane.to_string(),
            })
            .await
        {
            Response::Ack => {}
            other => panic!("expected Ack, got {other:?}"),
        }

        for (ws, pane) in [(ws1, pane1), (ws2, pane2)] {
            match conn
                .request(Request::Subscribe {
                    workspace: ws.to_string(),
                })
                .await
            {
                Response::Snapshot { workspace: info, .. } => {
                    let tree = info.tree.expect("workspace should still have its pane");
                    let leaf = tree.find(pane).expect("client-pane should still exist");
                    assert_eq!(leaf.bound, None, "client-pane should be unbound after server_kill");
                }
                other => panic!("expected Snapshot, got {other:?}"),
            }
        }
    }

    /// `ClientUnbind` detaches a client-pane while leaving its server-pane
    /// running -- the wire-level counterpart to
    /// `state::client_unbind_detaches_and_leaves_server_pane_running`.
    #[tokio::test]
    async fn client_unbind_detaches_without_killing_server_pane() {
        let guard = start_daemon().await;
        let mut conn = TestConn::connect(&guard.0).await;

        let server_pane = match conn
            .request(Request::ServerSpawn { name: None, cmd: Some("cat".to_string()) })
            .await
        {
            Response::ServerPane(info) => info.id,
            other => panic!("expected ServerPane, got {other:?}"),
        };

        let (workspace, pane) = match conn
            .request(Request::ClientSpawn {
                workspace: "1".to_string(),
                split_of: None,
                dir: None,
                bind: Some(server_pane.to_string()),
            })
            .await
        {
            Response::ClientPaneCreated { workspace, pane } => (workspace, pane),
            other => panic!("expected ClientPaneCreated, got {other:?}"),
        };

        match conn
            .request(Request::ClientUnbind { workspace: workspace.to_string(), pane })
            .await
        {
            Response::Ack => {}
            other => panic!("expected Ack, got {other:?}"),
        }

        match conn.request(Request::Subscribe { workspace: workspace.to_string() }).await {
            Response::Snapshot { workspace: info, .. } => {
                let tree = info.tree.expect("workspace should still have its pane");
                let leaf = tree.find(pane).expect("client-pane should still exist");
                assert_eq!(leaf.bound, None, "client-pane should be unbound");
            }
            other => panic!("expected Snapshot, got {other:?}"),
        }

        match conn.request(Request::ServerList).await {
            Response::ServerPaneList(list) => {
                assert!(
                    list.iter().any(|p| p.id == server_pane),
                    "unbind must not kill the detached server-pane"
                );
            }
            other => panic!("expected ServerPaneList, got {other:?}"),
        }
    }

    /// `ResizeSplit` updates a divider's ratio (mouse-drag resizing,
    /// design doc addendum), and the change is visible to a fresh
    /// `Subscribe` on the same workspace.
    #[tokio::test]
    async fn resize_split_updates_ratio_visible_to_subscribers() {
        let guard = start_daemon().await;
        let mut conn = TestConn::connect(&guard.0).await;

        let first = match conn
            .request(Request::ClientSpawn {
                workspace: "1".to_string(),
                split_of: None,
                dir: None,
                bind: None,
            })
            .await
        {
            Response::ClientPaneCreated { workspace, pane } => (workspace, pane),
            other => panic!("expected ClientPaneCreated, got {other:?}"),
        };
        let (workspace, first_pane) = first;

        match conn
            .request(Request::ClientSpawn {
                workspace: workspace.to_string(),
                split_of: Some(first_pane),
                dir: Some(protocol::SplitDir::Vertical),
                bind: None,
            })
            .await
        {
            Response::ClientPaneCreated { .. } => {}
            other => panic!("expected ClientPaneCreated, got {other:?}"),
        }

        let split_id = match conn.request(Request::Subscribe { workspace: workspace.to_string() }).await {
            Response::Snapshot { workspace: info, .. } => match info.tree.unwrap() {
                protocol::SplitTree::Split { id, .. } => id,
                other => panic!("expected a split, got {other:?}"),
            },
            other => panic!("expected Snapshot, got {other:?}"),
        };

        match conn
            .request(Request::ResizeSplit { workspace: workspace.to_string(), split: split_id, new_ratio: 0.25 })
            .await
        {
            Response::Ack => {}
            other => panic!("expected Ack, got {other:?}"),
        }

        match conn.request(Request::Subscribe { workspace: workspace.to_string() }).await {
            Response::Snapshot { workspace: info, .. } => match info.tree.unwrap() {
                protocol::SplitTree::Split { ratio, .. } => assert_eq!(ratio, 0.25),
                other => panic!("expected a split, got {other:?}"),
            },
            other => panic!("expected Snapshot, got {other:?}"),
        }

        // Unknown workspace/split both error cleanly.
        match conn
            .request(Request::ResizeSplit {
                workspace: Uuid::new_v4().to_string(),
                split: split_id,
                new_ratio: 0.5,
            })
            .await
        {
            Response::Error { .. } => {}
            other => panic!("expected Error, got {other:?}"),
        }
        match conn
            .request(Request::ResizeSplit {
                workspace: workspace.to_string(),
                split: Uuid::new_v4(),
                new_ratio: 0.5,
            })
            .await
        {
            Response::Error { .. } => {}
            other => panic!("expected Error, got {other:?}"),
        }
    }
}
