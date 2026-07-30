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
use state::{State, SubscriberId};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::UnixListener;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Mutex;

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
    let state = Arc::new(Mutex::new(State::new()));
    let registry: SubscriberRegistry = Arc::new(Mutex::new(HashMap::new()));
    let next_subscriber_id = Arc::new(std::sync::atomic::AtomicU64::new(0));

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
pub fn ensure_running(socket_path: &std::path::Path) -> anyhow::Result<()> {
    let _ = socket_path;
    todo!("double-fork + detach `dimux daemon` if the socket is not connectable")
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
    let _ = (state, registry, subscriber_id, subscribed_workspace, request);
    todo!(
        "match on Request variants, call into state::State methods, then for \
         mutations use state.subscribers_for_workspace/subscribers_for_server_pane \
         plus `registry` to push the corresponding Event to each affected subscriber \
         (skip pushing back to `subscriber_id` for its own request if that matters \
         for a given variant). Subscribe/Unsubscribe requests additionally update \
         `*subscribed_workspace` so cleanup on disconnect (above) knows what to release."
    )
}

fn tracing_lite_log(msg: &str) {
    eprintln!("[dimux-daemon] {msg}");
}
