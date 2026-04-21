use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

fn cli_styles() -> Styles {
    Styles::styled()
        .header(AnsiColor::Green.on_default() | Effects::BOLD)
        .usage(AnsiColor::Green.on_default() | Effects::BOLD)
        .literal(AnsiColor::Cyan.on_default() | Effects::BOLD)
        .placeholder(AnsiColor::Cyan.on_default())
}

#[derive(Parser, Debug)]
#[command(
    name = "syncline",
    version,
    about = "⚡ Syncline: A modern synchronization engine and workspace.",
    long_about = None,
    styles = cli_styles()
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Start the Syncline Server
    Server {
        /// Port to listen on
        #[arg(short, long, default_value = "3030")]
        port: u16,

        /// Database path or connection string
        #[arg(short, long, default_value = "syncline.db")]
        db_path: String,

        /// Log level (error, warn, info, debug, trace)
        #[arg(short, long, default_value = "info")]
        log_level: String,

        /// Optional file to redirect logs to
        #[arg(long)]
        log_file: Option<PathBuf>,
    },
    /// Migrate a v0 vault on disk to the v1 manifest layout.
    /// Idempotent — safe to run on an already-migrated vault.
    Migrate {
        /// Folder containing the `.syncline/` directory to migrate.
        #[arg(short, long, default_value = ".")]
        folder: PathBuf,

        /// Log level (error, warn, info, debug, trace)
        #[arg(long, default_value = "info")]
        log_level: String,

        /// Optional file to redirect logs to
        #[arg(long)]
        log_file: Option<PathBuf>,
    },
    /// Start the Syncline Client to sync a folder
    Sync {
        /// Folder to watch and sync
        #[arg(short, long, default_value = ".")]
        folder: PathBuf,

        /// URL of the Syncline server
        #[arg(
            short,
            long,
            default_value = "ws://127.0.0.1:3030/sync",
            env = "SYNCLINE_URL"
        )]
        url: String,

        /// Client name used in conflict filenames (e.g. "laptop", "work-mac").
        /// Defaults to hostname + short unique ID, persisted in .syncline/client_id.
        #[arg(short = 'n', long)]
        name: Option<String>,

        /// Log level (error, warn, info, debug, trace)
        #[arg(long, default_value = "info")]
        log_level: String,

        /// Optional file to redirect logs to
        #[arg(long)]
        log_file: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let (log_level, log_file) = match &cli.command {
        Commands::Server {
            log_level,
            log_file,
            ..
        } => (log_level, log_file),
        Commands::Migrate {
            log_level,
            log_file,
            ..
        } => (log_level, log_file),
        Commands::Sync {
            log_level,
            log_file,
            ..
        } => (log_level, log_file),
    };

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(log_level));

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .without_time();

    use tracing_subscriber::layer::SubscriberExt;
    let subscriber = tracing_subscriber::registry().with(filter).with(fmt_layer);

    if let Some(log_file) = log_file {
        let file_appender = tracing_appender::rolling::never(
            log_file
                .parent()
                .unwrap_or_else(|| std::path::Path::new("")),
            log_file.file_name().unwrap(),
        );
        let file_layer = tracing_subscriber::fmt::layer()
            .with_writer(file_appender)
            .with_ansi(false);
        tracing_subscriber::util::SubscriberInitExt::init(subscriber.with(file_layer));
    } else {
        tracing_subscriber::util::SubscriberInitExt::init(subscriber);
    }

    match cli.command {
        Commands::Server { port, db_path, .. } => {
            use colored::Colorize;
            tracing::info!("{} Starting Syncline server...", "🚀".green());
            tracing::info!("{} Port: {}", "🔌".blue(), port);
            tracing::info!("{} Database: {}", "💾".cyan(), db_path);

            // Convert db_path to sqlite connection string
            let connection_string = if db_path.starts_with("sqlite:") {
                db_path.clone()
            } else {
                format!("sqlite://{}?mode=rwc", db_path)
            };

            let db = syncline::server::db::Db::new(&connection_string).await?;
            syncline::server::server::run_server(db, port).await?;
        }
        Commands::Migrate { folder, .. } => {
            use colored::Colorize;
            tracing::info!(
                "{} Migrating vault at {} to v1 layout…",
                "📦".cyan(),
                folder.display()
            );
            let report = tokio::task::spawn_blocking(move || {
                syncline::v1::migrate_vault_on_disk(&folder)
            })
            .await??;

            if report.already_migrated {
                tracing::info!(
                    "{} Vault at {} already reports version 1 — nothing to do.",
                    "✅".green(),
                    report.vault_root.display()
                );
            } else {
                tracing::info!(
                    "{} Migrated {} text files, {} binary files, {} directories (actor {}).",
                    "✅".green(),
                    report.text_files,
                    report.binary_files,
                    report.directories,
                    report.actor_id
                );
                if !report.warnings.is_empty() {
                    tracing::warn!("{} {} warnings during migration:", "⚠️".yellow(), report.warnings.len());
                    for w in &report.warnings {
                        tracing::warn!("  - {}", w);
                    }
                }
                tracing::info!(
                    "v0 data preserved at .syncline/data.v0.bak; version marker written."
                );
            }
        }
        Commands::Sync { folder, url, name, .. } => {
            syncline::client::app::run_client(folder, url, name).await?;
        }
    }

    Ok(())
}
