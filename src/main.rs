use clap::Parser;
use dimux::cli::{self, Cli};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli {
        Cli::Attach => dimux::tui::run().await,
        Cli::Daemon => {
            let daemon = dimux::daemon::run(dimux::protocol::socket_path()).await?;
            let _ = daemon;
            std::future::pending::<()>().await;
            Ok(())
        }
        other => cli::run(other).await,
    }
}
