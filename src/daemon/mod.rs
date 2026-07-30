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

use crate::protocol::{self, Request, Response, ServerMessage};
use state::State;
use std::sync::Arc;
use tokio::net::UnixListener;
use tokio::sync::Mutex;

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
    let state = Arc::new(Mutex::new(State::new()));

    let path_for_task = socket_path.clone();
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let state = state.clone();
                    tokio::spawn(handle_connection(stream, state));
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
pub fn ensure_running(socket_path: &std::path::Path) -> anyhow::Result<()> {
    let _ = socket_path;
    todo!("double-fork + detach `dimux daemon` if the socket is not connectable")
}

async fn handle_connection(stream: tokio::net::UnixStream, state: Arc<Mutex<State>>) {
    let (mut reader, mut writer) = stream.into_split();
    let mut subscribed_workspace: Option<protocol::WorkspaceId> = None;

    loop {
        let request: Request = match protocol::framing::read_frame(&mut reader).await {
            Ok(r) => r,
            Err(_) => break, // connection closed
        };

        let response = dispatch(&state, &mut subscribed_workspace, request).await;
        if protocol::framing::write_frame(&mut writer, &ServerMessage::Response(response))
            .await
            .is_err()
        {
            break;
        }
    }

    if let Some(ws) = subscribed_workspace {
        state.lock().await.unsubscribe_all_for_connection(ws);
    }
}

/// Apply one request to `state` and produce its response. Split out from
/// `handle_connection` so it can be unit/integration tested without a
/// real socket (call it directly with a `State` and a `Request`).
async fn dispatch(
    state: &Arc<Mutex<State>>,
    subscribed_workspace: &mut Option<protocol::WorkspaceId>,
    request: Request,
) -> Response {
    let _ = (state, subscribed_workspace, request);
    todo!("match on Request variants, call into state::State methods, broadcast Events to subscribers")
}

fn tracing_lite_log(msg: &str) {
    eprintln!("[dimux-daemon] {msg}");
}
