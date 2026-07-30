//! `dimux server ...` / `dimux client ...` subcommands, and the client
//! library both they and the TUI frontend use to talk to the daemon.
//!
//! `Client` is the one place that knows how to open the socket, ensure
//! the daemon is running, and send a `Request`/get a `Response` — the
//! design doc's "the daemon does not distinguish CLI callers from
//! frontend callers" is implemented by both `cli` and `tui` going through
//! this same type.

use crate::protocol::{self, Request, Response, ServerMessage};
use tokio::net::UnixStream;

pub struct Client {
    stream: UnixStream,
}

impl Client {
    /// Connect to the daemon at the default socket path, auto-spawning it
    /// first if it isn't reachable (design doc "Error handling").
    pub async fn connect() -> anyhow::Result<Self> {
        let path = protocol::socket_path();
        if UnixStream::connect(&path).await.is_err() {
            crate::daemon::ensure_running(&path)?;
        }
        let stream = UnixStream::connect(&path).await?;
        Ok(Self { stream })
    }

    /// Send one request, read back its response. Callers that need to
    /// `Subscribe` and then keep reading pushed `Event`s (the TUI) should
    /// use [`Client::into_split`] instead of repeated calls to this.
    pub async fn request(&mut self, req: Request) -> anyhow::Result<Response> {
        protocol::framing::write_frame(&mut self.stream, &req).await?;
        match protocol::framing::read_frame(&mut self.stream).await? {
            ServerMessage::Response(r) => Ok(r),
            ServerMessage::Event(_) => {
                anyhow::bail!("protocol violation: got Event before Response")
            }
        }
    }
}

/// Top-level CLI argument tree. `main.rs` parses into this with `clap`
/// and dispatches to [`run`].
#[derive(clap::Parser)]
#[command(name = "dimux")]
pub enum Cli {
    /// Launch the TUI, attaching to (and auto-starting) the daemon.
    Attach,
    /// Control server-panes.
    Server {
        #[command(subcommand)]
        cmd: ServerCmd,
    },
    /// Control client-panes / workspaces.
    Client {
        #[command(subcommand)]
        cmd: ClientCmd,
    },
    /// Run the daemon in the foreground (used internally by the
    /// auto-spawn path; also useful for debugging).
    Daemon,
}

#[derive(clap::Subcommand)]
pub enum ServerCmd {
    Spawn {
        name: String,
        #[arg(long)]
        cmd: Option<String>,
    },
    Kill { target: String },
    Rename { target: String, new_name: String },
    Ls,
}

#[derive(clap::Subcommand)]
pub enum ClientCmd {
    Spawn {
        workspace: String,
        #[arg(long)]
        split: Option<uuid::Uuid>,
        #[arg(long)]
        dir: Option<SplitDirArg>,
        #[arg(long)]
        bind: Option<String>,
    },
    Close { addr: String },
    Rename { addr: String, new_name: String },
    Bind { addr: String, target: String },
    Ls { workspace: Option<String> },
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum SplitDirArg {
    H,
    V,
}

impl From<SplitDirArg> for protocol::SplitDir {
    fn from(v: SplitDirArg) -> Self {
        match v {
            SplitDirArg::H => protocol::SplitDir::Horizontal,
            SplitDirArg::V => protocol::SplitDir::Vertical,
        }
    }
}

/// Parse a `<workspace>/<pane-id>` CLI address (design doc "CLI surface").
pub fn parse_pane_addr(addr: &str) -> anyhow::Result<(String, uuid::Uuid)> {
    let (workspace, pane) = addr
        .rsplit_once('/')
        .ok_or_else(|| anyhow::anyhow!("expected <workspace>/<pane-id>, got {addr:?}"))?;
    let pane = uuid::Uuid::parse_str(pane)?;
    Ok((workspace.to_string(), pane))
}

/// Execute a parsed `Cli` command against a real daemon connection and
/// print human-readable output, mirroring what `dimux server ls` /
/// `dimux client ls` etc. should show at the terminal. Split out from
/// `main` so it's the one place both the real binary and any CLI-level
/// tests invoke.
pub async fn run(cli: Cli) -> anyhow::Result<()> {
    let _ = cli;
    todo!("dispatch each Cli variant to a Client::request call, format Response for stdout/stderr")
}
