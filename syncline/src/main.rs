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
        Commands::Sync { folder, url, .. } => {
            syncline::client::app::run_client(folder, url).await?;
        }
    }

    Ok(())
}
