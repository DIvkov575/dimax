use clap::Parser;
use dimax::cli::{self, Args, Cli};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Args::parse().command.unwrap_or(Cli::Attach);
    match cli {
        Cli::Attach => dimax::tui::run().await,
        Cli::Daemon => {
            let daemon = dimax::daemon::run(dimax::protocol::socket_path()).await?;
            let _ = daemon;
            std::future::pending::<()>().await;
            Ok(())
        }
        Cli::Config => cli::run_config().await,
        other => cli::run(other).await,
    }
}
