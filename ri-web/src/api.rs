//! HTTP API routes: REST endpoints for session CRUD, SSE for streaming.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, Mutex};
use tokio_util::sync::CancellationToken;

use ri::{Message, SessionStore, SessionHeader};

use crate::agent::{self, AgentEvent};
use crate::state::{AppState, RunHandle, SessionState};

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/sessions", get(list_sessions).post(create_session))
        .route("/sessions/{id}", get(get_session).delete(delete_session))
        .route("/sessions/{id}/messages", post(send_message))
        .route("/sessions/{id}/events", get(session_events))
        .route("/sessions/{id}/cancel", post(cancel_session))
        .route("/settings", get(get_settings).put(update_settings))
        .with_state(state)
}

// -- Request / Response types --

#[derive(Deserialize)]
struct CreateSessionRequest {
    name: String,
    cwd: String,
}

#[derive(Serialize)]
struct SessionSummary {
    id: String,
    name: String,
    ts: String,
    cwd: String,
    message_count: usize,
}

#[derive(Serialize)]
struct SessionDetail {
    id: String,
    name: String,
    ts: String,
    cwd: String,
    status: String,
    messages: Vec<Message>,
}

#[derive(Deserialize)]
struct SendMessageRequest {
    text: String,
}

// -- Handlers --

/// List all sessions. Reads headers from JSONL files on disk.
async fn list_sessions(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<SessionSummary>>, AppError> {
    let mut summaries = Vec::new();

    if state.sessions_dir.exists() {
        let mut entries: Vec<_> = std::fs::read_dir(&state.sessions_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
            .collect();
        entries.sort_by(|a, b| b.file_name().cmp(&a.file_name()));

        for entry in entries {
            let path = entry.path();
            if let Ok(header) = read_session_header(&path) {
                let id = path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                // Count non-empty lines (messages) minus the header.
                // Uses BufRead to avoid loading entire file content into memory.
                let line_count = count_lines(&path).unwrap_or(0);
                summaries.push(SessionSummary {
                    id,
                    name: header.session,
                    ts: header.ts,
                    cwd: header.cwd.unwrap_or_default(),
                    message_count: line_count.saturating_sub(1),
                });
            }
        }
    }

    Ok(Json(summaries))
}

/// Create a new session.
async fn create_session(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateSessionRequest>,
) -> Result<(StatusCode, Json<SessionSummary>), AppError> {
    let cwd = std::path::PathBuf::from(&req.cwd);
    if !cwd.is_dir() {
        return Err(AppError::BadRequest(format!("'{}' is not a directory", req.cwd)));
    }

    let mut store = SessionStore::new(state.sessions_dir.clone());
    store.load_all()?;
    let session_path = store.new_session(&req.name, &req.cwd)?;

    let ts = chrono::Utc::now().to_rfc3339();
    let id = session_path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();

    // Write system prompt as first message.
    let system_prompt = crate::agent::build_system_prompt(&cwd);
    let sys_id = store.next_id();
    let sys_msg = Message::new(sys_id.clone(), ri::Role::System, vec![ri::ContentBlock::text(&system_prompt)]);
    store.write_message(sys_msg)?;

    let (events_tx, _) = broadcast::channel(256);

    let session_state = SessionState {
        store,
        message_ids: vec![sys_id],
        cwd,
        name: req.name.clone(),
        ts: ts.clone(),
        events_tx,
        current_run: None,
    };

    state.sessions.write().await
        .insert(id.clone(), Arc::new(Mutex::new(session_state)));

    Ok((StatusCode::CREATED, Json(SessionSummary {
        id,
        name: req.name,
        ts,
        cwd: req.cwd,
        message_count: 1,
    })))
}

/// Get session detail with all messages.
async fn get_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<SessionDetail>, AppError> {
    let session = get_or_load_session(&state, &id).await?;
    let lock = session.lock().await;

    let messages: Vec<Message> = lock.store.pool.resolve_existing(&lock.message_ids)
        .into_iter()
        .cloned()
        .collect();

    Ok(Json(SessionDetail {
        id,
        name: lock.name.clone(),
        ts: lock.ts.clone(),
        cwd: lock.cwd.to_string_lossy().to_string(),
        status: lock.status().to_string(),
        messages,
    }))
}

/// Delete session from memory (stop agent loop if running). Does not delete the file.
async fn delete_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let removed = state.sessions.write().await.remove(&id);
    if let Some(session) = removed {
        let lock = session.lock().await;
        if let Some(ref run) = lock.current_run {
            run.cancel.cancel();
        }
    }
    Ok(StatusCode::OK)
}

/// Send a user message and start the agent loop.
async fn send_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<SendMessageRequest>,
) -> Result<StatusCode, AppError> {
    let session = get_or_load_session(&state, &id).await?;

    let mut lock = session.lock().await;
    if lock.is_running() {
        return Err(AppError::Conflict("Agent loop is already running".into()));
    }

    let cancel = CancellationToken::new();
    let thinking = *state.thinking.read().await;
    let task = agent::spawn_agent_loop(
        session.clone(),
        req.text,
        state.provider.clone(),
        state.model.clone(),
        state.tools.clone(),
        thinking,
        cancel.clone(),
    );

    lock.current_run = Some(RunHandle { cancel, task });

    Ok(StatusCode::ACCEPTED)
}

/// SSE endpoint: subscribe to the session's agent event broadcast.
async fn session_events(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, AppError> {
    let session = get_or_load_session(&state, &id).await?;
    let rx = session.lock().await.events_tx.subscribe();

    let stream = event_stream(rx);
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// Cancel the active agent loop.
async fn cancel_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let session = get_or_load_session(&state, &id).await?;
    let lock = session.lock().await;
    if let Some(ref run) = lock.current_run {
        run.cancel.cancel();
    }
    Ok(StatusCode::OK)
}

// -- Settings --

#[derive(Serialize)]
struct SettingsResponse {
    thinking: String,
}

#[derive(Deserialize)]
struct UpdateSettingsRequest {
    thinking: Option<String>,
}

async fn get_settings(
    State(state): State<Arc<AppState>>,
) -> Json<SettingsResponse> {
    let thinking = *state.thinking.read().await;
    Json(SettingsResponse {
        thinking: thinking_to_str(thinking).to_string(),
    })
}

async fn update_settings(
    State(state): State<Arc<AppState>>,
    Json(req): Json<UpdateSettingsRequest>,
) -> Result<Json<SettingsResponse>, AppError> {
    if let Some(ref raw) = req.thinking {
        let level = parse_thinking(raw)
            .ok_or_else(|| AppError::BadRequest(format!("Invalid thinking level: '{}'", raw)))?;
        *state.thinking.write().await = level;
    }

    // Persist to settings.json. Read existing, merge, write back.
    if let Some(config_dir) = ri_tools::resources::config_dir() {
        let path = config_dir.join("settings.json");
        let mut obj: serde_json::Map<String, serde_json::Value> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|c| serde_json::from_str(&c).ok())
            .unwrap_or_default();

        if let Some(ref raw) = req.thinking {
            obj.insert("defaultThinking".to_string(), serde_json::json!(raw));
        }

        if let Err(e) = std::fs::write(&path, serde_json::to_string_pretty(&obj).unwrap_or_default()) {
            tracing::warn!("Failed to write settings.json: {}", e);
        }
    }

    let thinking = *state.thinking.read().await;
    Ok(Json(SettingsResponse {
        thinking: thinking_to_str(thinking).to_string(),
    }))
}

fn thinking_to_str(level: ri::ThinkingLevel) -> &'static str {
    match level {
        ri::ThinkingLevel::Off => "off",
        ri::ThinkingLevel::Low => "low",
        ri::ThinkingLevel::Medium => "medium",
        ri::ThinkingLevel::High => "high",
        ri::ThinkingLevel::XHigh => "xhigh",
    }
}

fn parse_thinking(raw: &str) -> Option<ri::ThinkingLevel> {
    match raw {
        "off" => Some(ri::ThinkingLevel::Off),
        "low" => Some(ri::ThinkingLevel::Low),
        "medium" => Some(ri::ThinkingLevel::Medium),
        "high" => Some(ri::ThinkingLevel::High),
        "xhigh" => Some(ri::ThinkingLevel::XHigh),
        _ => None,
    }
}

// -- SSE stream conversion --

fn event_stream(
    mut rx: broadcast::Receiver<AgentEvent>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if let Some(sse_event) = agent_event_to_sse(&event) {
                        yield Ok(sse_event);
                    }
                    if matches!(event, AgentEvent::Done) {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("SSE client lagged, missed {} events", n);
                    let event = Event::default()
                        .event("resync")
                        .data("{}");
                    yield Ok(event);
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }
}

fn agent_event_to_sse(event: &AgentEvent) -> Option<Event> {
    match event {
        AgentEvent::Stream(se) => stream_event_to_sse(se),
        AgentEvent::ToolStart { id, name } => {
            let data = serde_json::json!({ "id": id, "name": name });
            Some(Event::default().event("tool_start").data(data.to_string()))
        }
        AgentEvent::ToolEnd { id, output, is_error } => {
            let data = serde_json::json!({ "id": id, "output": output, "is_error": is_error });
            Some(Event::default().event("tool_end").data(data.to_string()))
        }
        AgentEvent::MessageComplete(msg) => {
            let data = serde_json::to_string(msg).unwrap_or_default();
            Some(Event::default().event("message_complete").data(data))
        }
        AgentEvent::Error(msg) => {
            let data = serde_json::json!({ "message": msg });
            Some(Event::default().event("error").data(data.to_string()))
        }
        AgentEvent::Done => {
            Some(Event::default().event("done").data("{}"))
        }
    }
}

fn stream_event_to_sse(event: &ri::StreamEvent) -> Option<Event> {
    match event {
        ri::StreamEvent::TextStart => {
            Some(Event::default().event("text_start").data("{}"))
        }
        ri::StreamEvent::TextDelta(delta) => {
            let data = serde_json::json!({ "delta": delta });
            Some(Event::default().event("text_delta").data(data.to_string()))
        }
        ri::StreamEvent::TextEnd { .. } => {
            Some(Event::default().event("text_end").data("{}"))
        }
        ri::StreamEvent::ThinkingStart => {
            Some(Event::default().event("thinking_start").data("{}"))
        }
        ri::StreamEvent::ThinkingDelta(delta) => {
            let data = serde_json::json!({ "delta": delta });
            Some(Event::default().event("thinking_delta").data(data.to_string()))
        }
        ri::StreamEvent::ThinkingEnd { .. } => {
            Some(Event::default().event("thinking_end").data("{}"))
        }
        ri::StreamEvent::ToolCallStart { id, name } => {
            let data = serde_json::json!({ "id": id, "name": name });
            Some(Event::default().event("tool_call_start").data(data.to_string()))
        }
        ri::StreamEvent::ToolCallDelta { id, json_fragment } => {
            let data = serde_json::json!({ "id": id, "delta": json_fragment });
            Some(Event::default().event("tool_call_delta").data(data.to_string()))
        }
        ri::StreamEvent::ToolCallEnd { .. } => {
            Some(Event::default().event("tool_call_end").data("{}"))
        }
        ri::StreamEvent::Usage(usage) => {
            let data = serde_json::to_string(usage).unwrap_or_default();
            Some(Event::default().event("usage").data(data))
        }
        ri::StreamEvent::Done => None, // Handled at AgentEvent level
        ri::StreamEvent::Error(msg) => {
            let data = serde_json::json!({ "message": msg });
            Some(Event::default().event("error").data(data.to_string()))
        }
    }
}

// -- Session loading --

/// Get a session from the in-memory map, or load it from disk.
/// Holds the write lock during load to prevent duplicate loading.
async fn get_or_load_session(
    state: &AppState,
    id: &str,
) -> Result<Arc<Mutex<SessionState>>, AppError> {
    let mut sessions = state.sessions.write().await;

    if let Some(session) = sessions.get(id) {
        return Ok(session.clone());
    }

    // Load from disk while holding the write lock.
    let filename = format!("{}.jsonl", id);
    let path = state.sessions_dir.join(&filename);
    if !path.exists() {
        return Err(AppError::NotFound(format!("Session '{}' not found", id)));
    }

    let header = read_session_header(&path)?;
    let cwd = std::path::PathBuf::from(header.cwd.as_deref().unwrap_or("."));

    let mut store = SessionStore::new(state.sessions_dir.clone());
    store.load_all()?;

    let message_ids = read_session_message_ids(&path)?;
    let (events_tx, _) = broadcast::channel(256);

    let session_state = SessionState {
        store,
        message_ids,
        cwd,
        name: header.session,
        ts: header.ts,
        events_tx,
        current_run: None,
    };

    let session = Arc::new(Mutex::new(session_state));
    sessions.insert(id.to_string(), session.clone());

    Ok(session)
}

fn read_session_header(path: &std::path::Path) -> Result<SessionHeader, AppError> {
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let first_line = std::io::BufRead::lines(reader)
        .next()
        .ok_or_else(|| AppError::NotFound("Empty session file".into()))??;
    let header: SessionHeader = serde_json::from_str(&first_line)?;
    Ok(header)
}

fn count_lines(path: &std::path::Path) -> Result<usize, AppError> {
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let count = std::io::BufRead::lines(reader)
        .filter(|l| l.as_ref().is_ok_and(|s| !s.trim().is_empty()))
        .count();
    Ok(count)
}

fn read_session_message_ids(path: &std::path::Path) -> Result<Vec<String>, AppError> {
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let mut ids = Vec::new();
    let mut first = true;

    for line in std::io::BufRead::lines(reader) {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() { continue; }

        if first {
            first = false;
            // Skip header line (has "session" key).
            if let Ok(obj) = serde_json::from_str::<serde_json::Value>(trimmed) {
                if obj.get("session").is_some() && obj.get("role").is_none() {
                    continue;
                }
            }
        }

        if let Ok(msg) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if let Some(id) = msg.get("id").and_then(|v| v.as_str()) {
                ids.push(id.to_string());
            }
        }
    }

    Ok(ids)
}

// -- Error type --

enum AppError {
    NotFound(String),
    BadRequest(String),
    Conflict(String),
    Internal(String),
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self { AppError::Internal(e.to_string()) }
}

impl From<eyre::Report> for AppError {
    fn from(e: eyre::Report) -> Self { AppError::Internal(e.to_string()) }
}

impl From<serde_json::Error> for AppError {
    fn from(e: serde_json::Error) -> Self { AppError::Internal(e.to_string()) }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            AppError::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            AppError::Conflict(msg) => (StatusCode::CONFLICT, msg),
            AppError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
        };
        let body = serde_json::json!({ "error": message });
        (status, Json(body)).into_response()
    }
}
