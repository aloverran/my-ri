use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use clap::Parser;
use color_eyre::eyre::Result;
use tokio::sync::RwLock;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};

mod agent;
mod api;
mod meta_tools;
mod state;

use state::AppState;

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
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    // Load settings from ~/.config/agents/settings.json.
    let settings = ri_tools::resources::load_settings();

    let default_model = cli.model
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

    let sessions_dir = ri::SessionStore::default_dir()?;

    let app_state = Arc::new_cyclic(|weak| {
        let meta = meta_tools::create(weak.clone());
        let all_tools: Vec<Arc<dyn ri::Tool>> = base_tools.iter().cloned()
            .chain(meta)
            .collect();
        AppState {
            tools: all_tools,
            base_tools: base_tools.clone(),
            default_model: default_model.clone(),
            default_thinking,
            sessions_dir: sessions_dir.clone(),
            sessions: RwLock::new(std::collections::HashMap::new()),
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
        let serve = ServeDir::new(&frontend_dir)
            .fallback(ServeFile::new(&index));
        Router::new()
            .nest("/api", api_routes)
            .fallback_service(serve)
    };

    let addr: SocketAddr = format!("{}:{}", cli.host, cli.port).parse()?;
    tracing::info!("listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

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
