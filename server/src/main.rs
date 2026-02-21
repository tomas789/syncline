use clap::Parser;
use server::db::Db;
use server::server::run_server;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(short, long, default_value = "3030")]
    port: u16,

    #[arg(short, long, default_value = "syncline.db")]
    db_path: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logging
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = Args::parse();

    // Convert db_path to sqlite connection string
    // If it's a file path, we need to ensure it has sqlite:// prefix.
    let connection_string = if args.db_path.starts_with("sqlite:") {
        args.db_path
    } else {
        format!("sqlite://{}?mode=rwc", args.db_path)
    };

    // Create DB (creates file if not exists)
    let db = Db::new(&connection_string).await?;

    log::info!("Starting Syncline server on port {}", args.port);
    log::info!("Using database: {}", connection_string);

    run_server(db, args.port).await?;

    Ok(())
}
