//! HTTP API routes: REST endpoints for session CRUD, SSE for streaming.

use std::convert::Infallible;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, broadcast};
use tokio_util::sync::CancellationToken;

use ri::{Message, MessageId, SessionHeader, SessionId, Store};

use crate::agent::{self, AgentEvent};
use crate::state::{AppState, LoginInProgress, LoginStatus, RunHandle, SessionState};

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/sessions", get(list_sessions).post(create_session))
        .route("/sessions/{id}", get(get_session).delete(delete_session))
        .route("/sessions/{id}/messages", post(send_message))
        .route("/sessions/{id}/events", get(session_events))
        .route("/sessions/{id}/cancel", post(cancel_session))
        .route("/models", get(list_models))
        .route("/settings", get(get_settings))
        .route("/logs", get(log_events))
        .route("/auth/status", get(auth_status))
        .route("/auth/login", post(auth_login))
        .route("/auth/complete", post(auth_complete))
        .route("/auth/logout", post(auth_logout))
        .route("/auth/login-status/{provider_id}", get(auth_login_status))
        .with_state(state)
}

// -- Request / Response types --

#[derive(Deserialize)]
struct CreateSessionRequest {
    cwd: String,
}

#[derive(Serialize)]
struct SessionSummary {
    id: String,
    name: String,
    ts: String,
    cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent: Option<String>,
    message_count: usize,
}

#[derive(Serialize)]
struct SessionDetail {
    id: String,
    name: String,
    ts: String,
    cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent: Option<String>,
    status: String,
    messages: Vec<Message>,
}

#[derive(Deserialize)]
struct SendMessageRequest {
    text: String,
    /// Model ID for this request. Resolved against the registry.
    #[serde(default)]
    model: Option<String>,
    /// Thinking level for this request.
    #[serde(default)]
    thinking: Option<String>,
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
                let id = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                // Count non-empty lines (messages) minus the header.
                let line_count = count_lines(&path).unwrap_or(0);
                summaries.push(SessionSummary {
                    id,
                    name: header.session,
                    ts: header.ts,
                    cwd: header.cwd.unwrap_or_default(),
                    parent: header.parent,
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
        return Err(AppError::BadRequest(format!(
            "'{}' is not a directory",
            req.cwd
        )));
    }

    let name = "New session";
    let mut store = Store::new(state.sessions_dir.clone());
    store.load_all()?;
    let id = store.create_session(name, &req.cwd, None)?;

    let ts = chrono::Utc::now().to_rfc3339();

    // Write system prompt as first message, tagged with discovered context file
    // paths so they participate in the agents_context seen system.
    let context_files = ri_tools::resources::discover_context_files(&cwd);
    let system_prompt = {
        let mut parts = vec![
            ri_tools::resources::BASE_SYSTEM_PROMPT.to_string(),
            ri_tools::resources::get_environment_system_prompt(Some(vec![
                format!("Session id for this Ri session: {id}"),
                format!("Working directory: {}", cwd.display()),
            ])),
            ri_tools::resources::format_context_files(&context_files),
        ];
        parts.retain(|p| !p.is_empty());
        parts.join("\n\n")
    };

    let context_paths: Vec<String> = context_files
        .iter()
        .filter_map(|cf| cf.path.canonicalize().ok()?.to_str().map(str::to_string))
        .collect();
    let sys_msg = store.write_message(
        &id,
        ri::Role::System,
        vec![ri::ContentBlock::text(&system_prompt)],
        if context_paths.is_empty() {
            None
        } else {
            Some(serde_json::json!({ "agents_context": context_paths }))
        },
    )?;

    let message_ids = vec![sys_msg.id];
    store.checkpoint(&id, &message_ids, None)?;

    let (events_tx, _) = broadcast::channel(256);

    let session_state = SessionState {
        store,
        message_ids,
        cwd,
        name: name.to_string(),
        ts: ts.clone(),
        file_id: id.clone(),
        parent: None,
        events_tx,
        current_run: None,
        title_gen_seq: 0,
    };

    state
        .sessions
        .write()
        .await
        .insert(id.to_string(), Arc::new(Mutex::new(session_state)));

    Ok((
        StatusCode::CREATED,
        Json(SessionSummary {
            id: id.to_string(),
            name: name.to_string(),
            ts,
            cwd: req.cwd,
            parent: None,
            message_count: 1,
        }),
    ))
}

/// Get session detail with all messages.
async fn get_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<SessionDetail>, AppError> {
    let session = get_or_load_session(&state, &id).await?;
    let lock = session.lock().await;

    let messages: Vec<Message> = lock
        .store
        .pool
        .resolve(&lock.message_ids)
        .into_iter()
        .cloned()
        .collect();

    Ok(Json(SessionDetail {
        id,
        name: lock.name.clone(),
        ts: lock.ts.clone(),
        cwd: lock.cwd.to_string_lossy().to_string(),
        parent: lock.parent.as_ref().map(|p| p.to_string()),
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

    // Expand prompt templates for the session's working directory.
    let text = {
        let mut templates = Vec::new();
        if let Some(global) = ri_tools::resources::config_dir() {
            templates.extend(ri_tools::prompts::load_templates(&global.join("prompts")));
        }
        let mut dir = lock
            .cwd
            .canonicalize()
            .ok()
            .or_else(|| Some(lock.cwd.clone()));
        while let Some(d) = dir {
            templates.extend(ri_tools::prompts::load_templates(
                &d.join(".agents").join("prompts"),
            ));
            if d.join(".git").exists() {
                break;
            }
            dir = d.parent().map(std::path::Path::to_path_buf);
        }
        ri_tools::prompts::expand_prompt(&req.text, &templates)
    };

    // Resolve model: request > server default.
    let model_id = req.model.unwrap_or_else(|| state.default_model.clone());
    let (provider, model) = ri_ai::registry::resolve(&model_id)
        .await
        .map_err(|e| AppError::BadRequest(e.to_string()))?;

    // Resolve thinking: request > server default.
    let thinking = req
        .thinking
        .as_deref()
        .and_then(parse_thinking)
        .unwrap_or(state.default_thinking);

    let cancel = CancellationToken::new();
    let task = agent::spawn_agent_loop(
        session.clone(),
        text,
        Arc::from(provider),
        model,
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

/// SSE endpoint: stream tracing log entries from the server.
/// Global (not per-session) -- streams all tracing output.
///
/// On connect: subscribes to broadcast first, then snapshots the ring
/// buffer. This guarantees no events are missed (subscribe catches
/// anything after this point; snapshot catches everything before).
/// A few entries at the boundary may appear twice -- harmless for a
/// debug panel.
async fn log_events(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // Subscribe before snapshot so we don't miss events in the gap.
    let rx = state.log_tx.subscribe();
    let history = state.log_buffer.snapshot();
    tracing::info!("log panel connected, replaying {} entries", history.len());

    let stream = async_stream::stream! {
        // Replay buffered history first.
        for entry in history {
            if let Ok(data) = serde_json::to_string(&entry) {
                yield Ok(Event::default().event("log").data(data));
            }
        }

        // Then stream live events from broadcast.
        let mut rx = rx;
        loop {
            match rx.recv().await {
                Ok(entry) => {
                    if let Ok(data) = serde_json::to_string(&entry) {
                        yield Ok(Event::default().event("log").data(data));
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::debug!("log SSE client lagged, missed {} entries", n);
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}

// -- Models --

#[derive(Serialize)]
struct ModelInfo {
    id: String,
    name: String,
    provider: String,
    context_window: usize,
}

/// List all available models across providers.
async fn list_models() -> Json<Vec<ModelInfo>> {
    let mut models = Vec::new();
    for provider in ri_ai::registry::all_providers() {
        if !provider.is_authenticated() { continue; }
        let provider_id = provider.id().to_string();
        for m in provider.models() {
            models.push(ModelInfo {
                id: m.id,
                name: m.name,
                provider: provider_id.clone(),
                context_window: m.context_window,
            });
        }
    }
    Json(models)
}

// -- Settings --

#[derive(Serialize)]
struct SettingsResponse {
    default_model: String,
    default_thinking: String,
}

async fn get_settings(State(state): State<Arc<AppState>>) -> Json<SettingsResponse> {
    Json(SettingsResponse {
        default_model: state.default_model.clone(),
        default_thinking: thinking_to_str(state.default_thinking).to_string(),
    })
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

// -- Auth --
//
// OAuth login flow for LLM providers. Two patterns:
//   PasteCode (Anthropic): user visits URL, copies code, POSTs it to /auth/complete.
//   LocalCallback (Gemini): backend starts temp server on callback port, browser
//     redirects there automatically. Frontend polls /auth/login-status until done.

#[derive(Serialize)]
struct ProviderAuthInfo {
    id: String,
    name: String,
    authenticated: bool,
    can_logout: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    account: Option<String>,
}

/// Which providers exist and whether they have stored credentials.
async fn auth_status() -> Json<Vec<ProviderAuthInfo>> {
    let providers = ri_ai::registry::all_providers();
    let info: Vec<ProviderAuthInfo> = providers
        .into_iter()
        .map(|p| ProviderAuthInfo {
            id: p.id().to_string(),
            name: p.name().to_string(),
            authenticated: p.is_authenticated(),
            can_logout: p.can_logout(),
            account: p.account_label(),
        })
        .collect();
    Json(info)
}

#[derive(Deserialize)]
struct AuthLoginRequest {
    provider_id: String,
}

#[derive(Serialize)]
struct AuthLoginResponse {
    /// "paste_code", "local_callback", or "text_input"
    method: String,
    /// URL for paste_code/local_callback, or prompt text for text_input.
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    placeholder: Option<String>,
}

/// Begin an OAuth login flow for a provider. Creates the provider instance,
/// calls begin_login, and stores the instance for the complete step.
/// For LocalCallback, also spawns a background task that starts the callback
/// server and auto-completes when the browser redirects.
async fn auth_login(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AuthLoginRequest>,
) -> Result<Json<AuthLoginResponse>, AppError> {
    // Clean up any previous login for this provider.
    state.logins.write().await.remove(&req.provider_id);

    // Find the matching provider factory.
    let provider = ri_ai::registry::all_providers()
        .into_iter()
        .find(|p| p.id() == req.provider_id)
        .ok_or_else(|| AppError::BadRequest(format!("Unknown provider: {}", req.provider_id)))?;

    let auth_method = provider
        .begin_login()
        .await
        .map_err(|e| AppError::Internal(format!("begin_login failed: {}", e)))?
        .ok_or_else(|| AppError::BadRequest("Provider does not support login".into()))?;

    let status = std::sync::Arc::new(Mutex::new(LoginStatus::AwaitingCode));

    let (method, url, placeholder) = match &auth_method {
        ri::AuthMethod::PasteCode { url } => ("paste_code".to_string(), url.clone(), None),
        ri::AuthMethod::TextInput { prompt, placeholder } => ("text_input".to_string(), prompt.clone(), Some(placeholder.clone())),
        ri::AuthMethod::LocalCallback { url, port, path } => {
            *status.lock().await = LoginStatus::AwaitingCallback;
            let callback_url = url.clone();
            let port = *port;
            let path = path.clone();

            // Spawn background task: start callback server, wait for code, complete login.
            let bg_status = status.clone();
            let provider_id = req.provider_id.clone();
            let bg_state = state.clone();
            tokio::spawn(async move {
                let result = run_local_callback(&bg_state, &provider_id, port, &path).await;
                match result {
                    Ok(()) => {
                        *bg_status.lock().await = LoginStatus::Complete;
                    }
                    Err(e) => {
                        *bg_status.lock().await = LoginStatus::Failed(e.to_string());
                    }
                }
            });

            ("local_callback".to_string(), callback_url, None)
        }
    };

    let login = LoginInProgress {
        provider: Mutex::new(provider),
        status,
    };
    state.logins.write().await.insert(req.provider_id, login);

    Ok(Json(AuthLoginResponse { method, url, placeholder }))
}

/// Start a temporary HTTP server on the callback port, wait for the OAuth
/// redirect, then call complete_login on the stored provider instance.
async fn run_local_callback(
    state: &Arc<AppState>,
    provider_id: &str,
    port: u16,
    expected_path: &str,
) -> eyre::Result<()> {
    use axum::{
        Router as CallbackRouter, extract::Query, response::Html, routing::get as get_route,
    };
    use std::collections::HashMap as StdHashMap;

    let (tx, rx) = tokio::sync::oneshot::channel::<Result<String, String>>();
    let tx = std::sync::Arc::new(Mutex::new(Some(tx)));

    let handler = {
        let tx = tx.clone();
        move |Query(params): Query<StdHashMap<String, String>>| {
            let tx = tx.clone();
            async move {
                let mut guard = tx.lock().await;
                if let Some(tx) = guard.take() {
                    if let Some(error) = params.get("error") {
                        let _ = tx.send(Err(error.clone()));
                        return Html(
                            "<h1>Authorization failed</h1><p>You can close this window.</p>"
                                .to_string(),
                        );
                    }
                    if let Some(code) = params.get("code") {
                        let _ = tx.send(Ok(code.clone()));
                        return Html("<h1>Login successful</h1><p>You can close this window and return to ri.</p>".to_string());
                    }
                    let _ = tx.send(Err("No authorization code in callback".into()));
                }
                Html("<h1>Unexpected request</h1>".to_string())
            }
        }
    };

    let app = CallbackRouter::new().route(expected_path, get_route(handler));
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{}", port))
        .await
        .map_err(|e| eyre::eyre!("Failed to bind callback on port {}: {}", port, e))?;

    let code = tokio::select! {
        result = axum::serve(listener, app) => {
            result.map_err(|e| eyre::eyre!("Callback server error: {}", e))?;
            return Err(eyre::eyre!("Callback server stopped unexpectedly"));
        }
        result = rx => {
            result
                .map_err(|_| eyre::eyre!("Callback channel closed"))?
                .map_err(|e| eyre::eyre!("OAuth error: {}", e))?
        }
        _ = tokio::time::sleep(std::time::Duration::from_secs(300)) => {
            return Err(eyre::eyre!("OAuth callback timed out after 5 minutes"));
        }
    };

    // Complete login on the stored provider instance.
    let logins = state.logins.read().await;
    let login = logins
        .get(provider_id)
        .ok_or_else(|| eyre::eyre!("Login state disappeared"))?;
    let provider = login.provider.lock().await;
    provider.complete_login(&code).await?;

    Ok(())
}

#[derive(Deserialize)]
struct AuthCompleteRequest {
    provider_id: String,
    code: String,
}

/// Complete a PasteCode login flow with the code the user copied from the
/// provider's callback page.
async fn auth_complete(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AuthCompleteRequest>,
) -> Result<StatusCode, AppError> {
    let logins = state.logins.read().await;
    let login = logins
        .get(&req.provider_id)
        .ok_or_else(|| AppError::BadRequest("No login in progress for this provider".into()))?;

    let provider = login.provider.lock().await;
    provider
        .complete_login(&req.code)
        .await
        .map_err(|e| AppError::Internal(format!("complete_login failed: {}", e)))?;

    *login.status.lock().await = LoginStatus::Complete;
    drop(provider);
    drop(logins);

    // Clean up -- login is done.
    state.logins.write().await.remove(&req.provider_id);

    Ok(StatusCode::OK)
}

#[derive(Deserialize)]
struct AuthLogoutRequest {
    provider_id: String,
}

/// Logout from a provider: delete stored credentials and clear in-memory auth state.
async fn auth_logout(
    Json(req): Json<AuthLogoutRequest>,
) -> Result<StatusCode, AppError> {
    let provider = ri_ai::registry::all_providers()
        .into_iter()
        .find(|p| p.id() == req.provider_id)
        .ok_or_else(|| AppError::BadRequest(format!("Unknown provider: {}", req.provider_id)))?;

    provider
        .logout()
        .await
        .map_err(|e| AppError::BadRequest(e.to_string()))?;

    tracing::info!("Logged out from provider [{}]", req.provider_id);
    Ok(StatusCode::OK)
}

#[derive(Serialize)]
struct AuthLoginStatusResponse {
    status: String,
    error: Option<String>,
}

/// Poll the status of a LocalCallback login flow.
async fn auth_login_status(
    State(state): State<Arc<AppState>>,
    Path(provider_id): Path<String>,
) -> Result<Json<AuthLoginStatusResponse>, AppError> {
    let logins = state.logins.read().await;
    let login = logins
        .get(&provider_id)
        .ok_or_else(|| AppError::NotFound("No login in progress".into()))?;

    let status = login.status.lock().await.clone();
    let (status_str, error) = match &status {
        LoginStatus::AwaitingCode => ("awaiting_code", None),
        LoginStatus::AwaitingCallback => ("awaiting_callback", None),
        LoginStatus::Complete => ("complete", None),
        LoginStatus::Failed(e) => ("failed", Some(e.clone())),
    };

    // Clean up completed/failed flows.
    drop(logins);
    if matches!(status, LoginStatus::Complete | LoginStatus::Failed(_)) {
        state.logins.write().await.remove(&provider_id);
    }

    Ok(Json(AuthLoginStatusResponse {
        status: status_str.to_string(),
        error,
    }))
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
        AgentEvent::ToolEnd {
            id,
            output,
            is_error,
            details,
        } => {
            let data = serde_json::json!({ "id": id, "output": output, "is_error": is_error, "details": details });
            Some(Event::default().event("tool_end").data(data.to_string()))
        }
        AgentEvent::MessageComplete(msg) => {
            let data = serde_json::to_string(msg).unwrap_or_default();
            Some(Event::default().event("message_complete").data(data))
        }
        AgentEvent::TitleUpdate(title) => {
            let data = serde_json::json!({ "title": title });
            Some(Event::default().event("title_update").data(data.to_string()))
        }
        AgentEvent::Error(msg) => {
            let data = serde_json::json!({ "message": msg });
            Some(Event::default().event("agent_error").data(data.to_string()))
        }
        AgentEvent::Done => Some(Event::default().event("done").data("{}")),
    }
}

fn stream_event_to_sse(event: &ri::StreamEvent) -> Option<Event> {
    match event {
        ri::StreamEvent::TextStart => Some(Event::default().event("text_start").data("{}")),
        ri::StreamEvent::TextDelta(delta) => {
            let data = serde_json::json!({ "delta": delta });
            Some(Event::default().event("text_delta").data(data.to_string()))
        }
        ri::StreamEvent::TextEnd { .. } => Some(Event::default().event("text_end").data("{}")),
        ri::StreamEvent::ThinkingStart => Some(Event::default().event("thinking_start").data("{}")),
        ri::StreamEvent::ThinkingDelta(delta) => {
            let data = serde_json::json!({ "delta": delta });
            Some(
                Event::default()
                    .event("thinking_delta")
                    .data(data.to_string()),
            )
        }
        ri::StreamEvent::ThinkingEnd { .. } => {
            Some(Event::default().event("thinking_end").data("{}"))
        }
        ri::StreamEvent::ToolCallStart { id, name } => {
            let data = serde_json::json!({ "id": id, "name": name });
            Some(
                Event::default()
                    .event("tool_call_start")
                    .data(data.to_string()),
            )
        }
        ri::StreamEvent::ToolCallDelta { id, json_fragment } => {
            let data = serde_json::json!({ "id": id, "delta": json_fragment });
            Some(
                Event::default()
                    .event("tool_call_delta")
                    .data(data.to_string()),
            )
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
            Some(Event::default().event("agent_error").data(data.to_string()))
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

    let mut store = Store::new(state.sessions_dir.clone());
    store.load_all()?;

    // Prefer the head step's context (written by checkpoint), fall back to
    // scanning msg lines for sessions that predate step/head tracking.
    let message_ids: Vec<MessageId> = match store.head_context(id) {
        Some(ctx) => ctx.messages.clone(),
        None => read_session_message_ids(&path)?.into_iter().map(MessageId::from).collect(),
    };
    let (events_tx, _) = broadcast::channel(256);

    let session_state = SessionState {
        store,
        message_ids,
        cwd,
        name: header.session,
        ts: header.ts,
        file_id: SessionId::from(id),
        parent: header.parent.map(SessionId::from),
        events_tx,
        current_run: None,
        title_gen_seq: 0,
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

    for line in std::io::BufRead::lines(reader) {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Ok(obj) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if let Some(id) = obj.get("msg").and_then(|v| v.as_str()) {
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
    fn from(e: std::io::Error) -> Self {
        AppError::Internal(e.to_string())
    }
}

impl From<eyre::Report> for AppError {
    fn from(e: eyre::Report) -> Self {
        AppError::Internal(e.to_string())
    }
}

impl From<serde_json::Error> for AppError {
    fn from(e: serde_json::Error) -> Self {
        AppError::Internal(e.to_string())
    }
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
