use ruzz::{config, import, memory, search, server};

use std::sync::Arc;

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
    /// Delete index files no longer referenced by the current commit
    ///
    /// Merging supersedes segments but does not remove them, and until
    /// ruzz 0.2 nothing ever collected the leftovers — existing indexes can
    /// be more than half dead weight. Import does this automatically now;
    /// this reclaims an index built before that, without re-importing.
    Gc,
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
        Command::Gc => {
            import::collect_garbage(&config)?;
        }
    }

    Ok(())
}

async fn serve(config: Arc<config::Config>) -> anyhow::Result<()> {
    let engine = search::SearchEngine::open(config.clone())?;

    // Apply memory budget
    memory::apply_memory_budget(&config.server.index_path, &config.server.memory_budget);

    let port = config.server.port;

    let state = Arc::new(server::AppState::new(engine));

    let app = server::create_router(state);

    let bind = config.server.bind.trim();
    let addr = format!("{}:{}", bind, port);
    println!("⚡ ruzz server listening on http://localhost:{}", port);
    if bind != "0.0.0.0" {
        println!(
            "  bound to {} — reachable only through whatever proxies it",
            bind
        );
    }
    println!("  /search?q=abax&country_code=NO&limit=20");
    println!("  /lookup?country_code=NO&org_number=936512054");
    println!("  /stats");
    println!("  /health");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
