//! Meta-tools for orchestrating ri from within an agent loop.
//!
//! Five tools organized by function:
//!
//! Read:
//! - `readContextGraph`: DAG neighborhood explorer (replaces readSession)
//! - `readMessage`: inspect a single message with provenance
//!
//! Write (the context algebra primitives):
//! - `appendMessage`: create a message and advance a context in one step
//! - `createContext`: compose a context from any set of message IDs
//!
//! Execute:
//! - `runAgent`: spawn a sub-agent loop asynchronously
//!
//! These are constructed with shared state (Weak<AppState>) and registered
//! alongside the base coding tools. Sub-agents spawned by runAgent receive
//! only the base tools (no recursion into meta-tools).

use std::collections::{HashSet, VecDeque};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast};
use tokio_util::sync::CancellationToken;

use ri::{
    ContentBlock, ContextId, Message, MessageId, Role, SessionHeader, SessionId, Store,
    ThinkingLevel, Tool, ToolContext, ToolOutput,
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
        Arc::new(ReadContextGraphTool { app: app.clone() }),
        Arc::new(ReadMessageTool { app: app.clone() }),
        Arc::new(AppendMessageTool { app: app.clone() }),
        Arc::new(CreateContextTool { app }),
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
    fn name(&self) -> &str {
        "runAgent"
    }

    fn description(&self) -> &str {
        "Starts a full agent loop (LLM turn, tool calls, repeat until done), async. \
         Writes resulting messages back to the store and updates the session head. \
         Requires either context_id or message_ids for conversation history. \
         Session can be a new name and the corresponding session will be \
         created. If not provided a random one will be created and returned. *Always* use \
         `readContextGraph` on the current session before using runAgent to pass context from this session."
    }

    fn parameters(&self) -> Value {
        let models = ri_ai::registry::available_model_ids().join(", ");
        json!({
            "type": "object",
            "properties": {
                "context_id": {
                    "type": "string",
                    "description": "A context ID to use as the prompt history. Resolves to its message list. Preferred over message_ids."
                },
                "message_ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "A list of message ids making up the prompt history for this turn to start from. Used when context_id is not provided."
                },
                "user_prompt": {
                    "type": "string",
                    "description": "Text to append as a user message just before initiating the new turn."
                },
                "session_id": {
                    "type": "string",
                    "description": "The session to update with the turn. Use an existing session to extend it, or skip and a random id will be created."
                },
                "model_id": {
                    "type": "string",
                    "description": format!("The model identifier to use for the turn. Available models: {}", models)
                },
                "model_params": {
                    "type": "object",
                    "description": "Parameters to pass to the model.",
                    "properties": {
                        "thinking": { "type": "string", "description": "Thinking level: off, low, medium, high, xhigh" },
                        "max_tokens": { "type": "integer", "description": "Maximum output tokens." }
                    }
                }
            },
            "required": ["model_id"]
        })
    }

    async fn run(&self, input: Value, ctx: ToolContext, _cancel: CancellationToken) -> ToolOutput {
        let app = match self.app.upgrade() {
            Some(a) => a,
            None => return err("ri server is shutting down"),
        };

        // -- Parse inputs --
        // context_id wins over message_ids. At least one must be provided.

        let message_ids: Vec<MessageId> = if let Some(cid) = input.get("context_id").and_then(|v| v.as_str()) {
            match resolve_context_messages(&app, ctx.session_id.as_ref(), cid).await {
                Some(ids) => ids,
                None => return err(&format!("context '{}' not found", cid)),
            }
        } else if let Some(arr) = input.get("message_ids").and_then(|v| v.as_array()) {
            arr.iter()
                .filter_map(|v| v.as_str().map(MessageId::from))
                .collect()
        } else {
            return err("either 'context_id' or 'message_ids' is required");
        };

        let model_id = match input.get("model_id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return err("missing 'model_id' parameter"),
        };

        let user_prompt = input
            .get("user_prompt")
            .and_then(|v| v.as_str())
            .map(String::from);
        let session_id = input
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(String::from);

        let params = input.get("model_params");
        let thinking = params
            .and_then(|p| p.get("thinking"))
            .and_then(|v| v.as_str())
            .and_then(parse_thinking)
            .unwrap_or(app.default_thinking);
        let max_tokens = params
            .and_then(|p| p.get("max_tokens"))
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .map(|n| n as usize);

        // -- Resolve model --

        let (provider, model) = match ri_ai::registry::resolve(&model_id).await {
            Ok(r) => r,
            Err(e) => return err(&format!("model resolution failed: {}", e)),
        };

        // -- Create or find session --

        let parent = ctx.session_id.as_ref();
        let (session_arc, file_id) =
            match setup_session(&app, session_id, &message_ids, &ctx.cwd, parent).await {
                Ok(v) => v,
                Err(e) => return err(&format!("session setup failed: {}", e)),
            };

        // -- Optionally write user prompt --

        if let Some(text) = &user_prompt {
            let mut lock = session_arc.lock().await;
            let sid = lock.file_id.clone();
            match lock.store.write_message(
                &sid,
                Role::User,
                vec![ContentBlock::text(text)],
                None,
            ) {
                Ok(msg) => lock.message_ids.push(msg.id),
                Err(e) => return err(&format!("failed to write user message: {}", e)),
            }
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
                    &session,
                    provider.as_ref(),
                    &model,
                    &tools,
                    thinking,
                    max_tokens,
                    &cancel_inner,
                )
                .await;
                if let Err(e) = result {
                    let mut lock = session.lock().await;
                    let sid = lock.file_id.clone();
                    if let Ok(msg) = lock.store.write_message(
                        &sid,
                        ri::Role::Assistant,
                        vec![ri::ContentBlock::error(e.to_string())],
                        None,
                    ) {
                        lock.message_ids.push(msg.id.clone());
                    }
                    let ids = lock.message_ids.clone();
                    let _ = lock.store.checkpoint(&sid, &ids, None);
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
    message_ids: &[MessageId],
    fallback_cwd: &PathBuf,
    parent: Option<&SessionId>,
) -> eyre::Result<(Arc<Mutex<SessionState>>, SessionId)> {
    // If a session_id was provided, try to find it in memory or on disk.
    if let Some(ref id) = session_id {
        let mut sessions = app.sessions.write().await;
        if let Some(session) = sessions.get(id.as_str()) {
            let mut lock = session.lock().await;
            if lock.is_running() {
                return Err(eyre::eyre!("session '{}' already has a running agent", id));
            }
            lock.message_ids = message_ids.to_vec();
            return Ok((session.clone(), SessionId::from(id.as_str())));
        }

        // Try loading from disk.
        let path = app.sessions_dir.join(format!("{}.jsonl", id));
        if path.exists() {
            let header = read_header(&path)?;
            let session_cwd = header
                .cwd
                .map(PathBuf::from)
                .unwrap_or_else(|| fallback_cwd.clone());
            let mut store = Store::new(app.sessions_dir.clone());
            store.load_all()?;
            let sid = SessionId::from(id.as_str());
            let (events_tx, _) = broadcast::channel(256);
            let state = SessionState {
                store,
                message_ids: message_ids.to_vec(),
                cwd: session_cwd,
                name: header.session,
                ts: header.ts,
                file_id: sid.clone(),
                parent: header.parent.map(SessionId::from),
                events_tx,
                current_run: None,
                title_gen_seq: 0,
            };
            let arc = Arc::new(Mutex::new(state));
            sessions.insert(id.clone(), arc.clone());
            return Ok((arc, sid));
        }

        // Session not found -- fall through to creation with this name.
    }

    // Create a new session.
    let name = session_id.unwrap_or_else(|| ri::gen_id());
    let cwd_str = fallback_cwd.to_string_lossy().to_string();
    let mut store = Store::new(app.sessions_dir.clone());
    store.load_all()?;
    let file_id = store.create_session(&name, &cwd_str, parent)?;

    let ts = chrono::Utc::now().to_rfc3339();
    let (events_tx, _) = broadcast::channel(256);
    let state = SessionState {
        store,
        message_ids: message_ids.to_vec(),
        cwd: fallback_cwd.clone(),
        name: name.clone(),
        ts,
        file_id: file_id.clone(),
        parent: parent.cloned(),
        events_tx,
        current_run: None,
        title_gen_seq: 0,
    };
    let arc = Arc::new(Mutex::new(state));
    app.sessions
        .write()
        .await
        .insert(file_id.to_string(), arc.clone());
    Ok((arc, file_id))
}

// ---------------------------------------------------------------------------
// readContextGraph
// ---------------------------------------------------------------------------

/// DAG neighborhood explorer. Shows all contexts reachable from an entry
/// point (session head or explicit context), with diffs showing how each
/// context's message list changed from its parent.
struct ReadContextGraphTool {
    app: Weak<AppState>,
}

/// Default depth limit for ancestor/descendant traversal.
const GRAPH_DEPTH: usize = 10;

#[async_trait]
impl Tool for ReadContextGraphTool {
    fn name(&self) -> &str {
        "readContextGraph"
    }

    fn description(&self) -> &str {
        "Explore the context DAG reachable from a session or context. Requires \
         either session_id or context_id as an entry point. \
         Returns a flat list of context nodes with parent references \
         and diffs showing which messages were added/removed at each step. \
         The entry context shows its full message list. Message IDs include \
         inline summaries so you can understand the conversation without \
         follow-up readMessage calls."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "Session to explore (resolves to its head context)."
                },
                "context_id": {
                    "type": "string",
                    "description": "Context to explore directly. Takes precedence over session_id."
                },
                "depth": {
                    "type": "integer",
                    "description": "Max traversal depth in each direction. Default 10."
                }
            }
        })
    }

    async fn run(&self, input: Value, _ctx: ToolContext, _cancel: CancellationToken) -> ToolOutput {
        let app = match self.app.upgrade() {
            Some(a) => a,
            None => return err("ri server is shutting down"),
        };

        let depth = input.get("depth")
            .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
            .map(|n| n as usize)
            .unwrap_or(GRAPH_DEPTH);

        // Resolve entry point: context_id takes precedence, then session_id.
        let context_id_str = input.get("context_id").and_then(|v| v.as_str());
        let session_id_str = input.get("session_id").and_then(|v| v.as_str());

        let entry_id = if let Some(cid) = context_id_str {
            cid.to_string()
        } else if let Some(sid) = session_id_str {
            match resolve_session_head(&app, sid).await {
                Some(head) => head,
                None => return err(&format!("session '{}' not found or has no head", sid)),
            }
        } else {
            return err("either 'session_id' or 'context_id' is required");
        };

        // Load a store snapshot for graph traversal.
        let mut store = Store::new(app.sessions_dir.clone());
        if let Err(e) = store.load_all() {
            return err(&format!("failed to load store: {}", e));
        }

        // Also merge in any in-memory contexts/messages that haven't been
        // checkpointed yet (active sessions have fresh objects in their pools).
        merge_in_memory_state(&app, &mut store).await;

        let pool = &store.pool;

        let entry = match pool.get_context(&entry_id) {
            Some(ctx) => ctx,
            None => return err(&format!("context '{}' not found", entry_id)),
        };

        // Collect all reachable contexts via BFS in both directions.
        let mut visited = HashSet::new();
        let mut reachable: Vec<String> = Vec::new();
        visited.insert(entry_id.clone());

        // Backward: walk parents.
        {
            let mut back_queue: VecDeque<(String, usize)> = VecDeque::new();
            for pid in &entry.parents {
                back_queue.push_back((pid.to_string(), 1));
            }
            while let Some((id, d)) = back_queue.pop_front() {
                if d > depth || !visited.insert(id.clone()) { continue; }
                reachable.push(id.clone());
                if let Some(ctx) = pool.get_context(&id) {
                    for pid in &ctx.parents {
                        back_queue.push_back((pid.to_string(), d + 1));
                    }
                }
            }
        }

        // Forward: walk children.
        {
            let mut fwd_queue: VecDeque<(String, usize)> = VecDeque::new();
            for child in pool.children(&entry_id) {
                fwd_queue.push_back((child.id.to_string(), 1));
            }
            while let Some((id, d)) = fwd_queue.pop_front() {
                if d > depth || !visited.insert(id.clone()) { continue; }
                reachable.push(id.clone());
                for child in pool.children(&id) {
                    fwd_queue.push_back((child.id.to_string(), d + 1));
                }
            }
        }

        // Format as text: compact representation optimized for LLM consumption.
        let total = 1 + reachable.len();
        let mut out = format!("CONTEXT GRAPH entry={} count={}\n", entry_id, total);

        // Entry context (full message list).
        out.push('\n');
        format_context_header(&mut out, &entry_id, &entry.parents, true);
        format_message_list(&mut out, pool, &entry.messages);

        // Remaining reachable contexts.
        for id in &reachable {
            let ctx = match pool.get_context(&id) {
                Some(c) => c,
                None => {
                    out.push('\n');
                    format_context_header(&mut out, id, &[], false);
                    out.push_str("  (not loaded)\n");
                    continue;
                }
            };

            out.push('\n');
            format_context_header(&mut out, id, &ctx.parents, false);

            if let Some(parent_id) = ctx.parents.first() {
                if let Some(parent) = pool.get_context(parent_id.as_str()) {
                    let (added, removed) = diff_message_lists(&parent.messages, &ctx.messages);
                    format_diff(&mut out, pool, parent_id, &added, &removed);
                } else {
                    // Parent not in pool -- show full list.
                    format_message_list(&mut out, pool, &ctx.messages);
                }
            } else {
                // Root context (no parents) -- show full list.
                format_message_list(&mut out, pool, &ctx.messages);
            }
        }

        ToolOutput {
            text: out,
            is_error: false,
            details: None,
        }
    }
}

/// Resolve a session ID to its head context ID. Checks in-memory first, then disk.
async fn resolve_session_head(app: &AppState, session_id: &str) -> Option<String> {
    // In-memory sessions have the most current head.
    let sessions = app.sessions.read().await;
    if let Some(session) = sessions.get(session_id) {
        let lock = session.lock().await;
        return lock.store.get_session(lock.file_id.as_str())
            .map(|s| s.head.to_string());
    }
    drop(sessions);

    // Fall back to disk.
    let mut store = Store::new(app.sessions_dir.clone());
    store.load_all().ok()?;
    let session = store.get_session(session_id)?;
    Some(session.head.to_string())
}

/// Merge contexts and messages from in-memory sessions into a store snapshot.
/// Active sessions may have objects that haven't been checkpointed to disk yet.
async fn merge_in_memory_state(app: &AppState, store: &mut Store) {
    let sessions = app.sessions.read().await;
    for (_sid, session_arc) in sessions.iter() {
        if let Ok(lock) = session_arc.try_lock() {
            // Copy messages that the disk store doesn't have yet.
            for mid in &lock.message_ids {
                if store.pool.get_message(mid.as_str()).is_none() {
                    if let Some(msg) = lock.store.pool.get_message(mid.as_str()) {
                        store.pool.put_message(msg.clone());
                    }
                }
            }
            // Copy contexts visible in the session's pool.
            if let Some(s) = lock.store.get_session(lock.file_id.as_str()) {
                if store.pool.get_context(s.head.as_str()).is_none() {
                    if let Some(ctx) = lock.store.pool.get_context(s.head.as_str()) {
                        store.pool.put_context(ctx.clone());
                    }
                }
            }
        }
    }
}

/// Compute added/removed message IDs between a parent and child context.
fn diff_message_lists(parent: &[MessageId], child: &[MessageId]) -> (Vec<MessageId>, Vec<MessageId>) {
    let parent_set: HashSet<&str> = parent.iter().map(|id| id.as_str()).collect();
    let child_set: HashSet<&str> = child.iter().map(|id| id.as_str()).collect();

    let added: Vec<MessageId> = child.iter()
        .filter(|id| !parent_set.contains(id.as_str()))
        .cloned()
        .collect();
    let removed: Vec<MessageId> = parent.iter()
        .filter(|id| !child_set.contains(id.as_str()))
        .cloned()
        .collect();

    (added, removed)
}

/// Write a context header line to the output buffer.
///
/// Format: `<id> (entry) <- <parent1>, <parent2>`
/// Root contexts (no parents) omit the `<-`. Non-entry contexts omit `(entry)`.
fn format_context_header(out: &mut String, id: &str, parents: &[ContextId], is_entry: bool) {
    out.push_str(id);
    if is_entry {
        out.push_str(" (entry)");
    }
    if !parents.is_empty() {
        out.push_str(" <- ");
        for (i, p) in parents.iter().enumerate() {
            if i > 0 { out.push_str(", "); }
            out.push_str(p.as_str());
        }
    }
    out.push('\n');
}

/// Write indented message lines with inline summaries, or "(no messages)" for empty contexts.
fn format_message_list(out: &mut String, pool: &ri::Pool, messages: &[MessageId]) {
    if messages.is_empty() {
        out.push_str("  (no messages)\n");
        return;
    }
    for id in messages {
        out.push_str("  ");
        out.push_str(id.as_str());
        if let Some(msg) = pool.get_message(id.as_str()) {
            out.push(' ');
            out.push_str(&msg.summarize());
        }
        out.push('\n');
    }
}

/// Write a diff block showing added (+) and removed (-) messages with summaries.
fn format_diff(out: &mut String, pool: &ri::Pool, vs_id: &ContextId, added: &[MessageId], removed: &[MessageId]) {
    if added.is_empty() && removed.is_empty() {
        out.push_str("  (no changes)\n");
        return;
    }
    out.push_str("  diff vs ");
    out.push_str(vs_id.as_str());
    out.push('\n');
    for id in added {
        out.push_str("  + ");
        out.push_str(id.as_str());
        if let Some(msg) = pool.get_message(id.as_str()) {
            out.push(' ');
            out.push_str(&msg.summarize());
        }
        out.push('\n');
    }
    for id in removed {
        out.push_str("  - ");
        out.push_str(id.as_str());
        if let Some(msg) = pool.get_message(id.as_str()) {
            out.push(' ');
            out.push_str(&msg.summarize());
        }
        out.push('\n');
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
    fn name(&self) -> &str {
        "readMessage"
    }

    fn description(&self) -> &str {
        "Returns the full text of a single message, and the provenance & \
         metadata associated with its creation. Useful for precise reading \
         of message data when you want to inspect a message id."
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

    async fn run(&self, input: Value, _ctx: ToolContext, _cancel: CancellationToken) -> ToolOutput {
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
            if let Some(msg) = lock.store.pool.get_message(message_id) {
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
// appendMessage
// ---------------------------------------------------------------------------

/// Create a message and advance a context in one atomic step.
///
/// Creates a message, then creates a new context with the old context's
/// messages plus the new one. The old context becomes a parent. If no
/// context_id is provided, creates a fresh context with just the new message.
struct AppendMessageTool {
    app: Weak<AppState>,
}

#[async_trait]
impl Tool for AppendMessageTool {
    fn name(&self) -> &str {
        "appendMessage"
    }

    fn description(&self) -> &str {
        "Create a message and advance a context in one step. Returns both \
         the new context_id (primary -- pass to appendMessage, createContext, \
         or runAgent) and the message_id (secondary -- useful for cherry-picking \
         in createContext). If context_id is omitted, creates a fresh context \
         containing only the new message."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "context_id": {
                    "type": "string",
                    "description": "Context to append to. If omitted, creates a fresh context."
                },
                "role": {
                    "type": "string",
                    "enum": ["user", "assistant", "system"],
                    "description": "The role of the message."
                },
                "content": {
                    "type": "string",
                    "description": "Text content of the message."
                }
            },
            "required": ["role", "content"]
        })
    }

    async fn run(&self, input: Value, ctx: ToolContext, _cancel: CancellationToken) -> ToolOutput {
        let app = match self.app.upgrade() {
            Some(a) => a,
            None => return err("ri server is shutting down"),
        };

        let role = match input.get("role").and_then(|v| v.as_str()).and_then(parse_role) {
            Some(r) => r,
            None => return err("missing or invalid 'role' parameter (user, assistant, system)"),
        };
        let content = match input.get("content").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return err("missing 'content' parameter"),
        };
        let parent_context_id = input.get("context_id").and_then(|v| v.as_str());

        let session_id = match ctx.session_id.as_ref() {
            Some(id) => id,
            None => return err("no calling session -- appendMessage requires a session context"),
        };

        let sessions = app.sessions.read().await;
        let session = match sessions.get(session_id.as_str()) {
            Some(s) => s,
            None => return err(&format!("calling session '{}' not found in memory", session_id)),
        };

        let mut lock = session.lock().await;
        let sid = lock.file_id.clone();

        // Resolve the parent context's messages (if any).
        let parent_messages: Vec<MessageId> = if let Some(cid) = parent_context_id {
            match lock.store.pool.get_context(cid) {
                Some(ctx) => ctx.messages.clone(),
                None => return err(&format!("context '{}' not found", cid)),
            }
        } else {
            Vec::new()
        };

        // Write the new message.
        let msg = match lock.store.write_message(
            &sid,
            role,
            vec![ContentBlock::text(content)],
            None,
        ) {
            Ok(m) => m,
            Err(e) => return err(&format!("failed to write message: {}", e)),
        };

        // Build the new context: parent's messages + new message.
        let mut new_messages = parent_messages;
        new_messages.push(msg.id.clone());

        let parents: Vec<ContextId> = parent_context_id
            .map(|cid| vec![ContextId::from(cid)])
            .unwrap_or_default();

        let new_ctx = match lock.store.write_context(
            &sid,
            new_messages,
            parents,
            None,
        ) {
            Ok(c) => c,
            Err(e) => return err(&format!("failed to write context: {}", e)),
        };

        let msg_id = msg.id.to_string();
        let ctx_id = new_ctx.id.to_string();
        ToolOutput {
            text: format!("Appended message [{}], new context [{}] ({} messages)",
                msg_id, ctx_id, new_ctx.messages.len()),
            is_error: false,
            details: Some(json!({
                "context_id": ctx_id,
                "message_id": msg_id,
            })),
        }
    }
}

// ---------------------------------------------------------------------------
// createContext
// ---------------------------------------------------------------------------

/// Compose a new context from an arbitrary set of message IDs.
///
/// The fundamental write primitive: any context algebra operation
/// (append, merge, filter, fork, compact) is expressed as a createContext
/// call with the right message list and parents.
struct CreateContextTool {
    app: Weak<AppState>,
}

#[async_trait]
impl Tool for CreateContextTool {
    fn name(&self) -> &str {
        "createContext"
    }

    fn description(&self) -> &str {
        "Create a new context from an ordered list of message IDs. \
         Optionally specify parent context IDs for DAG lineage. \
         Returns the new context_id. Use this to compose, merge, \
         filter, or fork contexts."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "message_ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Ordered list of message IDs that make up this context."
                },
                "parents": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Parent context IDs for DAG lineage tracking. Optional."
                }
            },
            "required": ["message_ids"]
        })
    }

    async fn run(&self, input: Value, ctx: ToolContext, _cancel: CancellationToken) -> ToolOutput {
        let app = match self.app.upgrade() {
            Some(a) => a,
            None => return err("ri server is shutting down"),
        };

        let message_ids: Vec<MessageId> = match input.get("message_ids").and_then(|v| v.as_array()) {
            Some(arr) => arr.iter()
                .filter_map(|v| v.as_str().map(MessageId::from))
                .collect(),
            None => return err("missing 'message_ids' parameter"),
        };

        let parents: Vec<ContextId> = input.get("parents")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter()
                .filter_map(|v| v.as_str().map(ContextId::from))
                .collect())
            .unwrap_or_default();

        let session_id = match ctx.session_id.as_ref() {
            Some(id) => id,
            None => return err("no calling session -- createContext requires a session context"),
        };

        let sessions = app.sessions.read().await;
        let session = match sessions.get(session_id.as_str()) {
            Some(s) => s,
            None => return err(&format!("calling session '{}' not found in memory", session_id)),
        };

        let mut lock = session.lock().await;
        let sid = lock.file_id.clone();
        let context = match lock.store.write_context(
            &sid,
            message_ids,
            parents,
            None,
        ) {
            Ok(c) => c,
            Err(e) => return err(&format!("failed to write context: {}", e)),
        };

        let id = context.id.to_string();
        ToolOutput {
            text: format!("Created context [{}] ({} messages)", id, context.messages.len()),
            is_error: false,
            details: Some(json!({ "context_id": id })),
        }
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Resolve a context_id to its message list. Checks the caller's in-memory
/// pool first, then falls back to loading everything from disk.
async fn resolve_context_messages(
    app: &AppState,
    caller_sid: Option<&SessionId>,
    context_id: &str,
) -> Option<Vec<MessageId>> {
    // Try the caller's in-memory pool first.
    if let Some(sid) = caller_sid {
        let sessions = app.sessions.read().await;
        if let Some(session) = sessions.get(sid.as_str()) {
            if let Ok(lock) = session.try_lock() {
                if let Some(ctx) = lock.store.pool.get_context(context_id) {
                    return Some(ctx.messages.clone());
                }
            }
        }
    }

    // Fallback: load everything from disk.
    let mut store = Store::new(app.sessions_dir.clone());
    store.load_all().ok()?;
    let ctx = store.pool.get_context(context_id)?;
    Some(ctx.messages.clone())
}

fn err(msg: &str) -> ToolOutput {
    ToolOutput {
        text: msg.to_string(),
        is_error: true,
        details: None,
    }
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

fn parse_role(s: &str) -> Option<Role> {
    match s {
        "user" => Some(Role::User),
        "assistant" => Some(Role::Assistant),
        "system" => Some(Role::System),
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

/// Search all session files for a message with the given ID.
fn find_message_on_disk(message_id: &str, sessions_dir: &std::path::Path) -> Option<Message> {
    let mut store = Store::new(sessions_dir.to_path_buf());
    store.load_all().ok()?;
    store.pool.get_message(message_id).cloned()
}
