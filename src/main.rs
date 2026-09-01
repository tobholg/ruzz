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
    /// Import the configured sources (CSV or JSONL) into the index
    Import {
        /// Dry run: parse every source and report row counts, rejected
        /// rows, unmapped columns, and empty/duplicate primary keys —
        /// without touching the index
        #[arg(long)]
        check: bool,
    },
    /// Upsert rows from delta CSV files into the existing index
    ///
    /// Needs primary_key under [schema]. Each row replaces any document
    /// carrying the same key, in one commit — no full re-import, and a
    /// running server picks the change up on its own.
    Update {
        /// Delta files (CSV or JSONL, optionally .gz), in the same shape as
        /// a configured source. Pass "-" to read a delta from stdin.
        files: Vec<std::path::PathBuf>,
        /// The [[sources]] entry (by path) whose mapping and defaults the
        /// delta files follow. With several sources configured, an
        /// unambiguous delta is matched by its columns automatically.
        #[arg(long)]
        like: Option<std::path::PathBuf>,
        /// Aligned JSONL sidecar for the delta file (required when
        /// [store] source = "sidecar"; pairs with exactly one delta file)
        #[arg(long)]
        sidecar: Option<std::path::PathBuf>,
        /// Delta encoding. Defaults to the file extension — or, for stdin,
        /// to the matched source's format.
        #[arg(long, value_enum)]
        format: Option<ruzz::config::SourceFormat>,
    },
    /// Delete documents by primary key
    ///
    /// Needs primary_key under [schema].
    Delete {
        /// Primary key values to delete
        keys: Vec<String>,
        /// File with one key per line, in addition to any keys given inline
        #[arg(long)]
        file: Option<std::path::PathBuf>,
    },
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
        Command::Import { check } => {
            if check {
                let report = import::run_check(&config)?;
                if report.bad_rows > 0 {
                    anyhow::bail!(
                        "--check found {} row(s) the import would reject",
                        report.bad_rows
                    );
                }
            } else {
                import::run_import(&config)?;
            }
        }
        Command::Update {
            files,
            like,
            sidecar,
            format,
        } => {
            import::run_update(&config, &files, like.as_deref(), sidecar.as_deref(), format)?;
        }
        Command::Delete { keys, file } => {
            let mut keys = keys;
            if let Some(path) = file {
                let text = std::fs::read_to_string(&path)?;
                keys.extend(
                    text.lines()
                        .map(str::trim)
                        .filter(|l| !l.is_empty())
                        .map(String::from),
                );
            }
            import::run_delete(&config, &keys)?;
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
