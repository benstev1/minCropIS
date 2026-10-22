//! Serves the minIS web app locally. See `minis::web`.

use anyhow::Context;
use clap::Parser;
use std::path::PathBuf;
/// Serve the minIS web app.
#[derive(Parser)]
struct Args {
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:8765")]
    addr: String,

    /// Directory for uploads and results (default: a `minis-web` folder in the system temp dir).
    #[arg(long)]
    data_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let data_dir = args
        .data_dir
        .unwrap_or_else(|| std::env::temp_dir().join("minis-web"));
    let listener = tokio::net::TcpListener::bind(&args.addr)
        .await
        .with_context(|| format!("binding {}", args.addr))?;
    println!("minIS web app on http://{}", listener.local_addr()?);
    println!("files are kept in {}", data_dir.display());
    minis::web::serve(listener, data_dir).await
}
