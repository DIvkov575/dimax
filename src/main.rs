use clap::Parser;
use dimax::cli::{self, Args, Cli};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Args::parse().command.unwrap_or(Cli::Attach);
    match cli {
        Cli::Attach => dimax::tui::run().await,
        Cli::Daemon {
            cmd: None,
            resume_from,
        } => {
            let daemon = match resume_from {
                Some(path) => {
                    dimax::daemon::run_resumed(dimax::protocol::socket_path(), path).await?
                }
                None => dimax::daemon::run(dimax::protocol::socket_path(), true).await?,
            };
            let _ = daemon;
            std::future::pending::<()>().await;
            Ok(())
        }
        Cli::Config => cli::run_config().await,
        other => cli::run(other).await,
    }
}
