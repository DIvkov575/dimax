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
//!
//! # Hot reload
//!
//! `Request::DaemonReload` (`dimax daemon reload`) upgrades the running
//! daemon onto a freshly built binary *without* killing any server-pane
//! -- normal `pkill`-and-respawn kills every PTY the old process owned,
//! since a PTY master fd (and the shell's controlling-terminal
//! relationship to it) dies with the process holding it. Instead:
//!
//! 1. `dispatch`'s `DaemonReload` arm snapshots `State` via
//!    `state::State::to_resume` (every server-pane's metadata plus its
//!    PTY master's raw fd and its shell's pid -- see that method's doc
//!    comment for what's deliberately excluded) and writes it to a temp
//!    file next to the socket.
//! 2. It clears `FD_CLOEXEC` on every one of those PTY master fds, and
//!    on the listening socket's fd -- both would otherwise be closed by
//!    the kernel the instant `execve` runs, which is what makes a
//!    normal restart kill everything.
//! 3. It records the resume-file path on the shared `ReloadHandle`
//!    rather than exec'ing immediately, and returns `Response::Ack`.
//! 4. `handle_connection`'s loop writes that `Ack` frame to the
//!    socket -- *then*, and only then, checks `ReloadHandle` and calls
//!    `perform_reexec`. `execve` (via `std::process::Command::exec`)
//!    replaces this process's image in place, keeping its pid and every
//!    fd that survived step 2. On success it never returns; a failure
//!    (e.g. the binary vanished) is logged and this loop just keeps
//!    running the old code -- there's no way to tell the already-Ack'd
//!    caller after the fact, so `Request::DaemonReload`'s doc comment
//!    says as much.
//! 5. The new process image starts at `main` again, this time with
//!    `--resume-from <path>`: `run_resumed` reads the file back,
//!    rebuilds `State` via `state::State::from_resume` (which
//!    reconstructs each server-pane around its *already-open* PTY
//!    master fd via `term::ServerPane::from_inherited`, instead of
//!    spawning fresh), and re-wraps the inherited listener fd as a
//!    `tokio::net::UnixListener` instead of binding a new one.
//!
//! Residual risk, not eliminated by the above: if the new binary panics
//! very early in `run_resumed` -- before it finishes re-wrapping the
//! inherited fds as owned resources -- the process exits and closes
//! every fd, killing the PTYs this was trying to protect. There's no
//! practical way to fully eliminate that for a personal tool without a
//! much larger validate-before-committing mechanism (spawning the
//! candidate binary as a real subprocess first to prove it can at least
//! parse the resume file); accepted as-is for v1.
//!
//! Connections themselves are *not* inherited (only the listening
//! socket fd is) -- each already-connected client's accepted-socket fd
//! is left behind, so every `dimax attach` sees its connection drop and
//! must reconnect (see `tui::mod`'s reconnect loop).

pub mod pinned_dirs;
pub mod session_persistence;
pub mod state;

use crate::protocol::{
    self, ClientPane, Event, Request, Response, ServerMessage, ServerPaneId, WorkspaceId,
};
use crate::term::ServerPaneEvent;
use state::{CloseTabResult, State, SubscriberId};
use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
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
/// fallback for the crash case this handler can't catch. Used both by
/// `dimax attach`'s auto-spawn path and directly by integration tests
/// with a temp path.
///
/// `enable_session_persistence` gates `session_persistence`'s whole
/// feature (module doc for the full rationale): `true` restores
/// whatever recognized sessions the last unclean shutdown left behind
/// and starts the periodic on-disk snapshot task in [`start`]; `false`
/// skips both entirely. Every real caller (`main.rs`) passes `true`;
/// every test-only caller passes `false` deliberately -- these spin up
/// throwaway daemons against the developer's *real* `$HOME`/
/// `$XDG_CONFIG_HOME` (no fake path, unlike `pinned_dirs`' own
/// dedicated tests), so leaving this enabled by default would have
/// every test run silently spawn real processes from -- and overwrite
/// -- whatever the user's actual daemon had legitimately persisted.
pub async fn run(
    socket_path: std::path::PathBuf,
    enable_session_persistence: bool,
) -> anyhow::Result<Daemon> {
    let listener = bind_socket(&socket_path).await?;
    let mut state = State::new();
    if enable_session_persistence {
        state.restore_sessions_from_disk();
    }
    start(state, listener, socket_path, enable_session_persistence)
}

/// The other side of a hot reload's `execve` (module doc "Hot reload"):
/// rebuild `State` from the snapshot `resume_path` points to, instead of
/// starting fresh, and re-wrap the listener fd that snapshot recorded
/// (inherited across the exec, still open, still bound) instead of
/// binding a new one -- `bind_socket`'s stale-file detection would
/// otherwise see this same daemon's own still-live socket file and
/// (correctly, for the normal case) refuse to touch it.
///
/// # Safety
/// `resume_path`'s `listener_fd` must be a valid, still-open,
/// non-blocking-compatible listening Unix socket fd this process
/// inherited -- true for any file produced by `prepare_reload` and
/// carried across `execve` before this ever runs, never otherwise.
pub async fn run_resumed(
    socket_path: std::path::PathBuf,
    resume_path: std::path::PathBuf,
) -> anyhow::Result<Daemon> {
    let bytes = std::fs::read(&resume_path)
        .map_err(|e| anyhow::anyhow!("reading resume state {resume_path:?}: {e}"))?;
    let file: ReloadFile = serde_json::from_slice(&bytes)
        .map_err(|e| anyhow::anyhow!("parsing resume state {resume_path:?}: {e}"))?;
    let _ = std::fs::remove_file(&resume_path);
    let std_listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(file.listener_fd) };
    std_listener.set_nonblocking(true)?;
    let listener = UnixListener::from_std(std_listener)?;
    let state = State::from_resume(file.state)?;
    // Always on: a resumed daemon is never a test daemon (see `run`'s
    // doc comment) -- there's no throwaway-daemon-pollutes-real-`$HOME`
    // risk to guard against here, and every pane this resumed already
    // carries across via its live fd deserves the same crash-safety
    // net going forward as one restored from scratch would.
    start(state, listener, socket_path, true)
}

/// Shared tail of [`run`]/[`run_resumed`]: install the signal handler,
/// spin up the grid-broadcast and accept-loop tasks. `state` is either
/// freshly constructed or rebuilt from a resume snapshot; `listener` is
/// either freshly bound or an inherited fd re-wrapped -- identical from
/// here on either way. `enable_session_persistence` (see [`run`]'s doc
/// comment) additionally gates the periodic on-disk session-snapshot
/// task started below.
fn start(
    mut inner_state: State,
    listener: UnixListener,
    socket_path: std::path::PathBuf,
    enable_session_persistence: bool,
) -> anyhow::Result<Daemon> {
    spawn_cleanup_on_signal(socket_path.clone(), enable_session_persistence)?;
    // Claim the pane-event stream exactly once, right here at start-up
    // (module doc / `state::State::take_pane_events` doc comment) — this
    // is the *only* caller, so `expect` is warranted: a `None` here would
    // mean this function ran twice against the same `State`, which can't
    // happen since `State::new()`/`State::from_resume` are constructed
    // fresh above.
    let mut pane_events = inner_state
        .take_pane_events()
        .expect("freshly constructed State always has its pane-event receiver available");
    let reload = Arc::new(ReloadHandle {
        socket_path: socket_path.clone(),
        listener_fd: listener.as_raw_fd(),
        pending: Mutex::new(None),
    });
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
            // Lock only long enough to check subscribers + snapshot (see
            // `broadcast_grid_prepare`'s doc comment); the lock is
            // released before the expensive serialize+push below, so a
            // burst of PTY output on one pane no longer blocks every
            // other request the daemon is handling for the whole
            // duration of each broadcast.
            let prepared = {
                let state = drain_state.lock().await;
                broadcast_grid_prepare(&state, id)
            };
            if let Some(broadcast) = prepared {
                broadcast_grid_send(&drain_registry, broadcast).await;
            }
        }
    });

    // Periodic on-disk snapshot of every recognized-tool session (see
    // `session_persistence`'s module doc) -- the safety net a genuine
    // crash/power-loss needs, since nothing about that kind of death
    // gives this process a chance to save anything on its way out
    // (unlike `spawn_cleanup_on_signal`'s graceful-shutdown path, which
    // instead *deletes* this same file). 30s bounds how stale the
    // snapshot can be without costing anything noticeable -- this is a
    // best-effort recovery aid, not a transactional log.
    if enable_session_persistence {
        let snapshot_state = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                tick.tick().await;
                let sessions = snapshot_state.lock().await.restorable_sessions();
                session_persistence::save(&sessions);
            }
        });
    }

    let path_for_task = socket_path.clone();
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let state = state.clone();
                    let registry = registry.clone();
                    let reload = reload.clone();
                    let id = next_subscriber_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    tokio::spawn(handle_connection(stream, state, registry, reload, id));
                }
                Err(err) => {
                    tracing_lite_log(&format!("accept error on {path_for_task:?}: {err}"));
                }
            }
        }
    });

    Ok(Daemon { socket_path })
}

/// Process-wide handle for triggering a hot reload (module doc "Hot
/// reload") -- one instance per `start` call, cloned into every
/// connection so any connection's `DaemonReload` request can arrange
/// the re-exec. `pending` is `Some` for the brief window between
/// `dispatch` preparing a reload and `handle_connection`'s loop actually
/// performing it, right after writing that request's `Ack` response --
/// see both call sites' doc comments for why the ordering matters.
struct ReloadHandle {
    socket_path: PathBuf,
    listener_fd: RawFd,
    pending: Mutex<Option<PathBuf>>,
}

/// Everything `run_resumed` needs besides what's already inside
/// `state::ResumeState` -- just the listening socket's fd, which lives
/// at the daemon-process level (`start`'s `listener` parameter), not
/// inside `State` itself. Serialized to a temp file next to the socket,
/// read back on the other side of the exec that carries it across (fd
/// *numbers* are stable across `execve` for anything that survives
/// `FD_CLOEXEC` -- see `prepare_reload`).
#[derive(serde::Serialize, serde::Deserialize)]
struct ReloadFile {
    listener_fd: RawFd,
    state: state::ResumeState,
}

fn resume_file_path(socket_path: &Path) -> PathBuf {
    let mut path = socket_path.as_os_str().to_owned();
    path.push(".resume.json");
    PathBuf::from(path)
}

/// Clear `FD_CLOEXEC` on `fd` so it survives the `execve` in
/// `perform_reexec` -- everything the kernel would otherwise close the
/// instant the new process image starts. See module doc "Hot reload"
/// step 2.
fn clear_cloexec(fd: RawFd) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Snapshot `state`, clear `FD_CLOEXEC` on every fd that must survive
/// the exec, and write the resume file -- everything `DaemonReload`'s
/// handler needs to do *before* responding, so the response write and
/// the actual `execve` (in `handle_connection`'s loop, after that write
/// succeeds) can stay decoupled from each other. Returns the resume
/// file's path on success.
async fn prepare_reload(
    state: &Arc<Mutex<State>>,
    reload: &ReloadHandle,
) -> anyhow::Result<PathBuf> {
    let resume = state.lock().await.to_resume();
    for fd in resume.pty_fds() {
        clear_cloexec(fd)
            .map_err(|e| anyhow::anyhow!("clearing FD_CLOEXEC on pty fd {fd}: {e}"))?;
    }
    clear_cloexec(reload.listener_fd).map_err(|e| {
        anyhow::anyhow!(
            "clearing FD_CLOEXEC on listener fd {}: {e}",
            reload.listener_fd
        )
    })?;
    let path = resume_file_path(&reload.socket_path);
    let file = ReloadFile {
        listener_fd: reload.listener_fd,
        state: resume,
    };
    let json = serde_json::to_vec(&file)?;
    std::fs::write(&path, json)?;
    Ok(path)
}

/// Replace this process's own image with a fresh invocation of the same
/// binary, `--resume-from resume_path` -- see module doc "Hot reload"
/// step 4. Only ever called from `handle_connection`'s loop, and only
/// *after* that loop has already written the `Ack` response for the
/// `DaemonReload` request that produced `resume_path` -- `execve`
/// succeeding means this process never executes another line of Rust
/// code, so anything that needs to happen for that request has to
/// happen before this call, not after. On success, never returns.
/// Failure (this process survives, still running the old code) is
/// returned rather than panicked on -- e.g. the binary disappearing
/// between build and reload is a plausible, recoverable-by-just-trying-
/// again failure, not a reason to take the whole daemon down.
fn perform_reexec(resume_path: &Path) -> std::io::Error {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => return e,
    };
    std::process::Command::new(exe)
        .arg("daemon")
        .arg("--resume-from")
        .arg(resume_path)
        .exec()
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

/// Install a `SIGTERM`/`SIGINT` handler that unlinks `socket_path` before
/// the process exits, so a graceful shutdown (a normal `kill`, or
/// `Ctrl-C` to a foreground `dimax daemon`) leaves no stale file for
/// `bind_socket`'s fallback to need to clean up next time. Spawns a
/// background task that waits for either signal once, removes the file
/// (best-effort — nothing further to do if that fails), and exits the
/// process.
///
/// When `clear_session_persistence` is set, also deletes
/// `session_persistence`'s snapshot file first -- a *graceful* shutdown
/// reaching this handler at all is exactly the signal that the next
/// fresh start should NOT auto-restore anything (see that module's doc
/// comment): only a death that never reaches here (a crash, `SIGKILL`,
/// real power loss) leaves the file behind, which is what makes finding
/// it at the next start meaningful in the first place.
fn spawn_cleanup_on_signal(
    socket_path: PathBuf,
    clear_session_persistence: bool,
) -> anyhow::Result<()> {
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    tokio::spawn(async move {
        tokio::select! {
            _ = sigterm.recv() => {}
            _ = sigint.recv() => {}
        }
        let _ = std::fs::remove_file(&socket_path);
        if clear_session_persistence {
            session_persistence::clear();
        }
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
/// nothing extra here. Known limitation: unlike a true double-fork, the
/// child stays in this process's process group until it exits, so a
/// signal sent to the whole group (e.g. a shell's Ctrl-C to its
/// foreground job) could reach the daemon too; acceptable for v1.
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
    reload: Arc<ReloadHandle>,
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
            &reload,
            subscriber_id,
            &mut subscribed_workspace,
            request,
        )
        .await;
        let write_ok = protocol::framing::write_frame(
            &mut *writer.lock().await,
            &ServerMessage::Response(response),
        )
        .await
        .is_ok();
        // Only *after* the Ack this reload's request produced has
        // actually been written -- see `perform_reexec`'s doc comment
        // for why the ordering is load-bearing, not stylistic. A failed
        // write doesn't skip the reload attempt: the requesting
        // connection may be the one that's gone, not the daemon, and
        // every other connection still deserves the upgrade.
        if let Some(resume_path) = reload.pending.lock().await.take() {
            let err = perform_reexec(&resume_path);
            tracing_lite_log(&format!(
                "hot reload exec failed, staying on current code: {err}"
            ));
        }
        if !write_ok {
            break;
        }
    }

    push_task.abort();
    registry.lock().await.remove(&subscriber_id);
    if let Some(ws) = subscribed_workspace {
        state.lock().await.unsubscribe(subscriber_id, ws);
    }
    state
        .lock()
        .await
        .clear_scroll_offsets_for_subscriber(subscriber_id);
}

/// Apply one request to `state` and produce its response, pushing any
/// resulting `Event`s to other subscribers via `registry`. Split out
/// from `handle_connection` so it can be unit/integration tested without
/// a real socket (call it directly with a `State` and a `Request`).
async fn dispatch(
    state: &Arc<Mutex<State>>,
    registry: &SubscriberRegistry,
    reload: &Arc<ReloadHandle>,
    subscriber_id: SubscriberId,
    subscribed_workspace: &mut Option<protocol::WorkspaceId>,
    request: Request,
) -> Response {
    let _ = subscriber_id;
    match request {
        // See module doc "Hot reload" and `perform_reexec`'s doc comment:
        // this only *prepares* the reload (snapshot + FD_CLOEXEC clearing
        // + resume file) and records where to find it -- the actual
        // execve happens in `handle_connection`'s loop, strictly after
        // this `Ack` has been written to the socket.
        Request::DaemonReload => match prepare_reload(state, reload).await {
            Ok(path) => {
                *reload.pending.lock().await = Some(path);
                Response::Ack
            }
            Err(e) => Response::Error {
                message: format!("preparing hot reload: {e}"),
            },
        },
        Request::ServerSpawn { name, cmd, cwd } => {
            let mut state = state.lock().await;
            ok_or_err(state.server_spawn(name, cmd, cwd), Response::ServerPane)
        }

        Request::ServerKill { target } => {
            let mut state = state.lock().await;
            match state.server_kill(&target) {
                // `server_kill` can unbind a leaf in *any* workspace, not
                // just whichever one the caller happens to be subscribed
                // to (see its doc comment) -- every affected workspace
                // needs its own `LayoutDelta`, or a client viewing one of
                // the others keeps showing a dangling reference to a
                // pane that no longer exists.
                Ok(affected) => {
                    for ws_id in affected {
                        broadcast_layout(&state, registry, ws_id).await;
                    }
                    Response::Ack
                }
                Err(err) => Response::Error {
                    message: err.to_string(),
                },
            }
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
/// snapshot()` is a plain cell-grid walk-and-clone. Returns `None`
/// immediately, without touching the grid at all, if nobody is
/// subscribed — a pane nobody is currently viewing (e.g. producing
/// background scroll in an unwatched workspace) should cost nothing, not
/// even a wasted snapshot.
///
/// Deliberately does NOT serialize the snapshot to `ServerMessage`'s
/// wire bytes here — that's `broadcast_grid_send`'s job, called only
/// after the caller has released the state lock. Serializing a full grid
/// to JSON is the more expensive step of the two (~1.8ms measured for an
/// 85x246 pane in a release build — `cargo test`'s default debug build
/// measures roughly 30x higher than this and does not reflect what the
/// installed daemon binary actually costs; always re-measure with
/// `cargo test --release` before trusting a number here) — doing it
/// while every other request in the daemon is blocked on the same global
/// lock is what caused dimax to visibly freeze (stop responding to
/// keystrokes in any other pane) whenever a watched pane produced output
/// rapidly enough (e.g. an animated startup banner).
fn broadcast_grid_prepare(state: &State, server_pane: ServerPaneId) -> Option<GridBroadcast> {
    let subscribers = state.subscribers_for_server_pane(server_pane);
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

fn tracing_lite_log(msg: &str) {
    eprintln!("[dimax-daemon] {msg}");
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
        // `false`: a test daemon runs against the developer's real
        // `$HOME`/`$XDG_CONFIG_HOME` (no fake path, unlike `pinned_dirs`'
        // own tests) -- enabling session persistence here would spawn
        // real processes from, and overwrite, whatever the user's real
        // daemon has actually persisted. See `run`'s doc comment.
        run(path.clone(), false)
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
        // `false`: see `start_daemon`'s comment above.
        run(path.clone(), false)
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

    /// The state-level unbind above is necessary but not sufficient: a
    /// connection that was *already* subscribed to the other affected
    /// workspace -- and never re-subscribes -- must also be told live,
    /// via a pushed `LayoutDelta`, or its cached tree keeps showing a
    /// dangling reference to a pane that no longer exists: cycling onto
    /// that tab renders nothing (there's no grid to fetch), and the pane
    /// can never show up again in `server_list`/the attach menu, since
    /// both only ever look at panes still in the pool. Regression test
    /// for exactly that gap: `ServerKill`'s dispatch used to call
    /// `state.server_kill` and return `Ack` without broadcasting
    /// anything at all, so only a workspace the *caller* happened to be
    /// subscribed to (or a fresh `Subscribe`, as in the test above) ever
    /// saw the change.
    #[tokio::test]
    async fn server_kill_broadcasts_layout_delta_to_every_affected_workspace() {
        let guard = start_daemon().await;
        let mut conn = TestConn::connect(&guard.0).await;

        let server_pane = match conn
            .request(Request::ServerSpawn {
                name: None,
                cmd: Some("cat".to_string()),
                cwd: None,
            })
            .await
        {
            Response::ServerPane(info) => info.id,
            other => panic!("expected ServerPane, got {other:?}"),
        };

        // Bind the same server-pane into two different workspaces --
        // e.g. viewing one session from two windows at once. Nothing
        // stops this: `client_spawn`/`client_bind`/`client_add_tab`
        // never check whether `target` is already bound elsewhere.
        let _ = conn
            .request(Request::ClientSpawn {
                workspace: "1".to_string(),
                split_of: None,
                dir: None,
                bind: Some(server_pane.to_string()),
            })
            .await;
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

        // A passive viewer of workspace 2 -- never the one issuing the
        // kill below, and never re-subscribing afterward, so the only
        // way it can learn its leaf just got unbound is a pushed event.
        let mut viewer = TestConn::connect(&guard.0).await;
        match viewer
            .request(Request::Subscribe {
                workspace: ws2.to_string(),
            })
            .await
        {
            Response::Snapshot { .. } => {}
            other => panic!("expected Snapshot, got {other:?}"),
        }

        // `conn` never subscribed to ws2 at all -- matching a bare
        // `dimax server kill` call with no workspace context of its own.
        match conn
            .request(Request::ServerKill {
                target: server_pane.to_string(),
            })
            .await
        {
            Response::Ack => {}
            other => panic!("expected Ack, got {other:?}"),
        }

        let event = viewer
            .read_event(Duration::from_secs(2))
            .await
            .expect("expected a LayoutDelta push for workspace 2 within 2s");
        match event {
            Event::LayoutDelta { workspace, tree } => {
                assert_eq!(workspace, ws2);
                let tree = tree.expect("workspace 2 should still have its pane");
                let leaf = tree.find(pane2).expect("client-pane should still exist");
                assert_eq!(
                    leaf.active_bound(),
                    None,
                    "viewer must see the leaf unbound live, not just on a fresh Subscribe"
                );
            }
            other => panic!("expected LayoutDelta, got {other:?}"),
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
            })
            .await
        {
            Response::ServerPane(info) => info.id,
            other => panic!("expected ServerPane, got {other:?}"),
        };

        match conn
            .request(Request::ServerRename {
                target: server_pane.to_string(),
                new_name: "renamed-from-menu".to_string(),
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
    /// `Changed` event serialized the busy pane's grid to JSON (a few ms
    /// per event even in a release build, for a large pane -- see
    /// `broadcast_grid_prepare`'s doc comment) while holding the daemon's
    /// one global state lock -- a fast-scrolling pane could keep that
    /// lock saturated closely enough to starve every other request,
    /// which is what made typing in an unrelated pane appear to freeze.
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
        // they're not queueing behind a busy pane's own per-event
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
}
