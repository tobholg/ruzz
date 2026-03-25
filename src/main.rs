mod config;
mod dashboard;
mod import;
mod schema;
mod search;
mod server;

use std::sync::Arc;
use std::time::Instant;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "ruzz", about = "Fast fuzzy search engine")]
struct Cli {
    /// Path to config file
    #[arg(short, long, default_value = "ruzz.toml")]
    config: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Import CSV sources into the index
    Import,
    /// Start the search API server
    Serve,
    /// Import then serve
    Run,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = config::Config::load(std::path::Path::new(&cli.config))?;
    let config = Arc::new(config);

    match cli.command {
        Command::Import => {
            import::run_import(&config)?;
        }
        Command::Serve => {
            serve(config).await?;
        }
        Command::Run => {
            import::run_import(&config)?;
            println!();
            serve(config).await?;
        }
    }

    Ok(())
}

async fn serve(config: Arc<config::Config>) -> anyhow::Result<()> {
    let engine = search::SearchEngine::open(config.clone())?;
    let port = config.server.port;

    let state = Arc::new(server::AppState {
        engine,
        started_at: Instant::now(),
    });

    let app = server::create_router(state);

    let addr = format!("0.0.0.0:{}", port);
    println!("⚡ ruzz server listening on http://localhost:{}", port);
    println!("  /search?q=abax&country_code=NO&limit=20");
    println!("  /lookup?country_code=NO&org_number=936512054");
    println!("  /stats");
    println!("  /health");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
