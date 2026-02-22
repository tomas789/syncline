use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::Parser;
use colored::Colorize;
use server::db::Db;
use server::server::run_server;
use std::path::PathBuf;
use tracing::info;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

fn cli_styles() -> Styles {
    Styles::styled()
        .header(AnsiColor::Green.on_default() | Effects::BOLD)
        .usage(AnsiColor::Green.on_default() | Effects::BOLD)
        .literal(AnsiColor::Cyan.on_default() | Effects::BOLD)
        .placeholder(AnsiColor::Cyan.on_default())
}

#[derive(Parser, Debug)]
#[command(
    version,
    about = "⚡ Syncline Server: A modern synchronization engine.",
    long_about = None,
    styles = cli_styles()
)]
struct Args {
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // Setup logging
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&args.log_level));

    let fmt_layer = fmt::layer().with_target(false).without_time();

    let subscriber = tracing_subscriber::registry().with(filter).with(fmt_layer);

    if let Some(log_file) = &args.log_file {
        let file_appender = tracing_appender::rolling::never(
            log_file
                .parent()
                .unwrap_or_else(|| std::path::Path::new("")),
            log_file.file_name().unwrap(),
        );
        let file_layer = fmt::layer().with_writer(file_appender).with_ansi(false);
        subscriber.with(file_layer).init();
    } else {
        subscriber.init();
    }

    // Convert db_path to sqlite connection string
    let connection_string = if args.db_path.starts_with("sqlite:") {
        args.db_path.clone()
    } else {
        format!("sqlite://{}?mode=rwc", args.db_path)
    };

    println!(
        "{}",
        r#"
   _____                  ___         
  / ___/__  ______  _____/ (_)___  ___ 
  \__ \/ / / / __ \/ ___/ / / __ \/ _ \
 ___/ / /_/ / / / / /__/ / / / / /  __/
/____/\__, /_/ /_/\___/_/_/_/ /_/\___/ 
     /____/                            
"#
        .cyan()
        .bold()
    );
    println!("  {}\n", "⚡ A modern synchronization engine".green());

    let db = Db::new(&connection_string).await?;

    info!("{} Starting Syncline server...", "🚀".green());
    info!("{} Port: {}", "🔌".blue(), args.port);
    info!("{} Database: {}", "💾".cyan(), connection_string);
    if let Some(f) = &args.log_file {
        info!("{} Log file: {}", "📝".yellow(), f.display());
    }

    run_server(db, args.port).await?;

    Ok(())
}
