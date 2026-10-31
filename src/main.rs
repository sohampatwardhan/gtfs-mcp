use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use gtfs_rt_mcp::{serve_http, serve_stdio, startup_plan, Catalog, GtfsServer, StartupOverrides};

#[derive(Parser)]
#[command(
    name = "gtfs-rt-mcp",
    version,
    about = "MCP server for GTFS static schedules and GTFS-Realtime"
)]
struct Cli {
    /// Id for the startup feed. Overrides GTFS_RT_MCP_FEED_ID. Default `default`.
    #[arg(long, global = true)]
    feed_id: Option<String>,
    /// GTFS static zip URL or filesystem path. Required. Overrides GTFS_RT_MCP_STATIC.
    #[arg(long, global = true)]
    r#static: Option<String>,
    /// TripUpdates protobuf URL or path. Overrides GTFS_RT_MCP_TRIP_UPDATES.
    #[arg(long, global = true)]
    trip_updates: Option<String>,
    /// VehiclePositions protobuf URL or path. Overrides GTFS_RT_MCP_VEHICLE_POSITIONS.
    #[arg(long, global = true)]
    vehicle_positions: Option<String>,
    /// Alerts protobuf URL or path. Overrides GTFS_RT_MCP_ALERTS.
    #[arg(long, global = true)]
    alerts: Option<String>,
    /// Realtime TTL in seconds for the startup feed. Overrides GTFS_RT_MCP_RT_TTL.
    #[arg(long, global = true)]
    rt_ttl: Option<u64>,
    /// Static TTL in seconds for the startup feed. 0 disables the timer. Overrides GTFS_RT_MCP_STATIC_TTL.
    #[arg(long, global = true)]
    static_ttl: Option<u64>,
    /// JSON object of HTTP headers for the startup feed. Overrides GTFS_RT_MCP_HTTP_HEADERS. Values are never logged.
    #[arg(long, global = true)]
    http_headers: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Speak MCP over stdin and stdout. Logs go to stderr.
    Stdio,
    /// Speak streamable HTTP MCP.
    Http {
        /// Bind address. Default 127.0.0.1:8091.
        #[arg(long, env = "GTFS_RT_MCP_BIND", default_value = "127.0.0.1:8091")]
        bind: SocketAddr,
        /// Host header to allow, in addition to localhost, 127.0.0.1, and ::1.
        #[arg(long = "allowed-host", env = "GTFS_RT_MCP_ALLOWED_HOST")]
        allowed_hosts: Vec<String>,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    init_tracing(matches!(cli.command, Command::Http { .. }));
    let catalog = match Catalog::new() {
        Ok(catalog) => Arc::new(catalog),
        Err(error) => {
            eprintln!("gtfs-rt-mcp: {error}");
            std::process::exit(1);
        }
    };
    if let Err(error) = preload(&catalog, &cli).await {
        eprintln!("gtfs-rt-mcp: {}", error.message);
        std::process::exit(1);
    }
    let result = match cli.command {
        Command::Stdio => serve_stdio(GtfsServer::new(catalog)).await,
        Command::Http {
            bind,
            allowed_hosts,
        } => serve_http(catalog, bind, allowed_hosts).await,
    };
    if let Err(error) = result {
        eprintln!("gtfs-rt-mcp: {error}");
        std::process::exit(1);
    }
}

fn init_tracing(http: bool) {
    let fallback = if http { "info" } else { "warn" };
    let filter = std::env::var("GTFS_RT_MCP_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .unwrap_or_else(|_| fallback.to_string());
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

async fn preload(catalog: &Catalog, cli: &Cli) -> Result<(), gtfs_rt_mcp::ConfigError> {
    let request = startup_plan(
        std::env::vars(),
        &StartupOverrides {
            feed_id: cli.feed_id.clone(),
            static_gtfs: cli.r#static.clone(),
            trip_updates: cli.trip_updates.clone(),
            vehicle_positions: cli.vehicle_positions.clone(),
            alerts: cli.alerts.clone(),
            rt_ttl_secs: cli.rt_ttl,
            static_ttl_secs: cli.static_ttl,
            http_headers: cli.http_headers.clone(),
        },
    )?;
    let id = request
        .feed_id
        .clone()
        .unwrap_or_else(|| "default".to_string());
    let header_count = request.headers.len();
    tracing::info!(
        feed_id = %id,
        header_count,
        "loading feed from startup configuration"
    );
    match catalog.register(request.clone()).await {
        Ok(_) => {}
        Err(error) => {
            let redacted = redact(&error.error, &request.headers);
            eprintln!("gtfs-rt-mcp: feed {id}: {redacted}");
            let mut reported = gtfs_rt_mcp::AppError::new(redacted);
            if let Some(hint) = error.hint {
                reported = reported.hint(hint);
            }
            catalog.note_load_failure(request, &reported);
        }
    }
    Ok(())
}

fn redact(text: &str, headers: &BTreeMap<String, String>) -> String {
    let mut text = text.to_string();
    for value in headers.values() {
        if value.len() >= 4 {
            text = text.replace(value, "[redacted]");
        }
    }
    text
}
