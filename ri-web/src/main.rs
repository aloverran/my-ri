use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

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
mod watch;

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

    /// Watch mode: become a supervisor that watches source files,
    /// rebuilds on change, and restarts the server gracefully.
    #[arg(long)]
    watch: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    let cli = Cli::parse();

    // Supervisor mode: minimal tracing, no broadcast layer needed.
    // The supervisor only watches files, builds, and manages the child.
    if cli.watch {
        tracing_subscriber::fmt()
            .with_env_filter(default_env_filter())
            .init();
        watch::run_supervisor().await;
    }

    // -- Server mode (normal or supervised child) --

    // Broadcast channel for live tracing logs -> SSE. Ring buffer keeps
    // full history since boot (50k cap) so new SSE clients see everything.
    let (log_tx, _) = tokio::sync::broadcast::channel(1024);
    let log_buffer = Arc::new(LogBuffer::new(50_000));
    let (global_tx, _) = tokio::sync::broadcast::channel(64);

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_filter(default_env_filter()),
        )
        .with(BroadcastLayer::new(log_tx.clone(), log_buffer.clone()))
        .init();

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
            update_available: AtomicBool::new(false),
            update_trigger: Arc::new(tokio::sync::Notify::new()),
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
    // Two triggers can initiate shutdown:
    //   1. ctrl-c  -> normal exit (code 0)
    //   2. update  -> restart exit (code 42, supervised mode only)
    //
    // In both cases, the server stays fully operational while tracked
    // agent tasks drain. New connections and new agent spawns continue
    // to work. A second ctrl-c during drain hard-kills immediately.
    //
    // Sequence:
    //   1. trigger fires  -> close tracker, begin waiting for drain
    //   2. tracker drains -> cancel shutdown token (closes SSE streams)
    //                     -> send server stop signal (axum drains HTTP)
    //   3. axum finishes  -> main returns (or exits 42 for update)
    let (server_stop_tx, server_stop_rx) = tokio::sync::oneshot::channel::<()>();

    let supervised = std::env::var("RI_WEB_SUPERVISED").is_ok();

    // Stdin monitor: when running as a supervised child, the supervisor
    // communicates via stdin. "update\n" means a new binary is ready.
    // EOF means the supervisor died; trigger a graceful shutdown.
    let stdin_eof = CancellationToken::new();
    if supervised {
        let global_tx = global_tx.clone();
        let app = app_state.clone();
        let eof_token = stdin_eof.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let stdin = tokio::io::stdin();
            let mut lines = BufReader::new(stdin).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim() == "update" {
                    app.update_available
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    let _ = global_tx.send(state::GlobalEvent::UpdateAvailable);
                    tracing::info!("supervisor signaled: update available");
                }
            }
            // EOF: supervisor died or was killed.
            tracing::info!("supervisor stdin closed, shutting down");
            eof_token.cancel();
        });
    }

    tokio::spawn({
        let shutdown = shutdown.clone();
        let tracker = tracker.clone();
        let update_trigger = app_state.update_trigger.clone();
        async move {
            // Wait for any shutdown trigger.
            let exit_code: i32;
            tokio::select! {
                _ = tokio::signal::ctrl_c() => { exit_code = 0; }
                _ = update_trigger.notified() => { exit_code = 42; }
                _ = stdin_eof.cancelled() => { exit_code = 0; }
            }

            let active = tracker.len();
            if active > 0 {
                tracing::info!(
                    "shutting down, waiting for {} running agents to finish \
                     (press ctrl-c again to force exit)",
                    active
                );
            } else {
                tracing::info!("shutting down");
            }

            tracker.close();

            tokio::select! {
                _ = tracker.wait() => {
                    tracing::info!("all agents complete, stopping server");
                }
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("force exit");
                    std::process::exit(1);
                }
            }

            shutdown.cancel();

            // Update restart: all agent work is persisted. Exit
            // immediately so the supervisor can spawn the new binary.
            // No need for a graceful HTTP drain -- clients will
            // reconnect to the new server.
            if exit_code == 42 {
                std::process::exit(42);
            }

            // Normal shutdown: drain HTTP connections gracefully.
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

/// RUST_LOG if set, otherwise `info` -- a sensible default for a dev tool.
fn default_env_filter() -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
}
