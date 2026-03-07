use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use clap::Parser;
use color_eyre::eyre::Result;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};
use tracing_subscriber::prelude::*;

mod agent;
mod api;
mod meta_tools;
mod state;
mod tracing_broadcast;

use state::AppState;
use tracing_broadcast::{BroadcastLayer, LogBuffer};

#[derive(Parser)]
#[command(name = "ri-web", about = "ri web interface")]
struct Cli {
    #[arg(long, default_value = "3001")]
    port: u16,

    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// Dev mode: skip static file serving, enable permissive CORS.
    #[arg(long)]
    dev: bool,

    #[arg(long)]
    model: Option<String>,

    #[arg(long)]
    thinking: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    // Broadcast channel for live tracing logs -> SSE. Ring buffer keeps
    // full history since boot (50k cap) so new SSE clients see everything.
    let (log_tx, _) = tokio::sync::broadcast::channel(1024);
    let log_buffer = Arc::new(LogBuffer::new(50_000));
    let (global_tx, _) = tokio::sync::broadcast::channel(64);

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_filter(tracing_subscriber::EnvFilter::from_default_env()),
        )
        .with(BroadcastLayer::new(log_tx.clone(), log_buffer.clone()))
        .init();

    let cli = Cli::parse();

    // Load settings from ~/.config/agents/settings.json.
    let settings = ri_tools::resources::load_settings();

    let default_model = cli
        .model
        .or_else(|| settings.default_model.clone())
        .unwrap_or_else(|| ri_ai::registry::default_model_id().to_string());

    let default_thinking = resolve_thinking(
        cli.thinking.as_deref(),
        settings.default_thinking.as_deref(),
    );

    let base_tools: Vec<Arc<dyn ri::Tool>> = ri_tools::all_tools()
        .into_iter()
        .map(|t| Arc::from(t))
        .collect();

    let sessions_dir = ri::Store::default_dir()?;

    let shutdown = CancellationToken::new();
    let tracker = TaskTracker::new();

    let app_state = Arc::new_cyclic(|weak| {
        let meta = meta_tools::create(weak.clone());
        let all_tools: Vec<Arc<dyn ri::Tool>> = base_tools.iter().cloned().chain(meta).collect();
        AppState {
            tools: all_tools,
            base_tools: base_tools.clone(),
            default_model: default_model.clone(),
            default_thinking,
            sessions_dir: sessions_dir.clone(),
            sessions: RwLock::new(std::collections::HashMap::new()),
            logins: RwLock::new(std::collections::HashMap::new()),
            log_tx: log_tx.clone(),
            log_buffer: log_buffer.clone(),
            global_tx: global_tx.clone(),
            shutdown: shutdown.clone(),
            tracker: tracker.clone(),
        }
    });

    // Build the API router.
    let api_routes = api::router(app_state.clone());

    let app = if cli.dev {
        tracing::info!("dev mode: CORS permissive, no static file serving");
        Router::new()
            .nest("/api", api_routes)
            .layer(CorsLayer::permissive())
    } else {
        // Serve built frontend from frontend/dist/, fallback to index.html for SPA routing.
        let frontend_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("frontend/dist");
        let index = frontend_dir.join("index.html");
        let serve = ServeDir::new(&frontend_dir).fallback(ServeFile::new(&index));
        Router::new()
            .nest("/api", api_routes)
            .fallback_service(serve)
    };

    let addr: SocketAddr = format!("{}:{}", cli.host, cli.port).parse()?;
    tracing::info!("listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;

    // Graceful shutdown orchestration.
    //
    // The server stays fully operational after the first ctrl-c: new
    // connections, new agent spawns, everything works. We wait for all
    // tracked agent tasks to drain naturally before stopping the server.
    // At ~5% utilization there are always gaps, so convergence is fast.
    // A second ctrl-c during the drain period hard-kills immediately.
    //
    // Sequence:
    //   1. ctrl-c        -> close tracker, begin waiting for drain
    //   2. tracker drains -> cancel shutdown token (closes SSE utility streams)
    //                     -> send server stop signal (axum begins its own drain)
    //   3. axum finishes  -> main returns
    //
    // The oneshot channel decouples the axum shutdown signal from ctrl-c
    // so the server keeps accepting connections throughout the drain.
    let (server_stop_tx, server_stop_rx) = tokio::sync::oneshot::channel::<()>();

    tokio::spawn({
        let shutdown = shutdown.clone();
        let tracker = tracker.clone();
        async move {
            // Wait for first ctrl-c.
            let _ = tokio::signal::ctrl_c().await;
            let active = tracker.len();
            if active > 0 {
                tracing::info!(
                    "received ctrl-c, waiting for {} running agents to finish \
                     (press ctrl-c again to force exit)",
                    active
                );
            } else {
                tracing::info!("received ctrl-c, shutting down");
            }

            // Allow wait() to resolve once the count reaches zero.
            // This does NOT prevent new tasks from being spawned and tracked.
            tracker.close();

            // Race: either all tasks finish, or a second ctrl-c forces exit.
            tokio::select! {
                _ = tracker.wait() => {
                    tracing::info!("all agents complete, stopping server");
                }
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("force exit");
                    std::process::exit(1);
                }
            }

            // All tracked work is done. Cancel the shutdown token so SSE
            // utility streams (log_events, global_events) close, then tell
            // axum to stop accepting connections and drain the rest.
            shutdown.cancel();
            let _ = server_stop_tx.send(());
        }
    });

    axum::serve(listener, app)
        .with_graceful_shutdown(async { let _ = server_stop_rx.await; })
        .await?;

    tracing::info!("server stopped");
    Ok(())
}

fn resolve_thinking(cli_flag: Option<&str>, settings: Option<&str>) -> ri::ThinkingLevel {
    let raw = cli_flag.or(settings).unwrap_or("medium");
    match raw {
        "off" => ri::ThinkingLevel::Off,
        "low" => ri::ThinkingLevel::Low,
        "medium" => ri::ThinkingLevel::Medium,
        "high" => ri::ThinkingLevel::High,
        "xhigh" => ri::ThinkingLevel::XHigh,
        other => {
            tracing::warn!("Unknown thinking level '{}', using medium", other);
            ri::ThinkingLevel::Medium
        }
    }
}
