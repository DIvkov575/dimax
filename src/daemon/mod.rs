//! The dimax daemon: owns the global server-pane pool and the global
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
//! request; this is fine at the pane counts dimax targets (see design doc
//! non-goals) and keeps the logic in `state` free of async concerns.

pub mod handoff;
pub mod pinned_dirs;
pub mod session;
pub mod state;

/// Serializes every test across this crate that mutates
/// `$XDG_CONFIG_HOME` (or `$HOME`) -- both `pinned_dirs` and `session`
/// resolve their on-disk path from it, and it's process-global, so two
/// such tests from *different* modules running concurrently (the
/// default under `cargo test`) could each see the other's env mutation
/// mid-test even though each module already serializes its own tests
/// against each other. A single shared lock is what actually prevents
/// that cross-module race; two separate per-module locks (this
/// started as one in each of `pinned_dirs`/`session` before they were
/// observed to race one another) do not.
#[cfg(test)]
pub(crate) static XDG_CONFIG_HOME_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

use crate::protocol::{
    self, ClientPane, Event, Request, Response, ServerMessage, ServerPaneId, WorkspaceId,
};
use crate::term::ServerPaneEvent;
use state::{CloseTabResult, State, SubscriberId};
use std::collections::HashMap;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;
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

/// Bind `socket_path`, cleaning up a stale socket file left behind by a
/// crashed or killed previous daemon first (see [`bind_socket`]), then
/// installs a `SIGTERM`/`SIGINT` handler that unlinks `socket_path` before
/// letting the process actually exit, so a *graceful* shutdown leaves no
/// file behind at all -- [`bind_socket`]'s stale-detection is the
/// fallback for the crash case this handler can't catch. Starts from a
/// deterministically empty `State`, session file on disk or not --
/// every caller of this specific function (directly, `dimax attach`'s
/// auto-spawn path and every integration test in this module) wants
/// that. The real `dimax daemon` entry point calls
/// [`run_and_restore_session`] instead, so a session file left by a
/// previous clean shutdown never reaches this one.
pub async fn run(socket_path: std::path::PathBuf) -> anyhow::Result<Daemon> {
    run_with_state(socket_path, State::new()).await
}

/// Same as [`run`], but replays a previously-saved [`session`] into
/// `initial_state` before serving any connections -- what a *deliberate*
/// restart (picking up a rebuilt binary, the whole reason this exists)
/// can use to bring workspace layout back. Kept as a separate function
/// rather than a parameter on `run` itself so no existing test needs to
/// opt out of a behavior it never asked for; see `session` module doc
/// for exactly what this can and can't restore.
pub async fn run_and_restore_session(socket_path: std::path::PathBuf) -> anyhow::Result<Daemon> {
    let mut initial_state = State::new();
    if let Some(saved) = session::take() {
        initial_state.restore_session(saved);
    }
    run_with_state(socket_path, initial_state).await
}

async fn run_with_state(
    socket_path: std::path::PathBuf,
    mut inner_state: State,
) -> anyhow::Result<Daemon> {
    let listener = bind_socket(&socket_path).await?;
    // Claim the pane-event stream exactly once, right here at start-up
    // (module doc / `state::State::take_pane_events` doc comment) — this
    // is the *only* caller, so `expect` is warranted: a `None` here would
    // mean this function ran twice against the same `State`, which can't
    // happen since each caller passes in its own freshly constructed one.
    let mut pane_events = inner_state
        .take_pane_events()
        .expect("freshly constructed State always has its pane-event receiver available");
    let state = Arc::new(Mutex::new(inner_state));
    spawn_cleanup_on_signal(socket_path.clone(), state.clone())?;
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
            // `Died` is a one-time, important state change every
            // subscriber (focused or not) needs to learn about
            // immediately, so it always uses the unthrottled subscriber
            // list; only routine `Changed` output goes through
            // `throttled_subscribers_for_server_pane` (see that
            // method's doc comment for why this distinction matters).
            let (id, is_died) = match event {
                ServerPaneEvent::Changed(id) => (id, false),
                ServerPaneEvent::Died(id) => (id, true),
            };
            // Lock only long enough to check subscribers + snapshot (see
            // `broadcast_grid_prepare`'s doc comment); the lock is
            // released before the expensive serialize+push below, so a
            // burst of PTY output on one pane no longer blocks every
            // other request the daemon is handling for the whole
            // duration of each broadcast.
            let prepared = {
                let mut state = drain_state.lock().await;
                let subscribers = if is_died {
                    state.subscribers_for_server_pane(id)
                } else {
                    state.throttled_subscribers_for_server_pane(id)
                };
                broadcast_grid_prepare_for(&state, id, subscribers)
            };
            if let Some(broadcast) = prepared {
                broadcast_grid_send(&drain_registry, broadcast).await;
            }
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
                    tokio::spawn(handle_connection(
                        stream,
                        state,
                        registry,
                        id,
                        path_for_task.clone(),
                    ));
                }
                Err(err) => {
                    tracing_lite_log(&format!("accept error on {path_for_task:?}: {err}"));
                }
            }
        }
    });

    Ok(Daemon { socket_path })
}

/// Bind `socket_path`, first clearing out a stale leftover socket file if
/// one exists and nothing is actually listening on it.
///
/// A Unix socket file on disk outlives the process that created it if
/// that process dies without unlinking it (killed, crashed, `Ctrl-C`'d) —
/// `UnixListener::bind` then fails with `AddrInUse` on the *next* daemon
/// start even though no daemon is actually running, which is exactly the
/// stuck state `ensure_running`'s callers hit (spawn a new daemon, it
/// fails to bind and exits, the 2s reachability poll times out with no
/// indication why). Distinguishing "stale file" from "a real daemon is
/// already listening" matters — unconditionally unlinking on every start
/// would let two daemons started back-to-back race each other's sockets
/// out from under already-connected clients. So: try connecting first.
/// A successful connect means a real listener answered — leave the file
/// alone and let `bind` fail naturally (a second daemon has no business
/// running). A failed connect (`ECONNREFUSED` or similar — nothing
/// listening) means the file is stale; remove it and bind fresh.
async fn bind_socket(socket_path: &Path) -> anyhow::Result<UnixListener> {
    if socket_path.exists() && UnixStream::connect(socket_path).await.is_err() {
        std::fs::remove_file(socket_path)?;
    }
    Ok(UnixListener::bind(socket_path)?)
}

/// Install a `SIGTERM`/`SIGINT` handler that saves the current
/// workspace layout (see `session` module doc) and unlinks
/// `socket_path` before the process exits, so a graceful shutdown (a
/// normal `kill`, or `Ctrl-C` to a foreground `dimax daemon`) leaves no
/// stale file for `bind_socket`'s fallback to need to clean up next
/// time, and leaves a session file the next `run_and_restore_session`
/// can replay. Spawns a background task that waits for either signal
/// once, saves (best-effort — see `session::save`'s doc comment for why
/// this never blocks or fails shutdown), removes the socket file
/// (also best-effort), and exits the process.
fn spawn_cleanup_on_signal(socket_path: PathBuf, state: Arc<Mutex<State>>) -> anyhow::Result<()> {
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    tokio::spawn(async move {
        tokio::select! {
            _ = sigterm.recv() => {}
            _ = sigint.recv() => {}
        }
        session::save(&state.lock().await.snapshot_for_session_save());
        let _ = std::fs::remove_file(&socket_path);
        std::process::exit(0);
    });
    Ok(())
}

/// Auto-spawn the daemon as a detached background process if
/// `socket_path` isn't reachable, per design doc "Error handling". Used
/// by `dimax attach` and every CLI subcommand before connecting.
///
/// This does a plain detached `std::process::Command::spawn` of `dimax
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
    socket_path: PathBuf,
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
            &socket_path,
            request,
        )
        .await;
        if protocol::framing::write_frame(
            &mut *writer.lock().await,
            &ServerMessage::Response(response),
        )
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
    let mut state = state.lock().await;
    state.clear_scroll_offsets_for_subscriber(subscriber_id);
    state.clear_focus_for_subscriber(subscriber_id);
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
    socket_path: &Path,
    request: Request,
) -> Response {
    let _ = subscriber_id;
    match request {
        Request::ServerSpawn {
            name,
            cmd,
            cwd,
            workspace,
        } => {
            let mut state = state.lock().await;
            let owner = match workspace {
                Some(target) => match state.resolve_workspace(&target) {
                    Ok(id) => Some(id),
                    Err(err) => {
                        return Response::Error {
                            message: err.to_string(),
                        };
                    }
                },
                None => None,
            };
            ok_or_err(
                state.server_spawn(name, cmd, cwd, owner),
                Response::ServerPane,
            )
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
            let mut state = state.lock().await;
            Response::ServerPaneList(state.server_list())
        }

        Request::ToggleDirectoryPin { dir } => {
            let mut state = state.lock().await;
            state.toggle_pinned_dir(dir);
            Response::PinnedDirsList(state.pinned_dirs().to_vec())
        }

        Request::PinnedDirsList => {
            let state = state.lock().await;
            Response::PinnedDirsList(state.pinned_dirs().to_vec())
        }

        Request::ConsumeShellFallback => {
            let mut state = state.lock().await;
            Response::ShellFallback {
                available: state.consume_shell_fallback(),
            }
        }

        Request::ClientSpawn {
            workspace,
            split_of,
            dir,
            bind,
        } => {
            let mut state = state.lock().await;
            let ws_id = match state.resolve_or_create_workspace(&workspace) {
                Ok(id) => id,
                Err(err) => {
                    return Response::Error {
                        message: err.to_string(),
                    };
                }
            };
            let resolved_bind = match bind {
                Some(target) => match state.resolve_server_pane(&target) {
                    Ok(id) => Some(id),
                    Err(err) => {
                        return Response::Error {
                            message: err.to_string(),
                        };
                    }
                },
                None => None,
            };
            match state.client_spawn(ws_id, split_of, dir, resolved_bind) {
                Ok(pane) => {
                    broadcast_layout(&state, registry, ws_id).await;
                    Response::ClientPaneCreated {
                        workspace: ws_id,
                        pane,
                    }
                }
                Err(err) => Response::Error {
                    message: err.to_string(),
                },
            }
        }

        Request::ClientClose { workspace, pane } => {
            let mut state = state.lock().await;
            let ws_id = match state.resolve_workspace(&workspace) {
                Ok(id) => id,
                Err(err) => {
                    return Response::Error {
                        message: err.to_string(),
                    };
                }
            };
            match state.client_close(ws_id, pane) {
                Ok(()) => {
                    broadcast_layout(&state, registry, ws_id).await;
                    Response::Ack
                }
                Err(err) => Response::Error {
                    message: err.to_string(),
                },
            }
        }

        Request::ClientRename {
            workspace,
            pane,
            new_name,
        } => {
            let mut state = state.lock().await;
            let ws_id = match state.resolve_workspace(&workspace) {
                Ok(id) => id,
                Err(err) => {
                    return Response::Error {
                        message: err.to_string(),
                    };
                }
            };
            match state.client_rename(ws_id, pane, new_name) {
                Ok(()) => {
                    broadcast_layout(&state, registry, ws_id).await;
                    Response::Ack
                }
                Err(err) => Response::Error {
                    message: err.to_string(),
                },
            }
        }

        Request::ClientBind {
            workspace,
            pane,
            target,
        } => {
            let mut state = state.lock().await;
            let ws_id = match state.resolve_workspace(&workspace) {
                Ok(id) => id,
                Err(err) => {
                    return Response::Error {
                        message: err.to_string(),
                    };
                }
            };
            let target_id = match state.resolve_server_pane(&target) {
                Ok(id) => id,
                Err(err) => {
                    return Response::Error {
                        message: err.to_string(),
                    };
                }
            };
            match state.client_bind(ws_id, pane, target_id) {
                Ok(()) => {
                    broadcast_layout(&state, registry, ws_id).await;
                    Response::Ack
                }
                Err(err) => Response::Error {
                    message: err.to_string(),
                },
            }
        }

        Request::ClientUnbind { workspace, pane } => {
            let mut state = state.lock().await;
            let ws_id = match state.resolve_workspace(&workspace) {
                Ok(id) => id,
                Err(err) => {
                    return Response::Error {
                        message: err.to_string(),
                    };
                }
            };
            match state.client_unbind(ws_id, pane) {
                Ok(()) => {
                    broadcast_layout(&state, registry, ws_id).await;
                    Response::Ack
                }
                Err(err) => Response::Error {
                    message: err.to_string(),
                },
            }
        }

        Request::ClientAddTab {
            workspace,
            pane,
            target,
        } => {
            let mut state = state.lock().await;
            let ws_id = match state.resolve_workspace(&workspace) {
                Ok(id) => id,
                Err(err) => {
                    return Response::Error {
                        message: err.to_string(),
                    };
                }
            };
            let target_id = match state.resolve_server_pane(&target) {
                Ok(id) => id,
                Err(err) => {
                    return Response::Error {
                        message: err.to_string(),
                    };
                }
            };
            match state.client_add_tab(ws_id, pane, target_id) {
                Ok(()) => {
                    broadcast_layout(&state, registry, ws_id).await;
                    // The newly-active tab (target_id) has no grid pushed
                    // to its viewers yet -- without this, it renders
                    // "(server-pane closed)" until it next produces
                    // output on its own (see broadcast_grid_prepare's doc
                    // comment for why this is a separate, post-lock step).
                    let prepared = broadcast_grid_prepare(&state, target_id);
                    drop(state);
                    if let Some(broadcast) = prepared {
                        broadcast_grid_send(registry, broadcast).await;
                    }
                    Response::Ack
                }
                Err(err) => Response::Error {
                    message: err.to_string(),
                },
            }
        }

        Request::ClientCycleTab {
            workspace,
            pane,
            forward,
        } => {
            let mut state = state.lock().await;
            let ws_id = match state.resolve_workspace(&workspace) {
                Ok(id) => id,
                Err(err) => {
                    return Response::Error {
                        message: err.to_string(),
                    };
                }
            };
            match state.client_cycle_tab(ws_id, pane, forward) {
                Ok(()) => {
                    broadcast_layout(&state, registry, ws_id).await;
                    // Same rationale as ClientAddTab above: the tab
                    // cycled onto needs its grid pushed, not just the
                    // layout change that says which tab is now active.
                    let prepared = state
                        .bound_server_pane(pane)
                        .and_then(|sp| broadcast_grid_prepare(&state, sp));
                    drop(state);
                    if let Some(broadcast) = prepared {
                        broadcast_grid_send(registry, broadcast).await;
                    }
                    Response::Ack
                }
                Err(err) => Response::Error {
                    message: err.to_string(),
                },
            }
        }

        Request::ClientCloseTab { workspace, pane } => {
            let mut state = state.lock().await;
            let ws_id = match state.resolve_workspace(&workspace) {
                Ok(id) => id,
                Err(err) => {
                    return Response::Error {
                        message: err.to_string(),
                    };
                }
            };
            match state.client_close_tab(ws_id, pane) {
                // Both outcomes need the same broadcast: `LeafClosed` already
                // ran `client_close` internally, so the workspace tree in the
                // delta reflects either change on its own.
                Ok(CloseTabResult::TabRemoved | CloseTabResult::LeafClosed) => {
                    broadcast_layout(&state, registry, ws_id).await;
                    // TabRemoved leaves a *different* tab active than
                    // before -- it needs its grid pushed for the same
                    // reason ClientCycleTab does. LeafClosed leaves no
                    // pane bound here at all, so `bound_server_pane`
                    // correctly yields nothing to push.
                    let prepared = state
                        .bound_server_pane(pane)
                        .and_then(|sp| broadcast_grid_prepare(&state, sp));
                    drop(state);
                    if let Some(broadcast) = prepared {
                        broadcast_grid_send(registry, broadcast).await;
                    }
                    Response::Ack
                }
                Err(err) => Response::Error {
                    message: err.to_string(),
                },
            }
        }

        Request::ClientList { workspace } => {
            let state = state.lock().await;
            match workspace {
                Some(w) => {
                    let ws_id = match state.resolve_workspace(&w) {
                        Ok(id) => id,
                        Err(err) => {
                            return Response::Error {
                                message: err.to_string(),
                            };
                        }
                    };
                    let panes: Vec<ClientPane> = state
                        .client_list(Some(ws_id))
                        .into_iter()
                        .map(|(_, pane)| pane)
                        .collect();
                    Response::ClientPaneList {
                        workspace: ws_id,
                        panes,
                    }
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
                    // and ignore `workspace` are unaffected. `dimax
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
                    Response::ClientPaneList {
                        workspace: Uuid::nil(),
                        panes,
                    }
                }
            }
        }

        Request::BeginHandoff { datagram_path } => {
            let mut state = state.lock().await;
            let workspaces: Vec<handoff::HandoffWorkspace> = state
                .workspace_ids()
                .into_iter()
                .filter_map(|id| {
                    let info = state.workspace_info(id).ok()?;
                    Some(handoff::HandoffWorkspace {
                        id: info.id,
                        number: info.number,
                        name: info.name,
                        tree: info.tree.map(split_tree_to_handoff_tree),
                    })
                })
                .collect();

            let panes: Vec<(handoff::HandoffPane, RawFd)> = state
                .server_list()
                .into_iter()
                .filter_map(|info| {
                    let pane = state.server_pane(info.id)?;
                    let fd = pane.dup_master_fd().ok()?;
                    let pid = pane.child_pid()?;
                    Some((
                        handoff::HandoffPane {
                            id: info.id,
                            name: info.name,
                            size: info.size,
                            short_id: info.short_id,
                            owner_workspace: info.owner_workspace,
                            pid,
                        },
                        fd,
                    ))
                })
                .collect();
            let pane_count = panes.len();
            drop(state);

            let path = std::path::PathBuf::from(datagram_path);
            let own_socket_path = socket_path.to_path_buf();
            tokio::spawn(async move {
                match handoff::send_handoff(&path, workspaces, panes).await {
                    Ok(()) => {
                        // The transfer succeeded -- this daemon's job is
                        // done and the new one is about to bind this
                        // same socket path, so vacate it exactly like a
                        // clean `SIGTERM` shutdown does (no session-file
                        // save needed here: unlike that path, the live
                        // state was *already* handed over, not lost).
                        let _ = std::fs::remove_file(&own_socket_path);
                        // A real donor process must actually terminate
                        // here -- that's what fully vacates the socket
                        // path/fds for the new daemon to take over.
                        // `cargo test` instead runs every test as one
                        // thread inside a single shared OS process (see
                        // Task 9's test doc comment on this same
                        // "faithful enough" simplification), where an
                        // unconditional `process::exit` would kill the
                        // entire test binary -- and every other test
                        // running concurrently in it -- the instant any
                        // test exercises a successful handoff.
                        #[cfg(not(test))]
                        std::process::exit(0);
                    }
                    Err(err) => {
                        tracing_lite_log(&format!("send_handoff failed: {err:#}"));
                    }
                }
            });

            Response::HandoffStarting { pane_count }
        }

        Request::Subscribe { workspace } => {
            let mut state = state.lock().await;
            let ws_id = match state.resolve_or_create_workspace(&workspace) {
                Ok(id) => id,
                Err(err) => {
                    return Response::Error {
                        message: err.to_string(),
                    };
                }
            };
            state.subscribe(subscriber_id, ws_id);
            *subscribed_workspace = Some(ws_id);
            let info = match state.workspace_info(ws_id) {
                Ok(info) => info,
                Err(err) => {
                    return Response::Error {
                        message: err.to_string(),
                    };
                }
            };
            let grids = info
                .tree
                .as_ref()
                .map(|tree| {
                    tree.leaves()
                        .into_iter()
                        .filter_map(|leaf| leaf.active_bound())
                        .filter_map(|bound| {
                            let pane = state.server_pane(bound)?;
                            let offset = state.scroll_offset_for(subscriber_id, bound);
                            Some(pane.snapshot(offset))
                        })
                        .collect()
                })
                .unwrap_or_default();
            Response::Snapshot {
                workspace: info,
                grids,
            }
        }

        Request::SetFocus { server_pane } => {
            let mut state = state.lock().await;
            state.set_focus(subscriber_id, server_pane);
            // Immediate, unthrottled catch-up for the newly-focused
            // pane (if any) -- see `SetFocus`'s and `set_focus`'s doc
            // comments for why this is what keeps throttling an
            // unfocused pane's broadcasts always safe, never stale:
            // whatever its last throttled broadcast looked like, this
            // pushes the true current grid right now, to just this one
            // subscriber.
            let prepared = server_pane
                .and_then(|sp| broadcast_grid_prepare_for(&state, sp, vec![subscriber_id]));
            drop(state);
            if let Some(broadcast) = prepared {
                broadcast_grid_send(registry, broadcast).await;
            }
            Response::Ack
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
            let mut guard = state.lock().await;
            let affected = guard.bound_server_pane(pane);
            guard.resize_client_pane(pane, size);
            let prepared =
                affected.and_then(|server_pane| broadcast_grid_prepare(&guard, server_pane));
            // Drop the lock before the expensive serialize+push -- see
            // `broadcast_grid_prepare`'s doc comment.
            drop(guard);
            if let Some(broadcast) = prepared {
                broadcast_grid_send(registry, broadcast).await;
            }
            Response::Ack
        }

        Request::ScrollClientPane { pane, delta } => {
            let mut guard = state.lock().await;
            let Some(server_pane) = guard.bound_server_pane(pane) else {
                return Response::Ack;
            };
            guard.scroll_server_pane(subscriber_id, server_pane, delta);
            let prepared = broadcast_grid_prepare(&guard, server_pane);
            drop(guard);
            if let Some(broadcast) = prepared {
                broadcast_grid_send(registry, broadcast).await;
            }
            Response::Ack
        }

        Request::ResizeSplit {
            workspace,
            split,
            new_ratio,
        } => {
            let mut state = state.lock().await;
            let ws_id = match state.resolve_workspace(&workspace) {
                Ok(id) => id,
                Err(err) => {
                    return Response::Error {
                        message: err.to_string(),
                    };
                }
            };
            match state.resize_split(ws_id, split, new_ratio) {
                Ok(()) => {
                    broadcast_layout(&state, registry, ws_id).await;
                    Response::Ack
                }
                Err(err) => Response::Error {
                    message: err.to_string(),
                },
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

        Request::ServerRead { target } => {
            let state = state.lock().await;
            let id = match state.resolve_server_pane(&target) {
                Ok(id) => id,
                Err(err) => {
                    return Response::Error {
                        message: err.to_string(),
                    };
                }
            };
            let server_pane = state
                .server_pane(id)
                .expect("resolve_server_pane only yields ids present in the pool");
            Response::ServerReadOutput {
                text: server_pane.snapshot_text(),
            }
        }

        Request::ServerSend {
            target,
            text,
            enter,
        } => {
            let state = state.lock().await;
            let id = match state.resolve_server_pane(&target) {
                Ok(id) => id,
                Err(err) => {
                    return Response::Error {
                        message: err.to_string(),
                    };
                }
            };
            let server_pane = state
                .server_pane(id)
                .expect("resolve_server_pane only yields ids present in the pool");
            let mut bytes = text.into_bytes();
            if enter {
                bytes.push(b'\n');
            }
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
        Err(err) => Response::Error {
            message: err.to_string(),
        },
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
    let event = ServerMessage::Event(Event::LayoutDelta {
        workspace,
        tree: info.tree,
    });
    let subscribers = state.subscribers_for_workspace(workspace);
    push_to_subscribers(registry, &subscribers, &event).await;
}

/// Everything needed to push a `GridDelta` for one server-pane's current
/// snapshot to every connection viewing it (i.e. subscribed to some
/// workspace that binds it) — computed while the global state lock is
/// held (see [`broadcast_grid_prepare`]) so the caller can drop the lock
/// before doing any of the actually expensive work. Used both from
/// `dispatch` (`ResizeClientPane`) and from the pane-event drain task in
/// `run` (`ServerPaneEvent::Changed`/`Died`).
struct GridBroadcast {
    /// One entry per distinct scroll offset currently in use among
    /// `server_pane`'s subscribers -- in the common case (nobody
    /// scrolled back) this has exactly one entry, same cost as before
    /// this feature existed.
    groups: Vec<(ServerMessage, Vec<SubscriberId>)>,
}

/// The cheap half of a grid broadcast: check whether anyone is even
/// subscribed to `server_pane`, and if so, copy its current grid.
///
/// Must be called with the state lock held (it needs `state.server_pane`/
/// `subscribers_for_server_pane`), but is itself fast — `ServerPane::
/// snapshot()` is a plain cell-grid walk-and-clone, measured at ~2ms for
/// a 50x200 pane. Returns `None` immediately, without touching the grid
/// at all, if nobody is subscribed — a pane nobody is currently viewing
/// (e.g. producing background scroll in an unwatched workspace) should
/// cost nothing, not even a wasted snapshot.
///
/// Deliberately does NOT serialize the snapshot to `ServerMessage`'s
/// wire bytes here — that's `broadcast_grid_send`'s job, called only
/// after the caller has released the state lock. Serializing a full grid
/// to JSON is the actually expensive step (~22ms measured for the same
/// 50x200 pane, roughly 10x the snapshot itself) — doing it while every
/// other request in the daemon is blocked on the same global lock is
/// what caused dimax to visibly freeze (stop responding to keystrokes in
/// any other pane) whenever a watched pane produced output rapidly
/// enough (e.g. an animated startup banner).
fn broadcast_grid_prepare(state: &State, server_pane: ServerPaneId) -> Option<GridBroadcast> {
    broadcast_grid_prepare_for(state, server_pane, state.subscribers_for_server_pane(server_pane))
}

/// Same as [`broadcast_grid_prepare`], but for an explicit, already-
/// decided `subscribers` list rather than every current viewer --
/// what the drain loop uses to apply
/// `State::throttled_subscribers_for_server_pane`'s focus-aware
/// filtering to a `Changed` broadcast without changing behavior for
/// every other (always-unthrottled) caller of `broadcast_grid_prepare`
/// itself.
fn broadcast_grid_prepare_for(
    state: &State,
    server_pane: ServerPaneId,
    subscribers: Vec<SubscriberId>,
) -> Option<GridBroadcast> {
    if subscribers.is_empty() {
        return None;
    }
    let pane = state.server_pane(server_pane)?;

    let mut by_offset: HashMap<usize, Vec<SubscriberId>> = HashMap::new();
    for sub in subscribers {
        let offset = state.scroll_offset_for(sub, server_pane);
        by_offset.entry(offset).or_default().push(sub);
    }

    let groups = by_offset
        .into_iter()
        .map(|(offset, subs)| {
            let event = ServerMessage::Event(Event::GridDelta {
                snapshot: pane.snapshot(offset),
            });
            (event, subs)
        })
        .collect();
    Some(GridBroadcast { groups })
}

/// The expensive half: serialize and push. Call this with the state lock
/// already released — see [`broadcast_grid_prepare`]'s doc comment for
/// why the split exists.
async fn broadcast_grid_send(registry: &SubscriberRegistry, broadcast: GridBroadcast) {
    for (event, subscribers) in &broadcast.groups {
        push_to_subscribers(registry, subscribers, event).await;
    }
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

pub(crate) fn tracing_lite_log(msg: &str) {
    eprintln!("[dimax-daemon] {msg}");
}

/// The inverse of `state::State::handoff_tree_to_split_tree`: convert a
/// live `protocol::SplitTree` (as returned by `workspace_info`) into the
/// wire shape `handoff::send_handoff` streams to the acceptor daemon.
/// A free function, not a method -- `dispatch` isn't a method on a type
/// with a natural `Self`.
fn split_tree_to_handoff_tree(tree: protocol::SplitTree) -> handoff::HandoffTree {
    match tree {
        protocol::SplitTree::Leaf(pane) => handoff::HandoffTree::Leaf {
            id: pane.id,
            name: pane.name,
            tabs: pane.tabs,
            active_tab: pane.active_tab,
            short_id: pane.short_id,
        },
        protocol::SplitTree::Split { id, dir, ratio, a, b } => handoff::HandoffTree::Split {
            id,
            dir,
            ratio,
            a: Box::new(split_tree_to_handoff_tree(*a)),
            b: Box::new(split_tree_to_handoff_tree(*b)),
        },
    }
}

/// Integration tests per design doc "Testing" > Integration tier: the
/// daemon spun up in-process against a temp socket path, driven through
/// raw `Request`/`ServerMessage` frames (no `cli.rs`/`Client` — this crate
/// deliberately doesn't depend on that module, and the point of these
/// tests is to exercise the wire protocol directly).
#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Size, SplitDir};
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use tokio::net::UnixStream;
    use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

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
        let path = std::env::temp_dir().join(format!("dimax-test-{}.sock", Uuid::new_v4()));
        run(path.clone())
            .await
            .expect("daemon should bind and start");
        SocketGuard(path)
    }

    /// A stale socket file (nothing listening on it) does not block a new
    /// daemon from starting -- `bind_socket` must detect that connecting
    /// to it fails and remove it before binding, rather than propagating
    /// `AddrInUse` (the bug this test guards against: a crashed daemon's
    /// leftover socket file permanently blocking every subsequent start).
    #[tokio::test]
    async fn stale_socket_file_is_cleaned_up_before_binding() {
        let path = std::env::temp_dir().join(format!("dimax-test-{}.sock", Uuid::new_v4()));
        let _guard = SocketGuard(path.clone());

        // Create a listener and immediately drop it without unlinking --
        // exactly what a crashed/killed daemon leaves behind: a socket
        // file on disk with nothing listening on it.
        {
            let listener = UnixListener::bind(&path).unwrap();
            drop(listener);
        }
        assert!(
            path.exists(),
            "the stale file should still be on disk after the listener drops"
        );

        // A real daemon must still be able to start at this path.
        run(path.clone())
            .await
            .expect("daemon should clean up the stale file and bind");

        // And a client must be able to connect to it.
        let mut conn = TestConn::connect(&path).await;
        match conn.request(Request::ServerList).await {
            Response::ServerPaneList(list) => assert!(list.is_empty()),
            other => panic!("expected ServerPaneList, got {other:?}"),
        }
    }

    /// A socket with a real daemon already listening on it is left
    /// alone -- `bind_socket` must not unlink a live daemon's socket out
    /// from under it.
    #[tokio::test]
    async fn live_socket_is_not_removed_and_second_bind_fails() {
        let guard = start_daemon().await;

        // A second `run` at the same path must fail (real daemon already
        // listening) rather than silently stealing the socket file.
        assert!(bind_socket(&guard.0).await.is_err());

        // The original daemon must still be reachable afterward.
        let mut conn = TestConn::connect(&guard.0).await;
        match conn.request(Request::ServerList).await {
            Response::ServerPaneList(list) => assert!(list.is_empty()),
            other => panic!("expected ServerPaneList, got {other:?}"),
        }
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
                cwd: None,
                workspace: None,
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

        assert_eq!(
            grids.len(),
            1,
            "expected exactly one grid for the bound server-pane"
        );
        let text: String = grids[0]
            .lines
            .iter()
            .flat_map(|row| row.iter().map(|c| c.text.as_str()))
            .collect();
        assert!(
            text.contains("hello"),
            "grid text did not contain \"hello\": {text:?}"
        );
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
        // connection — simulates `dimax client spawn <ws> --split <pane>`.
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
                Event::LayoutDelta {
                    workspace: ws,
                    tree,
                } => {
                    assert_eq!(ws, workspace);
                    assert!(
                        tree.is_some(),
                        "split should have produced a non-empty tree"
                    );
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
                cwd: None,
                workspace: None,
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
                Response::Snapshot {
                    workspace: info, ..
                } => {
                    let tree = info.tree.expect("workspace should still have its pane");
                    let leaf = tree.find(pane).expect("client-pane should still exist");
                    assert_eq!(
                        leaf.active_bound(),
                        None,
                        "client-pane should be unbound after server_kill"
                    );
                }
                other => panic!("expected Snapshot, got {other:?}"),
            }
        }
    }

    /// Regression coverage for the attach menu's new delete/rename
    /// actions (see docs/superpowers/specs/2026-08-03-attach-menu-groups-
    /// and-shortcuts-design.md): both requests the menu now issues
    /// directly must still round-trip through the wire protocol exactly
    /// as `dimax server kill`/`rename` already do. This is regression
    /// coverage for the wiring, not new daemon logic -- ServerKill and
    /// ServerRename's actual behavior is already covered by
    /// `server_kill_unbinds_client_panes_across_workspaces` above and by
    /// `daemon::state`'s `server_rename_*` tests.
    #[tokio::test]
    async fn server_rename_then_kill_round_trip_for_the_attach_menu() {
        let guard = start_daemon().await;
        let mut conn = TestConn::connect(&guard.0).await;

        let server_pane = match conn
            .request(Request::ServerSpawn {
                name: None,
                cmd: Some("cat".to_string()),
                cwd: None,
                workspace: None,
            })
            .await
        {
            Response::ServerPane(info) => info.id,
            other => panic!("expected ServerPane, got {other:?}"),
        };

        match conn
            .request(Request::ServerRename {
                target: server_pane.to_string(),
                new_name: Some("renamed-from-menu".to_string()),
            })
            .await
        {
            Response::Ack => {}
            other => panic!("expected Ack, got {other:?}"),
        }

        match conn.request(Request::ServerList).await {
            Response::ServerPaneList(list) => {
                let pane = list
                    .iter()
                    .find(|p| p.id == server_pane)
                    .expect("pane should still exist");
                assert_eq!(pane.name.as_deref(), Some("renamed-from-menu"));
            }
            other => panic!("expected ServerPaneList, got {other:?}"),
        }

        match conn
            .request(Request::ServerKill {
                target: server_pane.to_string(),
            })
            .await
        {
            Response::Ack => {}
            other => panic!("expected Ack, got {other:?}"),
        }

        match conn.request(Request::ServerList).await {
            Response::ServerPaneList(list) => {
                assert!(
                    !list.iter().any(|p| p.id == server_pane),
                    "killed server-pane should no longer be listed"
                );
            }
            other => panic!("expected ServerPaneList, got {other:?}"),
        }
    }

    /// `ServerRead`/`ServerSend` let a caller drive a server-pane directly
    /// by name-or-id, with no workspace or client-pane involved at all --
    /// the gap this closes is that every other way to reach a pane's
    /// input/output (`Request::Input`, `Subscribe`) requires first
    /// binding it into a workspace's client-pane, which is unnecessary
    /// ceremony for a scripting caller (e.g. a Claude Skill) that just
    /// wants to type into a shell and read back what it printed.
    #[tokio::test]
    async fn server_send_and_read_round_trip_with_no_workspace_involved() {
        let guard = start_daemon().await;
        let mut conn = TestConn::connect(&guard.0).await;

        let server_pane = match conn
            .request(Request::ServerSpawn {
                name: None,
                cmd: Some("cat".to_string()),
                cwd: None,
                workspace: None,
            })
            .await
        {
            Response::ServerPane(info) => info.id,
            other => panic!("expected ServerPane, got {other:?}"),
        };

        match conn
            .request(Request::ServerSend {
                target: server_pane.to_string(),
                text: "hello-from-server-send".to_string(),
                enter: true,
            })
            .await
        {
            Response::Ack => {}
            other => panic!("expected Ack, got {other:?}"),
        }

        // `cat` echoes stdin back to its own stdout, so the text sent
        // above should show up in the pane's screen shortly after.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            match conn
                .request(Request::ServerRead {
                    target: server_pane.to_string(),
                })
                .await
            {
                Response::ServerReadOutput { text } => {
                    if text.contains("hello-from-server-send") {
                        break;
                    }
                }
                other => panic!("expected ServerReadOutput, got {other:?}"),
            }
            assert!(
                std::time::Instant::now() < deadline,
                "cat never echoed the sent text back"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// `ServerSpawn { cwd: Some(..) }` -- the attach menu's per-group
    /// "spawn new here" row -- must actually start the process in that
    /// directory, not merely record it as metadata. Exercised through
    /// `ServerRead`, so this also proves the full request/dispatch/
    /// `ServerPane::spawn` wiring end to end.
    #[tokio::test]
    async fn server_spawn_with_cwd_starts_the_process_there() {
        let guard = start_daemon().await;
        let mut conn = TestConn::connect(&guard.0).await;

        let server_pane = match conn
            .request(Request::ServerSpawn {
                name: None,
                cmd: Some("pwd".to_string()),
                cwd: Some("/tmp".to_string()),
                workspace: None,
            })
            .await
        {
            Response::ServerPane(info) => info.id,
            other => panic!("expected ServerPane, got {other:?}"),
        };

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            match conn
                .request(Request::ServerRead {
                    target: server_pane.to_string(),
                })
                .await
            {
                // `/tmp` resolves to `/private/tmp` on macOS -- match on
                // "tmp" (what `pwd` actually prints) rather than the
                // literal path string passed in.
                Response::ServerReadOutput { text } if text.contains("tmp") => break,
                Response::ServerReadOutput { .. } => {}
                other => panic!("expected ServerReadOutput, got {other:?}"),
            }
            assert!(
                std::time::Instant::now() < deadline,
                "pwd never printed the /tmp cwd it was spawned with"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn server_read_unknown_target_errors() {
        let guard = start_daemon().await;
        let mut conn = TestConn::connect(&guard.0).await;

        match conn
            .request(Request::ServerRead {
                target: "no-such-pane".to_string(),
            })
            .await
        {
            Response::Error { .. } => {}
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn consume_shell_fallback_over_the_wire_grants_once_then_denies() {
        let guard = start_daemon().await;
        let mut conn = TestConn::connect(&guard.0).await;

        match conn.request(Request::ConsumeShellFallback).await {
            Response::ShellFallback { available } => assert!(available, "first call should grant"),
            other => panic!("expected ShellFallback, got {other:?}"),
        }
        match conn.request(Request::ConsumeShellFallback).await {
            Response::ShellFallback { available } => assert!(!available, "second call should deny"),
            other => panic!("expected ShellFallback, got {other:?}"),
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
            .request(Request::ServerSpawn {
                name: None,
                cmd: Some("cat".to_string()),
                cwd: None,
                workspace: None,
            })
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
            .request(Request::ClientUnbind {
                workspace: workspace.to_string(),
                pane,
            })
            .await
        {
            Response::Ack => {}
            other => panic!("expected Ack, got {other:?}"),
        }

        match conn
            .request(Request::Subscribe {
                workspace: workspace.to_string(),
            })
            .await
        {
            Response::Snapshot {
                workspace: info, ..
            } => {
                let tree = info.tree.expect("workspace should still have its pane");
                let leaf = tree.find(pane).expect("client-pane should still exist");
                assert_eq!(leaf.active_bound(), None, "client-pane should be unbound");
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

    /// Wire-level walk through the whole tab lifecycle on one leaf:
    /// `ClientAddTab` appends-and-activates, `ClientCycleTab` wraps in both
    /// directions, and `ClientCloseTab` drops just the active tab (leaving
    /// its server-pane running) until the last one takes the leaf with it.
    /// Covers all three dispatch arms plus the `CloseTabResult` split.
    #[tokio::test]
    async fn client_tab_requests_add_cycle_and_close_over_the_wire() {
        let guard = start_daemon().await;
        let mut conn = TestConn::connect(&guard.0).await;

        async fn spawn_server_pane(conn: &mut TestConn) -> ServerPaneId {
            match conn
                .request(Request::ServerSpawn {
                    name: None,
                    cmd: Some("cat".to_string()),
                    cwd: None,
                    workspace: None,
                })
                .await
            {
                Response::ServerPane(info) => info.id,
                other => panic!("expected ServerPane, got {other:?}"),
            }
        }
        let first = spawn_server_pane(&mut conn).await;
        let second = spawn_server_pane(&mut conn).await;

        let (workspace, pane) = match conn
            .request(Request::ClientSpawn {
                workspace: "1".to_string(),
                split_of: None,
                dir: None,
                bind: Some(first.to_string()),
            })
            .await
        {
            Response::ClientPaneCreated { workspace, pane } => (workspace, pane),
            other => panic!("expected ClientPaneCreated, got {other:?}"),
        };

        // Reads the leaf's current (tabs, active_tab) straight off the wire,
        // so every assertion below is about what a real frontend would see.
        async fn tabs_of(
            conn: &mut TestConn,
            workspace: WorkspaceId,
            pane: protocol::ClientPaneId,
        ) -> (Vec<ServerPaneId>, usize) {
            match conn
                .request(Request::Subscribe {
                    workspace: workspace.to_string(),
                })
                .await
            {
                Response::Snapshot {
                    workspace: info, ..
                } => {
                    let tree = info.tree.expect("workspace should have a tree");
                    let leaf = tree.find(pane).expect("client-pane should exist");
                    (leaf.tabs.clone(), leaf.active_tab)
                }
                other => panic!("expected Snapshot, got {other:?}"),
            }
        }

        match conn
            .request(Request::ClientAddTab {
                workspace: workspace.to_string(),
                pane,
                target: second.to_string(),
            })
            .await
        {
            Response::Ack => {}
            other => panic!("expected Ack, got {other:?}"),
        }
        assert_eq!(
            tabs_of(&mut conn, workspace, pane).await,
            (vec![first, second], 1),
            "add-tab should append the new tab and make it active"
        );

        // Unknown targets must be rejected, same as `ClientBind`.
        match conn
            .request(Request::ClientAddTab {
                workspace: workspace.to_string(),
                pane,
                target: "no-such-pane".to_string(),
            })
            .await
        {
            Response::Error { .. } => {}
            other => panic!("expected Error, got {other:?}"),
        }

        // Forward from the last tab wraps to the first, and back again.
        for (forward, expected_active) in [(true, 0), (false, 1)] {
            match conn
                .request(Request::ClientCycleTab {
                    workspace: workspace.to_string(),
                    pane,
                    forward,
                })
                .await
            {
                Response::Ack => {}
                other => panic!("expected Ack, got {other:?}"),
            }
            let (tabs, active) = tabs_of(&mut conn, workspace, pane).await;
            assert_eq!(
                tabs,
                vec![first, second],
                "cycling must not change the tab list"
            );
            assert_eq!(
                active, expected_active,
                "cycle forward={forward} should wrap"
            );
        }

        // Closing the active tab (`second`) leaves the other tab, and the
        // closed tab's server-pane, alive.
        match conn
            .request(Request::ClientCloseTab {
                workspace: workspace.to_string(),
                pane,
            })
            .await
        {
            Response::Ack => {}
            other => panic!("expected Ack, got {other:?}"),
        }
        assert_eq!(
            tabs_of(&mut conn, workspace, pane).await,
            (vec![first], 0),
            "close-tab should drop only the active tab and clamp active_tab"
        );
        match conn.request(Request::ServerList).await {
            Response::ServerPaneList(list) => assert!(
                list.iter().any(|p| p.id == second),
                "closing a tab must not kill its server-pane"
            ),
            other => panic!("expected ServerPaneList, got {other:?}"),
        }

        // Closing the last tab closes the leaf itself (design doc: there is
        // no reachable 0-tab-but-present leaf state).
        match conn
            .request(Request::ClientCloseTab {
                workspace: workspace.to_string(),
                pane,
            })
            .await
        {
            Response::Ack => {}
            other => panic!("expected Ack, got {other:?}"),
        }
        match conn
            .request(Request::Subscribe {
                workspace: workspace.to_string(),
            })
            .await
        {
            Response::Snapshot {
                workspace: info, ..
            } => {
                assert!(
                    info.tree.is_none(),
                    "closing the last tab of the only leaf should empty the workspace"
                );
            }
            other => panic!("expected Snapshot, got {other:?}"),
        }
    }

    /// Regression test: cycling to (or adding) a tab must push a
    /// `GridDelta` for the tab's server-pane immediately, not leave a
    /// subscriber waiting for that pane's own next PTY output. Without
    /// this, a subscriber who cycles to an idle tab (nothing producing
    /// fresh output right now) sees "(server-pane closed)" until
    /// something happens to write to it -- which may be never, for a
    /// finished build or a paused REPL, exactly the kind of pane a user
    /// cycles to in order to read.
    #[tokio::test]
    async fn cycling_and_adding_tabs_pushes_a_grid_for_the_newly_active_pane() {
        let guard = start_daemon().await;
        let mut conn = TestConn::connect(&guard.0).await;
        let mut subscriber = TestConn::connect(&guard.0).await;

        async fn spawn_printing_pane(conn: &mut TestConn, text: &str) -> ServerPaneId {
            let id = match conn
                .request(Request::ServerSpawn {
                    name: None,
                    cmd: Some(format!("printf {text}")),
                    cwd: None,
                    workspace: None,
                })
                .await
            {
                Response::ServerPane(info) => info.id,
                other => panic!("expected ServerPane, got {other:?}"),
            };
            // Wait for the pane's own output to actually land before
            // moving on -- under heavy parallel test load, `printf`'s
            // process may not even be scheduled yet a fixed number of ms
            // later, and this test needs the pane's content to be
            // present and *stable* before the tab-add/cycle push it's
            // checking for happens, or the assertion below can't tell
            // "still starting up" from "the fix didn't work".
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            loop {
                match conn
                    .request(Request::ServerRead {
                        target: id.to_string(),
                    })
                    .await
                {
                    Response::ServerReadOutput { text: seen } if seen.contains(text) => break,
                    Response::ServerReadOutput { .. } => {}
                    other => panic!("expected ServerReadOutput, got {other:?}"),
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "pane never printed {text:?}"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            id
        }
        let first = spawn_printing_pane(&mut conn, "first-output").await;

        let (workspace, pane) = match conn
            .request(Request::ClientSpawn {
                workspace: "1".to_string(),
                split_of: None,
                dir: None,
                bind: Some(first.to_string()),
            })
            .await
        {
            Response::ClientPaneCreated { workspace, pane } => (workspace, pane),
            other => panic!("expected ClientPaneCreated, got {other:?}"),
        };

        match subscriber
            .request(Request::Subscribe {
                workspace: workspace.to_string(),
            })
            .await
        {
            Response::Snapshot { .. } => {}
            other => panic!("expected Snapshot, got {other:?}"),
        }
        // Drain whatever GridDelta(s) `first`'s own printf produced so
        // they can't be mistaken for the tab-add/cycle push below.
        while subscriber
            .read_event(Duration::from_millis(100))
            .await
            .is_some()
        {}

        // Spawn a *second* pane whose own output already happened and
        // finished well before it's ever added as a tab -- nothing about
        // adding or cycling to it should depend on it producing anything
        // new.
        let second = spawn_printing_pane(&mut conn, "second-output").await;
        while conn.read_event(Duration::from_millis(50)).await.is_some() {}

        match conn
            .request(Request::ClientAddTab {
                workspace: workspace.to_string(),
                pane,
                target: second.to_string(),
            })
            .await
        {
            Response::Ack => {}
            other => panic!("expected Ack, got {other:?}"),
        }
        let grid = loop {
            match subscriber
                .read_event(Duration::from_secs(2))
                .await
                .expect("ClientAddTab should push a GridDelta for the newly-active tab")
            {
                Event::GridDelta { snapshot } if snapshot.server_pane == second => break snapshot,
                _ => continue,
            }
        };
        let text: String = grid
            .lines
            .iter()
            .flat_map(|row| row.iter().map(|c| c.text.as_str()))
            .collect();
        assert!(
            text.contains("second-output"),
            "pushed grid should be for the newly-active pane: {text:?}"
        );

        // Cycle back to `first` -- same requirement applies.
        match conn
            .request(Request::ClientCycleTab {
                workspace: workspace.to_string(),
                pane,
                forward: false,
            })
            .await
        {
            Response::Ack => {}
            other => panic!("expected Ack, got {other:?}"),
        }
        let grid = loop {
            match subscriber
                .read_event(Duration::from_secs(2))
                .await
                .expect("ClientCycleTab should push a GridDelta for the newly-active tab")
            {
                Event::GridDelta { snapshot } if snapshot.server_pane == first => break snapshot,
                _ => continue,
            }
        };
        let text: String = grid
            .lines
            .iter()
            .flat_map(|row| row.iter().map(|c| c.text.as_str()))
            .collect();
        assert!(
            text.contains("first-output"),
            "pushed grid should be for the newly-active pane: {text:?}"
        );
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

        let split_id = match conn
            .request(Request::Subscribe {
                workspace: workspace.to_string(),
            })
            .await
        {
            Response::Snapshot {
                workspace: info, ..
            } => match info.tree.unwrap() {
                protocol::SplitTree::Split { id, .. } => id,
                other => panic!("expected a split, got {other:?}"),
            },
            other => panic!("expected Snapshot, got {other:?}"),
        };

        match conn
            .request(Request::ResizeSplit {
                workspace: workspace.to_string(),
                split: split_id,
                new_ratio: 0.25,
            })
            .await
        {
            Response::Ack => {}
            other => panic!("expected Ack, got {other:?}"),
        }

        match conn
            .request(Request::Subscribe {
                workspace: workspace.to_string(),
            })
            .await
        {
            Response::Snapshot {
                workspace: info, ..
            } => match info.tree.unwrap() {
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

    /// Regression test for the "bash renders in the top half" bug: a
    /// client-pane's on-screen size, once reported via
    /// `Request::ResizeClientPane`, must actually change the bound
    /// server-pane's PTY/grid size that a subsequent `Subscribe`
    /// snapshot returns -- this exercises the exact wire-level path the
    /// TUI's per-frame resize reporting now drives.
    #[tokio::test]
    async fn resize_client_pane_changes_the_subscribed_grid_size() {
        let guard = start_daemon().await;
        let mut conn = TestConn::connect(&guard.0).await;

        let (workspace, pane) = match conn
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

        match conn
            .request(Request::ResizeClientPane {
                pane,
                size: Size {
                    rows: 40,
                    cols: 120,
                },
            })
            .await
        {
            Response::Ack => {}
            other => panic!("expected Ack, got {other:?}"),
        }

        match conn
            .request(Request::Subscribe {
                workspace: workspace.to_string(),
            })
            .await
        {
            Response::Snapshot { grids, .. } => {
                // No server-pane is bound yet in this test, so `grids`
                // is empty -- this test only needs to confirm the
                // request itself is accepted and doesn't error; the
                // actual size-application-to-a-bound-pane path is
                // already covered by `daemon::state`'s
                // `pty_size_is_smallest_viewer_dimension_wise` test.
                // Bind a server-pane now and re-subscribe to see the
                // resize actually reflected in a grid.
                let _ = grids;
            }
            other => panic!("expected Snapshot, got {other:?}"),
        }

        let server_pane = match conn
            .request(Request::ServerSpawn {
                name: None,
                cmd: Some("cat".to_string()),
                cwd: None,
                workspace: None,
            })
            .await
        {
            Response::ServerPane(info) => info.id,
            other => panic!("expected ServerPane, got {other:?}"),
        };
        match conn
            .request(Request::ClientBind {
                workspace: workspace.to_string(),
                pane,
                target: server_pane.to_string(),
            })
            .await
        {
            Response::Ack => {}
            other => panic!("expected Ack, got {other:?}"),
        }

        match conn
            .request(Request::Subscribe {
                workspace: workspace.to_string(),
            })
            .await
        {
            Response::Snapshot { grids, .. } => {
                assert_eq!(
                    grids.len(),
                    1,
                    "expected exactly one grid for the bound server-pane"
                );
                assert_eq!(
                    grids[0].size,
                    Size {
                        rows: 40,
                        cols: 120
                    },
                    "server-pane's grid size should reflect the earlier ResizeClientPane call"
                );
            }
            other => panic!("expected Snapshot, got {other:?}"),
        }
    }

    /// Two connections subscribed to the same workspace/server-pane;
    /// one scrolls back, the other doesn't. Exercises
    /// `broadcast_grid_prepare`'s offset-grouping logic -- each
    /// connection's own subsequent `GridDelta`s must reflect its own
    /// scroll position, independent of the other's.
    #[tokio::test]
    async fn scroll_offset_is_independent_per_subscriber() {
        let guard = start_daemon().await;

        let mut owner = TestConn::connect(&guard.0).await;
        let server_pane = match owner
            .request(Request::ServerSpawn {
                name: None,
                cmd: Some("cat".to_string()),
                cwd: None,
                workspace: None,
            })
            .await
        {
            Response::ServerPane(info) => info.id,
            other => panic!("expected ServerPane, got {other:?}"),
        };
        let (workspace, pane) = match owner
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
        match owner
            .request(Request::ResizeClientPane {
                pane,
                size: Size { rows: 5, cols: 80 },
            })
            .await
        {
            Response::Ack => {}
            other => panic!("expected Ack, got {other:?}"),
        }

        let mut scroller = TestConn::connect(&guard.0).await;
        let mut watcher = TestConn::connect(&guard.0).await;
        for conn in [&mut scroller, &mut watcher] {
            match conn
                .request(Request::Subscribe {
                    workspace: workspace.to_string(),
                })
                .await
            {
                Response::Snapshot { .. } => {}
                other => panic!("expected Snapshot, got {other:?}"),
            }
        }

        for i in 0..20 {
            match owner
                .request(Request::Input {
                    pane,
                    bytes: format!("line-{i}\n").into_bytes(),
                })
                .await
            {
                Response::Ack => {}
                other => panic!("expected Ack, got {other:?}"),
            }
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if scroller
                .read_event(Duration::from_millis(50))
                .await
                .is_none()
            {
                break;
            }
        }
        while std::time::Instant::now() < deadline {
            if watcher
                .read_event(Duration::from_millis(50))
                .await
                .is_none()
            {
                break;
            }
        }

        match scroller
            .request(Request::ScrollClientPane { pane, delta: 3 })
            .await
        {
            Response::Ack => {}
            other => panic!("expected Ack, got {other:?}"),
        }

        match owner
            .request(Request::Input {
                pane,
                bytes: b"more\n".to_vec(),
            })
            .await
        {
            Response::Ack => {}
            other => panic!("expected Ack, got {other:?}"),
        }

        let scroller_event = scroller
            .read_event(Duration::from_secs(2))
            .await
            .expect("expected a GridDelta for the scrolled-back connection");
        let watcher_event = watcher
            .read_event(Duration::from_secs(2))
            .await
            .expect("expected a GridDelta for the live connection");

        match scroller_event {
            Event::GridDelta { snapshot } => {
                assert!(
                    snapshot.scroll_offset > 0,
                    "scroller's snapshot should reflect its scrolled offset"
                );
            }
            other => panic!("expected GridDelta, got {other:?}"),
        }
        match watcher_event {
            Event::GridDelta { snapshot } => {
                assert_eq!(
                    snapshot.scroll_offset, 0,
                    "watcher never scrolled -- should stay live"
                );
            }
            other => panic!("expected GridDelta, got {other:?}"),
        }
    }

    /// Regression test for the "dimax hangs often" bug: a watched
    /// server-pane producing rapid output must not block an UNRELATED
    /// request on a separate connection. Before the
    /// `broadcast_grid_prepare`/`broadcast_grid_send` split, every
    /// `Changed` event serialized the busy pane's grid to JSON (measured
    /// at ~22ms for a modest 50x200 pane) while holding the daemon's one
    /// global state lock -- a fast-scrolling pane could keep that lock
    /// saturated closely enough to starve every other request, which is
    /// what made typing in an unrelated pane appear to freeze.
    #[tokio::test]
    async fn busy_watched_pane_does_not_starve_unrelated_requests() {
        let guard = start_daemon().await;

        // Connection A: subscribes to a workspace containing a
        // fast-scrolling pane (`yes`), so its output genuinely triggers
        // the broadcast path this test is regression-testing, not the
        // already-cheap "nobody's watching" early-out.
        let mut busy_conn = TestConn::connect(&guard.0).await;
        let busy_server = match busy_conn
            .request(Request::ServerSpawn {
                name: None,
                cmd: Some("yes".to_string()),
                cwd: None,
                workspace: None,
            })
            .await
        {
            Response::ServerPane(info) => info.id,
            other => panic!("expected ServerPane, got {other:?}"),
        };
        let busy_workspace = match busy_conn
            .request(Request::ClientSpawn {
                workspace: "1".to_string(),
                split_of: None,
                dir: None,
                bind: Some(busy_server.to_string()),
            })
            .await
        {
            Response::ClientPaneCreated { workspace, .. } => workspace,
            other => panic!("expected ClientPaneCreated, got {other:?}"),
        };
        match busy_conn
            .request(Request::Subscribe {
                workspace: busy_workspace.to_string(),
            })
            .await
        {
            Response::Snapshot { .. } => {}
            other => panic!("expected Snapshot, got {other:?}"),
        }

        // Give `yes` a moment to actually get its output flowing through
        // the reader thread -> Changed events -> broadcast path before
        // measuring, so the test isn't just racing process startup.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Connection B: a totally unrelated connection, doing totally
        // unrelated work (spawning its own server-pane) while A's pane
        // is actively flooding. Measure wall-clock time for a handful of
        // B's requests; each one needing the same global lock A's
        // broadcasts contend for.
        let mut other_conn = TestConn::connect(&guard.0).await;
        let start = std::time::Instant::now();
        for i in 0..10 {
            match other_conn
                .request(Request::ServerSpawn {
                    name: Some(format!("unrelated-{i}")),
                    cmd: None,
                    cwd: None,
                    workspace: None,
                })
                .await
            {
                Response::ServerPane(_) => {}
                other => panic!("expected ServerPane, got {other:?}"),
            }
        }
        let elapsed = start.elapsed();

        // Generous bound: each of these 10 requests should complete in
        // well under a second combined on any reasonable machine once
        // they're not queueing behind a busy pane's own ~22ms-per-event
        // serialize-while-locked cost repeated many times a second. This
        // is deliberately loose (not asserting a tight ms bound, which
        // would be flaky across machines/CI) -- it exists to catch a
        // gross regression back to lock-holding-during-serialize, not to
        // pin an exact performance number.
        assert!(
            elapsed < Duration::from_secs(2),
            "10 unrelated ServerSpawn requests took {elapsed:?} while a watched pane was \
             flooding output -- expected well under 2s if broadcasts aren't holding the \
             global lock during serialization"
        );
    }

    /// Collect every `Event` `conn` receives within `window`, bounded by
    /// wall-clock time rather than "until a gap appears between
    /// events" -- required against a continuously-flooding pane like
    /// `yes`, since the whole point of an unthrottled/focused
    /// subscriber is that it never actually goes idle while the flood
    /// continues. An earlier version of this helper (inlined, "loop
    /// while `read_event` keeps returning `Some`") could run for
    /// minutes against real `yes` output before a scheduling gap wider
    /// than its own per-read timeout finally appeared by chance.
    async fn collect_events_for(conn: &mut TestConn, window: Duration) -> Vec<Event> {
        let deadline = std::time::Instant::now() + window;
        let mut events = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match conn.read_event(remaining).await {
                Some(event) => events.push(event),
                None => break,
            }
        }
        events
    }

    /// Regression coverage for the focus-aware throttle
    /// (`State::throttled_subscribers_for_server_pane`): a subscriber
    /// who never `SetFocus`es the flooding pane must receive
    /// meaningfully fewer `GridDelta`s for it than one who did, over
    /// the same burst of output -- the whole point of throttling
    /// unfocused broadcasts. And once the unfocused subscriber does
    /// focus it, the very next thing they see must be a fresh grid, not
    /// a stale one left over from throttling (`SetFocus`'s immediate
    /// catch-up push).
    #[tokio::test]
    async fn unfocused_subscriber_gets_fewer_broadcasts_and_catches_up_on_focus() {
        let guard = start_daemon().await;
        let mut spawner = TestConn::connect(&guard.0).await;

        let server = match spawner
            .request(Request::ServerSpawn {
                name: None,
                cmd: Some("yes".to_string()),
                cwd: None,
                workspace: None,
            })
            .await
        {
            Response::ServerPane(info) => info.id,
            other => panic!("expected ServerPane, got {other:?}"),
        };
        let workspace = match spawner
            .request(Request::ClientSpawn {
                workspace: "1".to_string(),
                split_of: None,
                dir: None,
                bind: Some(server.to_string()),
            })
            .await
        {
            Response::ClientPaneCreated { workspace, .. } => workspace,
            other => panic!("expected ClientPaneCreated, got {other:?}"),
        };

        let mut focused = TestConn::connect(&guard.0).await;
        let mut unfocused = TestConn::connect(&guard.0).await;
        for conn in [&mut focused, &mut unfocused] {
            match conn
                .request(Request::Subscribe {
                    workspace: workspace.to_string(),
                })
                .await
            {
                Response::Snapshot { .. } => {}
                other => panic!("expected Snapshot, got {other:?}"),
            }
        }
        match focused
            .request(Request::SetFocus {
                server_pane: Some(server),
            })
            .await
        {
            Response::Ack => {}
            other => panic!("expected Ack, got {other:?}"),
        }
        // `unfocused` deliberately never sends `SetFocus` at all --
        // exercising the common case (a client that never bothers, or a
        // pane in a workspace nobody's looking at right now) rather than
        // only the explicit "focused elsewhere" case.

        // Same fixed window for both: `yes` keeps flooding throughout,
        // so this compares broadcast counts over identical wall-clock
        // time rather than draining either side to (an unreachable)
        // quiescence. 2s (20x the 100ms throttle window) rather than
        // something tighter -- under a fully loaded `cargo test` run
        // (every other test's real PTYs competing for CPU), a short
        // window can let the flood produce so few batches overall that
        // none happen to be >100ms apart yet, making the two counts
        // come out equal by coincidence rather than by a broken
        // throttle. More headroom is cheap; this test still finishes
        // in ~2s either way.
        let window = Duration::from_millis(2000);
        let focused_events = collect_events_for(&mut focused, window).await;
        let unfocused_events = collect_events_for(&mut unfocused, window).await;
        assert!(
            unfocused_events.len() < focused_events.len(),
            "unfocused subscriber ({} events) should receive noticeably fewer broadcasts than \
             the focused one ({} events) over the same flood",
            unfocused_events.len(),
            focused_events.len()
        );

        // Flush anything already in flight for `unfocused` (up to one
        // throttle window's worth) so the very next event it sees after
        // `SetFocus` is unambiguously that request's own catch-up push.
        collect_events_for(&mut unfocused, Duration::from_millis(150)).await;

        // Focus it from the previously-unfocused connection: the very
        // next event must be a fresh `GridDelta` for `server`,
        // delivered immediately rather than waiting for the next
        // `Changed` (which, per the throttle, could otherwise be up to
        // 100ms away).
        match unfocused
            .request(Request::SetFocus {
                server_pane: Some(server),
            })
            .await
        {
            Response::Ack => {}
            other => panic!("expected Ack, got {other:?}"),
        }
        match unfocused
            .read_event(Duration::from_secs(1))
            .await
            .expect("SetFocus should push an immediate catch-up GridDelta")
        {
            Event::GridDelta { snapshot } => assert_eq!(snapshot.server_pane, server),
            other => panic!("expected GridDelta, got {other:?}"),
        }
    }

    /// End-to-end coverage for `session`: a daemon started via
    /// `run_and_restore_session` picks up a session file (the exact
    /// scenario motivating the whole module -- restarting to pick up a
    /// rebuilt binary without losing every workspace) and the pane it
    /// respawns is reachable over the real wire protocol with its saved
    /// name, unlike a plain `run()`. Verified against the actual daemon
    /// entry point every other test in this module uses, not just
    /// against `State::restore_session` in isolation (already covered
    /// by `daemon::state`'s own tests).
    #[tokio::test]
    async fn run_and_restore_session_brings_back_a_saved_workspace() {
        let dir = std::env::temp_dir().join(format!("dimax-session-restore-test-{}", Uuid::new_v4()));

        // Everything that touches `$XDG_CONFIG_HOME` (saving, and
        // `session::take` reading it back) happens synchronously in
        // this block, guard held throughout -- then the guard drops
        // before the first `.await` below. Holding a blocking
        // `std::sync::Mutex` across an await point is its own hazard
        // (clippy's `await_holding_lock`) independent of what the lock
        // protects; this avoids it entirely rather than relying on
        // exactly when `run_and_restore_session`'s own internal
        // `session::take()` call happens to run relative to its first
        // real await. `run_and_restore_session` itself is a thin,
        // directly-inspectable wrapper over exactly this
        // take-then-restore-then-serve sequence (see its doc comment),
        // so replicating it here to keep the lock synchronous doesn't
        // lose meaningful coverage.
        let restored_state = {
            // Shared across every module that mutates
            // `$XDG_CONFIG_HOME` -- see `XDG_CONFIG_HOME_TEST_LOCK`'s
            // doc comment.
            let _env_guard = XDG_CONFIG_HOME_TEST_LOCK.lock().unwrap();
            let prev = std::env::var_os("XDG_CONFIG_HOME");
            unsafe {
                std::env::set_var("XDG_CONFIG_HOME", &dir);
            }

            let mut prior = State::new();
            let sp = prior
                .server_spawn(Some("editor".to_string()), Some("cat".to_string()), None, None)
                .unwrap()
                .id;
            let ws = prior.resolve_or_create_workspace("3").unwrap();
            prior.client_spawn(ws, None, None, Some(sp)).unwrap();
            session::save(&prior.snapshot_for_session_save());
            // Nothing further needed from the live pane -- the
            // snapshot above is already plain owned data, independent
            // of the process from here on. Kill it explicitly rather
            // than just dropping `prior`: `ServerPane` has no `Drop`
            // impl (by design -- the daemon owns that lifecycle via
            // `State::server_kill`), so an un-killed pane would
            // otherwise leak its `cat` process and reader thread for
            // the rest of the test run.
            prior.server_kill(&sp.to_string()).unwrap();

            let mut fresh = State::new();
            if let Some(saved) = session::take() {
                fresh.restore_session(saved);
            }

            unsafe {
                match prev {
                    Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                    None => std::env::remove_var("XDG_CONFIG_HOME"),
                }
            }
            fresh
        };
        let _ = std::fs::remove_dir_all(&dir);

        let path = std::env::temp_dir().join(format!("dimax-test-{}.sock", Uuid::new_v4()));
        let _socket_guard = SocketGuard(path.clone());
        run_with_state(path.clone(), restored_state)
            .await
            .expect("daemon should bind, restore, and start");

        let mut conn = TestConn::connect(&path).await;
        match conn.request(Request::ServerList).await {
            Response::ServerPaneList(list) => {
                assert_eq!(list.len(), 1);
                assert_eq!(list[0].name, Some("editor".to_string()));
            }
            other => panic!("expected ServerPaneList, got {other:?}"),
        }
        match conn.request(Request::Subscribe { workspace: "3".to_string() }).await {
            Response::Snapshot { workspace, .. } => {
                assert_eq!(workspace.number, 3, "restore should keep the original workspace number");
            }
            other => panic!("expected Snapshot, got {other:?}"),
        }
    }

    /// Short filename -- a full-uuid-named path under a long macOS temp
    /// dir can exceed `AF_UNIX`'s `sun_path` limit (~104 bytes); this
    /// stays well under it. Mirrors `tui::tests::app_against_real_daemon`'s
    /// established pattern (pid + a per-process atomic counter, not a
    /// uuid) for the same reason.
    fn short_temp_socket_path(prefix: &str) -> PathBuf {
        static NEXT_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!("{prefix}-{}-{id}.sock", std::process::id()))
    }

    /// Regression coverage for the donor side of a hot restart: a live
    /// pane's fd genuinely survives the trip -- write through the
    /// *original* pty after `BeginHandoff` completes, confirm the
    /// *adopted* copy on the other end of the datagram channel
    /// observes it. This only exercises the donor's half (this
    /// dispatch handler); Task 9 covers the acceptor's
    /// `run_taking_over_or_fresh` end-to-end, including this same
    /// daemon actually exiting afterward.
    #[tokio::test]
    async fn begin_handoff_sends_every_live_pane_and_its_real_fd() {
        let guard = start_daemon().await;
        let mut conn = TestConn::connect(&guard.0).await;

        let server = match conn
            .request(Request::ServerSpawn {
                name: Some("editor".to_string()),
                cmd: Some("cat".to_string()),
                cwd: None,
                workspace: None,
            })
            .await
        {
            Response::ServerPane(info) => info.id,
            other => panic!("expected ServerPane, got {other:?}"),
        };
        let (_ws, _pane) = match conn
            .request(Request::ClientSpawn {
                workspace: "1".to_string(),
                split_of: None,
                dir: None,
                bind: Some(server.to_string()),
            })
            .await
        {
            Response::ClientPaneCreated { workspace, pane } => (workspace, pane),
            other => panic!("expected ClientPaneCreated, got {other:?}"),
        };

        let datagram_path = short_temp_socket_path("dmx-hod");
        let receiver_path = datagram_path.clone();
        let receiver = tokio::spawn(async move {
            crate::daemon::handoff::receive_handoff(&receiver_path).await.unwrap()
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        match conn
            .request(Request::BeginHandoff {
                datagram_path: datagram_path.to_string_lossy().into_owned(),
            })
            .await
        {
            Response::HandoffStarting { pane_count } => assert_eq!(pane_count, 1),
            other => panic!("expected HandoffStarting, got {other:?}"),
        }

        let received = receiver.await.unwrap();
        assert_eq!(received.workspaces.len(), 1);
        assert_eq!(received.workspaces[0].number, 1);
        assert_eq!(received.panes.len(), 1);
        assert_eq!(received.panes[0].0.id, server);
        assert_eq!(received.panes[0].0.name, Some("editor".to_string()));

        // Write directly through the *received* fd (standing in for
        // "the real child process produces output after the handoff
        // completes") and confirm the pane's own on-screen contents
        // pick it up, proving the received fd is a genuine live alias
        // of the *same* pty the original `ServerPane` holds -- not a
        // snapshot or an unrelated fd that happens to look right.
        //
        // Deliberately not a raw `libc::read` on the received fd:
        // this pane's own background reader thread is a separate dup
        // of the same master fd, already looping on reads against the
        // exact same underlying open file description. Dup'd fds
        // share one kernel-side stream, not independent copies, so a
        // raw read here would race that thread for the same bytes --
        // and the thread, already mid-poll, wins essentially every
        // time, leaving a competing blocking `read` here to hang
        // forever waiting for data that already got consumed
        // elsewhere. Observing via `ServerRead` (the pane's normal,
        // already-tested output path) sidesteps that race entirely.
        unsafe {
            let msg = b"hello\n";
            libc::write(received.panes[0].1, msg.as_ptr() as *const _, msg.len());
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            match conn
                .request(Request::ServerRead {
                    target: server.to_string(),
                })
                .await
            {
                Response::ServerReadOutput { text } if text.contains("hello") => break,
                Response::ServerReadOutput { .. } => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "adopted fd's write never showed up in the pane's own output within 2s"
                    );
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                other => panic!("expected ServerReadOutput, got {other:?}"),
            }
        }

        let _ = std::fs::remove_file(&datagram_path);
    }
}
