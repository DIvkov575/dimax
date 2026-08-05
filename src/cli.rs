//! `dimux server ...` / `dimux client ...` subcommands, and the client
//! library both they and the TUI frontend use to talk to the daemon.
//!
//! `Client` is the one place that knows how to open the socket, ensure
//! the daemon is running, and send a `Request`/get a `Response` — the
//! design doc's "the daemon does not distinguish CLI callers from
//! frontend callers" is implemented by both `cli` and `tui` going through
//! this same type.

use crate::protocol::{self, ClientPane, Request, Response, ServerMessage, ServerPaneInfo, ServerPaneStatus};
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

    /// Split into an owned read half and write half for callers (the TUI)
    /// that need to send requests and receive pushed `Event`s concurrently
    /// on the same connection, rather than the strict request/then-response
    /// cycle `request()` assumes.
    pub fn into_split(self) -> (tokio::net::unix::OwnedReadHalf, tokio::net::unix::OwnedWriteHalf) {
        self.stream.into_split()
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
    /// Print a server-pane's current on-screen contents as plain text.
    Read { target: String },
    /// Type text into a server-pane, bypassing any workspace/client-pane
    /// binding.
    Send {
        target: String,
        text: String,
        /// Append a trailing newline after `text`, as if the user typed
        /// it and pressed Enter.
        #[arg(long)]
        enter: bool,
    },
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
    Unbind { addr: String },
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

/// Handle a `Response::Error` (or any other unexpected `Response` that
/// doesn't match what `context` should have produced): print the
/// message to stderr and turn it into an `Err` so `main`'s top-level
/// `anyhow::Result` propagation exits non-zero (design doc "Error
/// handling" — "CLI prints to stderr and exits non-zero"). Never panics,
/// even on a variant mismatch that "shouldn't happen".
fn unexpected_response(context: &str, resp: Response) -> anyhow::Error {
    match resp {
        Response::Error { message } => {
            eprintln!("{message}");
            anyhow::anyhow!(message)
        }
        other => {
            let message = format!("unexpected response to {context}: {other:?}");
            eprintln!("{message}");
            anyhow::anyhow!(message)
        }
    }
}

/// Format one `ServerPaneInfo` as a single scriptable line: `id  name  status  rows x cols`.
fn format_server_pane_line(info: &ServerPaneInfo) -> String {
    let name = info.name.as_deref().unwrap_or("-");
    let status = match info.status {
        ServerPaneStatus::Running => "running",
        ServerPaneStatus::Dead => "dead",
    };
    let process = info.foreground.as_ref().map_or("-", |f| f.process_name.as_str());
    let cwd = info.foreground.as_ref().and_then(|f| f.cwd.as_deref()).unwrap_or("-");
    format!(
        "{}\t{}\t{}\t{}x{}\t{}\t{}",
        info.id, name, status, info.size.rows, info.size.cols, process, cwd
    )
}

/// Format one `ClientPane` as a single scriptable line: `id  name  bound-server-pane-or-dash`.
fn format_client_pane_line(pane: &ClientPane) -> String {
    let name = pane.name.as_deref().unwrap_or("-");
    let bound = pane
        .bound
        .map(|id| id.to_string())
        .unwrap_or_else(|| "-".to_string());
    format!("{}\t{}\t{}", pane.id, name, bound)
}

/// Execute a parsed `Cli` command against a real daemon connection and
/// print human-readable output, mirroring what `dimux server ls` /
/// `dimux client ls` etc. should show at the terminal. Split out from
/// `main` so it's the one place both the real binary and any CLI-level
/// tests invoke.
pub async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli {
        Cli::Server { cmd } => run_server(cmd).await,
        Cli::Client { cmd } => run_client(cmd).await,
        // `main.rs` handles `Attach`/`Daemon` itself and never calls
        // `run` with them; a caller doing so anyway is a `main.rs` bug,
        // not something to service here.
        Cli::Attach | Cli::Daemon => {
            anyhow::bail!("cli::run called with Attach/Daemon, which main.rs should handle directly")
        }
    }
}

async fn run_server(cmd: ServerCmd) -> anyhow::Result<()> {
    let mut client = Client::connect().await?;
    match cmd {
        ServerCmd::Spawn { name, cmd } => {
            let req = Request::ServerSpawn { name: Some(name), cmd };
            match client.request(req).await? {
                Response::ServerPane(info) => {
                    let label = info.name.as_deref().unwrap_or("-");
                    println!("spawned server-pane {label} ({})", info.id);
                    Ok(())
                }
                other => Err(unexpected_response("server spawn", other)),
            }
        }
        ServerCmd::Kill { target } => {
            let req = Request::ServerKill { target: target.clone() };
            match client.request(req).await? {
                Response::Ack => {
                    println!("killed server-pane {target}");
                    Ok(())
                }
                other => Err(unexpected_response("server kill", other)),
            }
        }
        ServerCmd::Rename { target, new_name } => {
            let req = Request::ServerRename { target: target.clone(), new_name: new_name.clone() };
            match client.request(req).await? {
                Response::Ack => {
                    println!("renamed server-pane {target} to {new_name}");
                    Ok(())
                }
                other => Err(unexpected_response("server rename", other)),
            }
        }
        ServerCmd::Ls => match client.request(Request::ServerList).await? {
            Response::ServerPaneList(panes) => {
                for info in &panes {
                    println!("{}", format_server_pane_line(info));
                }
                Ok(())
            }
            other => Err(unexpected_response("server ls", other)),
        },
        ServerCmd::Read { target } => {
            let req = Request::ServerRead { target };
            match client.request(req).await? {
                Response::ServerReadOutput { text } => {
                    println!("{text}");
                    Ok(())
                }
                other => Err(unexpected_response("server read", other)),
            }
        }
        ServerCmd::Send { target, text, enter } => {
            let req = Request::ServerSend { target: target.clone(), text, enter };
            match client.request(req).await? {
                Response::Ack => {
                    println!("sent to server-pane {target}");
                    Ok(())
                }
                other => Err(unexpected_response("server send", other)),
            }
        }
    }
}

async fn run_client(cmd: ClientCmd) -> anyhow::Result<()> {
    let mut client = Client::connect().await?;
    match cmd {
        ClientCmd::Spawn { workspace, split, dir, bind } => {
            let req = Request::ClientSpawn {
                workspace,
                split_of: split,
                dir: dir.map(Into::into),
                bind,
            };
            match client.request(req).await? {
                Response::ClientPaneCreated { workspace, pane } => {
                    println!("{workspace}/{pane}");
                    Ok(())
                }
                other => Err(unexpected_response("client spawn", other)),
            }
        }
        ClientCmd::Close { addr } => {
            let (workspace, pane) = parse_pane_addr(&addr)?;
            let req = Request::ClientClose { workspace, pane };
            match client.request(req).await? {
                Response::Ack => {
                    println!("closed {addr}");
                    Ok(())
                }
                other => Err(unexpected_response("client close", other)),
            }
        }
        ClientCmd::Rename { addr, new_name } => {
            let (workspace, pane) = parse_pane_addr(&addr)?;
            let req = Request::ClientRename { workspace, pane, new_name: new_name.clone() };
            match client.request(req).await? {
                Response::Ack => {
                    println!("renamed {addr} to {new_name}");
                    Ok(())
                }
                other => Err(unexpected_response("client rename", other)),
            }
        }
        ClientCmd::Bind { addr, target } => {
            let (workspace, pane) = parse_pane_addr(&addr)?;
            let req = Request::ClientBind { workspace, pane, target: target.clone() };
            match client.request(req).await? {
                Response::Ack => {
                    println!("bound {addr} to {target}");
                    Ok(())
                }
                other => Err(unexpected_response("client bind", other)),
            }
        }
        ClientCmd::Unbind { addr } => {
            let (workspace, pane) = parse_pane_addr(&addr)?;
            let req = Request::ClientUnbind { workspace, pane };
            match client.request(req).await? {
                Response::Ack => {
                    println!("unbound {addr}");
                    Ok(())
                }
                other => Err(unexpected_response("client unbind", other)),
            }
        }
        ClientCmd::Ls { workspace } => {
            let req = Request::ClientList { workspace };
            match client.request(req).await? {
                Response::ClientPaneList { workspace, panes } => {
                    for pane in &panes {
                        println!("{workspace}\t{}", format_client_pane_line(pane));
                    }
                    Ok(())
                }
                other => Err(unexpected_response("client ls", other)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ClientPane, ForegroundProcessInfo, ServerPaneStatus, Size};
    use uuid::Uuid;

    #[test]
    fn parse_pane_addr_valid() {
        let id = Uuid::new_v4();
        let addr = format!("dev/{id}");
        let (workspace, pane) = parse_pane_addr(&addr).unwrap();
        assert_eq!(workspace, "dev");
        assert_eq!(pane, id);
    }

    #[test]
    fn parse_pane_addr_missing_slash() {
        assert!(parse_pane_addr("not-an-address").is_err());
    }

    #[test]
    fn parse_pane_addr_empty_string() {
        assert!(parse_pane_addr("").is_err());
    }

    #[test]
    fn parse_pane_addr_invalid_uuid() {
        assert!(parse_pane_addr("dev/not-a-uuid").is_err());
    }

    #[test]
    fn parse_pane_addr_workspace_with_slash_in_name() {
        // rsplit_once takes the *last* `/`, so a workspace name containing
        // `/` is preserved as part of the workspace segment.
        let id = Uuid::new_v4();
        let addr = format!("team/dev/{id}");
        let (workspace, pane) = parse_pane_addr(&addr).unwrap();
        assert_eq!(workspace, "team/dev");
        assert_eq!(pane, id);
    }

    #[test]
    fn format_server_pane_line_with_name() {
        let info = ServerPaneInfo {
            id: Uuid::nil(),
            name: Some("editor".to_string()),
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Running,
            foreground: Some(ForegroundProcessInfo {
                process_name: "vim".to_string(),
                cwd: Some("/home/dev".to_string()),
            }),
        };
        let line = format_server_pane_line(&info);
        assert_eq!(
            line,
            format!("{}\teditor\trunning\t24x80\tvim\t/home/dev", Uuid::nil())
        );
    }

    #[test]
    fn format_server_pane_line_without_name_and_dead() {
        let info = ServerPaneInfo {
            id: Uuid::nil(),
            name: None,
            size: Size { rows: 10, cols: 20 },
            status: ServerPaneStatus::Dead,
            foreground: None,
        };
        let line = format_server_pane_line(&info);
        assert_eq!(line, format!("{}\t-\tdead\t10x20\t-\t-", Uuid::nil()));
    }

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
}
