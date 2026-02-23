//! Meta-tools for orchestrating ri from within an agent loop.
//!
//! Three tools that let an LLM agent control ri itself:
//! - `runAgent`: spawn a sub-agent loop asynchronously
//! - `readSession`: read a session's message history
//! - `readMessage`: inspect a single message with provenance
//!
//! These are constructed with shared state (Weak<AppState>) and registered
//! alongside the base coding tools. Sub-agents spawned by runAgent receive
//! only the base tools (no recursion into meta-tools).

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::{broadcast, Mutex};
use tokio_util::sync::CancellationToken;

use ri::{
    ContentBlock, Message, Role, SessionHeader, SessionStore, ThinkingLevel,
    Tool, ToolOutput,
};

use crate::agent;
use crate::state::{AppState, RunHandle, SessionState};

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

/// Build the meta-tools. Called during app startup with a Weak reference
/// to AppState (via Arc::new_cyclic) so that runAgent can access the
/// shared sessions map and tool list.
pub fn create(app: Weak<AppState>) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(RunAgentTool { app: app.clone() }),
        Arc::new(ReadSessionTool { app: app.clone() }),
        Arc::new(ReadMessageTool { app }),
    ]
}

// ---------------------------------------------------------------------------
// runAgent
// ---------------------------------------------------------------------------

/// Spawns a sub-agent loop asynchronously.
///
/// Creates (or extends) a session, optionally writes a user message, then
/// runs the full agent loop in a background task. Returns immediately with
/// the session ID so the calling agent can continue and check back later.
struct RunAgentTool {
    app: Weak<AppState>,
}

#[async_trait]
impl Tool for RunAgentTool {
    fn name(&self) -> &str { "runAgent" }

    fn description(&self) -> &str {
        "Run an LLM agent loop asynchronously. The agent resolves messages, \
         calls the model, executes tool calls, and repeats until the model \
         stops. All resulting messages are written to the store and the \
         session is updated. Returns immediately with the session ID."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "message_ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Message IDs forming the prompt history."
                },
                "user_prompt": {
                    "type": "string",
                    "description": "Text to append as a user message before starting."
                },
                "session_id": {
                    "type": "string",
                    "description": "Session to update. Omit to create a new one."
                },
                "model_id": {
                    "type": "string",
                    "description": "Model identifier (e.g. 'claude-sonnet-4-20250514')."
                },
                "model_params": {
                    "type": "object",
                    "properties": {
                        "thinking": { "type": "string", "description": "Thinking level: off, low, medium, high, xhigh" },
                        "max_tokens": { "type": "integer", "description": "Maximum output tokens." }
                    }
                }
            },
            "required": ["message_ids", "model_id"]
        })
    }

    async fn run(&self, input: Value, cwd: PathBuf, _cancel: CancellationToken) -> ToolOutput {
        let app = match self.app.upgrade() {
            Some(a) => a,
            None => return err("ri server is shutting down"),
        };

        // -- Parse inputs --

        let message_ids: Vec<String> = match input.get("message_ids").and_then(|v| v.as_array()) {
            Some(arr) => arr.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
            None => return err("missing 'message_ids' parameter"),
        };

        let model_id = match input.get("model_id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return err("missing 'model_id' parameter"),
        };

        let user_prompt = input.get("user_prompt").and_then(|v| v.as_str()).map(String::from);
        let session_id = input.get("session_id").and_then(|v| v.as_str()).map(String::from);

        let params = input.get("model_params");
        let thinking = params
            .and_then(|p| p.get("thinking"))
            .and_then(|v| v.as_str())
            .and_then(parse_thinking)
            .unwrap_or(app.default_thinking);
        let max_tokens = params
            .and_then(|p| p.get("max_tokens"))
            .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
            .map(|n| n as usize);

        // -- Resolve model --

        let (provider, model) = match ri_ai::registry::resolve(&model_id).await {
            Ok(r) => r,
            Err(e) => return err(&format!("model resolution failed: {}", e)),
        };

        // -- Create or find session --

        let (session_arc, file_id) = match setup_session(
            &app, session_id, &message_ids, &cwd,
        ).await {
            Ok(v) => v,
            Err(e) => return err(&format!("session setup failed: {}", e)),
        };

        // -- Optionally write user prompt --

        if let Some(text) = &user_prompt {
            let mut lock = session_arc.lock().await;
            let user_id = lock.store.next_id();
            let user_msg = Message::new(
                user_id.clone(), Role::User, vec![ContentBlock::text(text)],
            );
            if let Err(e) = lock.store.write_message(user_msg) {
                return err(&format!("failed to write user message: {}", e));
            }
            lock.message_ids.push(user_id);
        }

        // -- Spawn the agent loop --

        let cancel = CancellationToken::new();

        let task = {
            let session = session_arc.clone();
            let provider: Arc<dyn ri::LlmProvider> = Arc::from(provider);
            let tools = app.base_tools.clone();
            let cancel_inner = cancel.clone();
            tokio::spawn(async move {
                let result = agent::run_loop(
                    &session, provider.as_ref(), &model,
                    &tools, thinking, max_tokens, &cancel_inner,
                ).await;
                if let Err(e) = result {
                    let lock = session.lock().await;
                    let _ = lock.events_tx.send(agent::AgentEvent::Error(e.to_string()));
                }
                let mut lock = session.lock().await;
                lock.current_run = None;
            })
        };

        {
            let mut lock = session_arc.lock().await;
            lock.current_run = Some(RunHandle { cancel, task });
        }

        ToolOutput {
            text: format!("Agent loop started on session '{}'", file_id),
            is_error: false,
            details: Some(json!({ "session_id": file_id })),
        }
    }
}

/// Create or load a session for runAgent, seeding it with the given message_ids.
async fn setup_session(
    app: &AppState,
    session_id: Option<String>,
    message_ids: &[String],
    fallback_cwd: &PathBuf,
) -> eyre::Result<(Arc<Mutex<SessionState>>, String)> {
    // If a session_id was provided, try to find it in memory or on disk.
    if let Some(ref id) = session_id {
        let mut sessions = app.sessions.write().await;
        if let Some(session) = sessions.get(id) {
            let mut lock = session.lock().await;
            if lock.is_running() {
                return Err(eyre::eyre!("session '{}' already has a running agent", id));
            }
            lock.message_ids = message_ids.to_vec();
            return Ok((session.clone(), id.clone()));
        }

        // Try loading from disk.
        let path = app.sessions_dir.join(format!("{}.jsonl", id));
        if path.exists() {
            let header = read_header(&path)?;
            let session_cwd = header.cwd.map(PathBuf::from)
                .unwrap_or_else(|| fallback_cwd.clone());
            let mut store = SessionStore::new(app.sessions_dir.clone());
            store.load_all()?;
            let (events_tx, _) = broadcast::channel(256);
            let state = SessionState {
                store,
                message_ids: message_ids.to_vec(),
                cwd: session_cwd,
                name: header.session,
                ts: header.ts,
                events_tx,
                current_run: None,
            };
            let arc = Arc::new(Mutex::new(state));
            sessions.insert(id.clone(), arc.clone());
            return Ok((arc, id.clone()));
        }

        // Session not found -- fall through to creation with this name.
    }

    // Create a new session.
    let name = session_id.unwrap_or_else(|| ri::gen_id());
    let cwd_str = fallback_cwd.to_string_lossy().to_string();
    let mut store = SessionStore::new(app.sessions_dir.clone());
    store.load_all()?;
    let session_path = store.new_session(&name, &cwd_str)?;
    let file_id = session_path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();

    // Persist the initial message_ids in the header metadata so they survive
    // reload from disk. The messages themselves live in other session files.
    if !message_ids.is_empty() {
        rewrite_header_with_initial_ids(&session_path, message_ids)?;
    }

    let ts = chrono::Utc::now().to_rfc3339();
    let (events_tx, _) = broadcast::channel(256);
    let state = SessionState {
        store,
        message_ids: message_ids.to_vec(),
        cwd: fallback_cwd.clone(),
        name: name.clone(),
        ts,
        events_tx,
        current_run: None,
    };
    let arc = Arc::new(Mutex::new(state));
    app.sessions.write().await.insert(file_id.clone(), arc.clone());
    Ok((arc, file_id))
}

/// Rewrite the first line of a session JSONL file to include initial_ids.
fn rewrite_header_with_initial_ids(path: &std::path::Path, ids: &[String]) -> eyre::Result<()> {
    let content = std::fs::read_to_string(path)?;
    let mut lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return Ok(());
    }
    let mut header: Value = serde_json::from_str(lines[0])?;
    header["initial_ids"] = json!(ids);
    let new_header = serde_json::to_string(&header)?;
    lines[0] = &new_header;
    std::fs::write(path, lines.join("\n") + "\n")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// readSession
// ---------------------------------------------------------------------------

/// Read a session's message history from its JSONL file.
struct ReadSessionTool {
    app: Weak<AppState>,
}

#[async_trait]
impl Tool for ReadSessionTool {
    fn name(&self) -> &str { "readSession" }

    fn description(&self) -> &str {
        "Read a session's message history in reverse-chronological order. \
         Each entry includes the message ID, role, content, and provenance."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "The session to read."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max number of messages to return."
                },
                "contentLimit": {
                    "type": "integer",
                    "description": "Max bytes of each message's content to return."
                }
            },
            "required": ["session_id"]
        })
    }

    async fn run(&self, input: Value, _cwd: PathBuf, _cancel: CancellationToken) -> ToolOutput {
        let app = match self.app.upgrade() {
            Some(a) => a,
            None => return err("ri server is shutting down"),
        };

        let session_id = match input.get("session_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return err("missing 'session_id' parameter"),
        };
        let limit = input.get("limit")
            .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
            .map(|n| n as usize);
        let content_limit = input.get("contentLimit")
            .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
            .map(|n| n as usize);

        // First check in-memory sessions (they have the most current state).
        let sessions = app.sessions.read().await;
        if let Some(session) = sessions.get(session_id) {
            let lock = session.lock().await;
            let mut messages: Vec<Message> = lock.store.pool
                .resolve_existing(&lock.message_ids)
                .into_iter()
                .cloned()
                .collect();
            messages.reverse();
            return format_session_output(messages, limit, content_limit);
        }
        drop(sessions);

        // Fall back to reading from disk.
        let path = app.sessions_dir.join(format!("{}.jsonl", session_id));
        if !path.exists() {
            return err(&format!("session '{}' not found", session_id));
        }

        match read_session_messages(&path, &app.sessions_dir) {
            Ok(mut messages) => {
                messages.reverse();
                format_session_output(messages, limit, content_limit)
            }
            Err(e) => err(&format!("failed to read session: {}", e)),
        }
    }
}

// ---------------------------------------------------------------------------
// readMessage
// ---------------------------------------------------------------------------

/// Read a single message by ID with full content and provenance.
struct ReadMessageTool {
    app: Weak<AppState>,
}

#[async_trait]
impl Tool for ReadMessageTool {
    fn name(&self) -> &str { "readMessage" }

    fn description(&self) -> &str {
        "Read a single message by ID. Returns the full text, provenance \
         (input message IDs, model, timestamp, usage), and metadata."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "message_id": {
                    "type": "string",
                    "description": "The message ID to read."
                }
            },
            "required": ["message_id"]
        })
    }

    async fn run(&self, input: Value, _cwd: PathBuf, _cancel: CancellationToken) -> ToolOutput {
        let app = match self.app.upgrade() {
            Some(a) => a,
            None => return err("ri server is shutting down"),
        };

        let message_id = match input.get("message_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return err("missing 'message_id' parameter"),
        };

        // Check in-memory sessions first.
        let sessions = app.sessions.read().await;
        for (_sid, session) in sessions.iter() {
            let lock = session.lock().await;
            if let Some(msg) = lock.store.pool.get(message_id) {
                let text = serde_json::to_string_pretty(msg).unwrap_or_default();
                return ToolOutput {
                    text,
                    is_error: false,
                    details: Some(serde_json::to_value(msg).unwrap_or_default()),
                };
            }
        }
        drop(sessions);

        // Fall back to searching on disk.
        match find_message_on_disk(message_id, &app.sessions_dir) {
            Some(msg) => {
                let text = serde_json::to_string_pretty(&msg).unwrap_or_default();
                ToolOutput {
                    text,
                    is_error: false,
                    details: Some(serde_json::to_value(&msg).unwrap_or_default()),
                }
            }
            None => err(&format!("message '{}' not found", message_id)),
        }
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn err(msg: &str) -> ToolOutput {
    ToolOutput { text: msg.to_string(), is_error: true, details: None }
}

fn parse_thinking(s: &str) -> Option<ThinkingLevel> {
    match s {
        "off" => Some(ThinkingLevel::Off),
        "low" => Some(ThinkingLevel::Low),
        "medium" => Some(ThinkingLevel::Medium),
        "high" => Some(ThinkingLevel::High),
        "xhigh" => Some(ThinkingLevel::XHigh),
        _ => None,
    }
}

fn read_header(path: &std::path::Path) -> eyre::Result<SessionHeader> {
    let file = std::fs::File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    Ok(serde_json::from_str(line.trim())?)
}

/// Read all messages from a session file, resolving initial_ids from the
/// header by loading the full message pool from disk.
fn read_session_messages(
    path: &std::path::Path,
    sessions_dir: &std::path::Path,
) -> eyre::Result<Vec<Message>> {
    let header = read_header(path)?;

    // Collect initial_ids from header (cross-session references).
    let initial_ids: Vec<String> = header.extra
        .get("initial_ids")
        .and_then(|v: &serde_json::Value| v.as_array())
        .map(|arr: &Vec<serde_json::Value>| {
            arr.iter().filter_map(|v| v.as_str().map(String::from)).collect()
        })
        .unwrap_or_default();

    // Read messages physically in this file.
    let file = std::fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut file_messages: Vec<Message> = Vec::new();
    let mut first = true;
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() { continue; }
        if first {
            first = false;
            if let Ok(obj) = serde_json::from_str::<Value>(trimmed) {
                if obj.get("session").is_some() && obj.get("role").is_none() {
                    continue;
                }
            }
        }
        if let Ok(msg) = serde_json::from_str::<Message>(trimmed) {
            file_messages.push(msg);
        }
    }

    // If there are no initial_ids, just return the file messages.
    if initial_ids.is_empty() {
        return Ok(file_messages);
    }

    // Load the full pool to resolve initial_ids.
    let mut store = SessionStore::new(sessions_dir.to_path_buf());
    store.load_all()?;

    let mut result: Vec<Message> = initial_ids.iter()
        .filter_map(|id| store.pool.get(id).cloned())
        .collect();
    result.extend(file_messages);
    Ok(result)
}

/// Format messages for the readSession tool output.
fn format_session_output(
    mut messages: Vec<Message>,
    limit: Option<usize>,
    content_limit: Option<usize>,
) -> ToolOutput {
    if let Some(n) = limit {
        messages.truncate(n);
    }

    if let Some(max_bytes) = content_limit {
        for msg in &mut messages {
            truncate_content_blocks(&mut msg.content, max_bytes);
        }
    }

    let text = serde_json::to_string_pretty(&messages).unwrap_or_default();
    ToolOutput {
        text,
        is_error: false,
        details: Some(serde_json::to_value(&messages).unwrap_or_default()),
    }
}

/// Truncate text content blocks to at most `max_bytes` bytes.
fn truncate_content_blocks(blocks: &mut Vec<ContentBlock>, max_bytes: usize) {
    for block in blocks.iter_mut() {
        match block {
            ContentBlock::Text { text } => {
                if text.len() > max_bytes {
                    let truncated = truncate_str(text, max_bytes);
                    *text = format!("{}... ({} bytes truncated)", truncated, text.len() - max_bytes);
                }
            }
            ContentBlock::Thinking { thinking, .. } => {
                if thinking.len() > max_bytes {
                    let truncated = truncate_str(thinking, max_bytes);
                    *thinking = format!("{}... ({} bytes truncated)", truncated, thinking.len() - max_bytes);
                }
            }
            ContentBlock::ToolResult { content, .. } => {
                truncate_content_blocks(content, max_bytes);
            }
            _ => {}
        }
    }
}

/// Truncate a string to at most `max` bytes on a char boundary.
fn truncate_str(s: &str, max: usize) -> &str {
    if s.len() <= max { return s; }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Search all JSONL files in sessions_dir for a message with the given ID.
fn find_message_on_disk(message_id: &str, sessions_dir: &std::path::Path) -> Option<Message> {
    let entries = std::fs::read_dir(sessions_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let reader = BufReader::new(file);
        for line in reader.lines().flatten() {
            let trimmed = line.trim();
            if trimmed.is_empty() { continue; }
            // Quick check before full parse.
            if !trimmed.contains(message_id) { continue; }
            if let Ok(msg) = serde_json::from_str::<Message>(trimmed) {
                if msg.id == message_id {
                    return Some(msg);
                }
            }
        }
    }
    None
}
