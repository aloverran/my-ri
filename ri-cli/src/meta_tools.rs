//! Meta-tools for orchestrating ri from within an agent loop.
//!
//! Six tools organized by function:
//!
//! Read:
//! - `readContextGraph`: DAG neighborhood explorer
//! - `readMessage`: inspect a single message with provenance
//!
//! Write (the context algebra primitives):
//! - `appendMessage`: create a message and advance a context in one step
//! - `createContext`: compose a context from any set of message IDs
//!
//! Execute:
//! - `runTurn`: single LLM call (no tools, native capabilities enabled)
//! - `runAgent`: spawn a sub-agent loop asynchronously (LLM + tools, repeats)
//!
//! In the CLI, these tools own all the state they need (sessions_dir)
//! and create fresh Stores when reading. runAgent and runTurn spawn
//! fully self-contained background tasks.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use ri::{
    ContentBlock, ContextId, LlmProvider, Message, MessageId, Model, RequestOptions, Role,
    SessionId, Store, ThinkingLevel, Tool, ToolContext, ToolOutput, ToolSchema,
};
use ri_ai::Turn;

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

/// Build the meta-tools for the CLI. Only needs the sessions directory.
pub fn create(sessions_dir: PathBuf) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(RunAgentTool {
            sessions_dir: sessions_dir.clone(),
        }),
        Box::new(RunTurnTool {
            sessions_dir: sessions_dir.clone(),
        }),
        Box::new(ReadContextGraphTool {
            sessions_dir: sessions_dir.clone(),
        }),
        Box::new(ReadMessageTool {
            sessions_dir: sessions_dir.clone(),
        }),
        Box::new(AppendMessageTool {
            sessions_dir: sessions_dir.clone(),
        }),
        Box::new(CreateContextTool { sessions_dir }),
    ]
}

// ---------------------------------------------------------------------------
// runAgent
// ---------------------------------------------------------------------------

struct RunAgentTool {
    sessions_dir: PathBuf,
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
         created. If not provided a random one will be created and returned."
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
        // -- Parse inputs --
        // context_id wins over message_ids. At least one must be provided.

        let message_ids: Vec<MessageId> = if let Some(cid) = input.get("context_id").and_then(|v| v.as_str()) {
            let mut store = Store::new(self.sessions_dir.clone());
            if let Err(e) = store.load_all() {
                return err(&format!("failed to load store: {}", e));
            }
            match store.pool.get_context(cid) {
                Some(ctx) => ctx.messages.clone(),
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

        let settings = ri_tools::resources::load_settings();
        let params = input.get("model_params");
        let thinking = params
            .and_then(|p| p.get("thinking"))
            .and_then(|v| v.as_str())
            .and_then(parse_thinking)
            .or_else(|| {
                settings
                    .default_thinking
                    .as_deref()
                    .and_then(parse_thinking)
            })
            .unwrap_or(ThinkingLevel::Medium);
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

        // -- Create session --

        let sessions_dir = self.sessions_dir.clone();
        let mut store = Store::new(sessions_dir.clone());
        if let Err(e) = store.load_all() {
            return err(&format!("failed to load sessions: {}", e));
        }

        let name = session_id.unwrap_or_else(|| ri::gen_id());
        let cwd_str = ctx.cwd.to_string_lossy().to_string();
        let parent = ctx.session_id.as_ref();
        let mut msg_ids = message_ids;
        let file_id = match store.create_session(&name, &cwd_str, parent) {
            Ok(v) => v,
            Err(e) => return err(&format!("failed to create session: {}", e)),
        };

        // -- Optionally write user prompt --

        if let Some(text) = &user_prompt {
            match store.write_message(
                &file_id,
                Role::User,
                vec![ContentBlock::text(text)],
                None,
            ) {
                Ok(msg) => msg_ids.push(msg.id),
                Err(e) => return err(&format!("failed to write user message: {}", e)),
            }
        }

        // -- Spawn background agent loop --

        let cwd_clone = ctx.cwd.clone();
        let session_id_clone = file_id.clone();
        tokio::spawn(async move {
            if let Err(e) = run_background_loop(
                provider,
                model,
                store,
                msg_ids,
                cwd_clone,
                thinking,
                max_tokens,
                session_id_clone,
            )
            .await
            {
                tracing::error!("background agent loop failed: {}", e);
            }
        });

        ToolOutput {
            text: format!("Agent loop started on session '{}'", file_id),
            is_error: false,
            details: Some(json!({ "session_id": file_id })),
        }
    }
}

/// Self-contained agent loop that owns all its data. Runs in a background task.
///
/// Mirrors the logic in agent.rs but with owned values instead of borrows,
/// allowing it to run as a detached tokio task. The system prompt is
/// extracted from the first System-role message in the history.
async fn run_background_loop(
    provider: Box<dyn LlmProvider>,
    model: Model,
    mut store: Store,
    mut message_ids: Vec<MessageId>,
    cwd: PathBuf,
    thinking: ThinkingLevel,
    max_tokens: Option<usize>,
    session_id: SessionId,
) -> eyre::Result<()> {
    // Extract system prompt from the first System message.
    let system_prompt = store
        .pool
        .resolve(&message_ids)
        .iter()
        .find(|m| m.role == Role::System)
        .and_then(|m| {
            m.content.iter().find_map(|b| {
                if let ContentBlock::Text { text, .. } = b {
                    Some(text.clone())
                } else {
                    None
                }
            })
        })
        .unwrap_or_else(|| ri_tools::resources::BASE_SYSTEM_PROMPT.to_string());

    let tools = ri_tools::all_tools();
    let tool_schemas: Vec<ToolSchema> = tools.iter().map(|t| t.schema()).collect();
    let tool_map: HashMap<&str, &dyn Tool> = tools
        .iter()
        .map(|t| (t.name(), t.as_ref() as &dyn Tool))
        .collect();

    let cancel = CancellationToken::new();

    loop {
        let input_ids: Vec<MessageId> = message_ids.clone();
        let messages: Vec<Message> = store
            .pool
            .resolve(&input_ids)
            .into_iter()
            .cloned()
            .collect();

        let opts = RequestOptions {
            model: model.clone(),
            system_prompt: system_prompt.clone(),
            messages,
            tools: tool_schemas.clone(),
            thinking,
            max_tokens,
            native_tools: false,
        };

        let mut turn = match Turn::start(provider.as_ref(), opts).await {
            Ok(t) => t,
            Err(e) => {
                let msg = store.write_message(
                    &session_id,
                    Role::Assistant,
                    vec![ContentBlock::error(e.to_string())],
                    Some(serde_json::json!({
                        "model": model.id,
                        "ts": chrono::Utc::now().to_rfc3339(),
                    })),
                )?;
                message_ids.push(msg.id);
                break;
            }
        };

        // Drain the stream (no display in background).
        while let Some(result) = turn.next().await {
            if let Err(_) = result {
                break;
            }
        }
        let (content, usage) = turn.finish();

        // Persist assistant message.
        let assistant_msg = store.write_message(
            &session_id,
            Role::Assistant,
            content.clone(),
            Some(serde_json::json!({
                "model": model.id,
                "ts": chrono::Utc::now().to_rfc3339(),
                "usage": usage,
            })),
        )?;
        message_ids.push(assistant_msg.id);

        // Extract and execute tool calls.
        let calls: Vec<(String, String, Value)> = content
            .iter()
            .filter_map(|c| {
                if let ContentBlock::ToolUse {
                    id, name, input, ..
                } = c
                {
                    Some((id.clone(), name.clone(), input.clone()))
                } else {
                    None
                }
            })
            .collect();

        if calls.is_empty() {
            break;
        }

        let mut results: Vec<ContentBlock> = Vec::new();
        for (call_id, call_name, call_input) in &calls {
            let output = match tool_map.get(call_name.as_str()) {
                Some(tool) => {
                    // Sub-agents only receive base tools (no runAgent), so no session_id needed.
                    let ctx = ToolContext {
                        cwd: cwd.clone(),
                        session_id: None,
                    };
                    tool.run(call_input.clone(), ctx, cancel.clone()).await
                }
                None => ToolOutput {
                    text: format!("Tool '{}' not found", call_name),
                    is_error: true,
                    details: None,
                },
            };
            results.push(ContentBlock::tool_result_text(
                call_id,
                &output.text,
                output.is_error,
                output.details,
            ));
        }

        let tool_msg = store.write_message(&session_id, Role::User, results, None)?;
        message_ids.push(tool_msg.id);
    }

    store.checkpoint(&session_id, &message_ids, None)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// runTurn
// ---------------------------------------------------------------------------

/// A single LLM call: send a context, get a response, persist it.
///
/// Unlike `runAgent`, this makes exactly one LLM call with no function-calling
/// tools. The model's native capabilities are enabled automatically.
struct RunTurnTool {
    sessions_dir: PathBuf,
}

#[async_trait]
impl Tool for RunTurnTool {
    fn name(&self) -> &str {
        "runTurn"
    }

    fn description(&self) -> &str {
        "Invoke an LLM for a single response (no tool calls, no agent loop). \
         The model's native capabilities are enabled automatically -- Gemini \
         models get search grounding and code execution. Writes the response \
         to a session asynchronously. Use this for research queries, getting \
         a second opinion, or any case where you want a direct model response \
         without agentic tool use."
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
                    "description": "A list of message ids making up the prompt history. Used when context_id is not provided."
                },
                "user_prompt": {
                    "type": "string",
                    "description": "Text to append as a user message before the LLM call."
                },
                "session_id": {
                    "type": "string",
                    "description": "The session to write the response to. Created if it doesn't exist."
                },
                "model_id": {
                    "type": "string",
                    "description": format!("The model to call. Available: {}", models)
                },
                "model_params": {
                    "type": "object",
                    "description": "Parameters for the model call.",
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
        let message_ids: Vec<MessageId> = if let Some(cid) = input.get("context_id").and_then(|v| v.as_str()) {
            let mut store = Store::new(self.sessions_dir.clone());
            if let Err(e) = store.load_all() {
                return err(&format!("failed to load store: {}", e));
            }
            match store.pool.get_context(cid) {
                Some(ctx) => ctx.messages.clone(),
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

        let settings = ri_tools::resources::load_settings();
        let params = input.get("model_params");
        let thinking = params
            .and_then(|p| p.get("thinking"))
            .and_then(|v| v.as_str())
            .and_then(parse_thinking)
            .or_else(|| {
                settings
                    .default_thinking
                    .as_deref()
                    .and_then(parse_thinking)
            })
            .unwrap_or(ThinkingLevel::Medium);
        let max_tokens = params
            .and_then(|p| p.get("max_tokens"))
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .map(|n| n as usize);

        let (provider, model) = match ri_ai::registry::resolve(&model_id).await {
            Ok(r) => r,
            Err(e) => return err(&format!("model resolution failed: {}", e)),
        };

        let sessions_dir = self.sessions_dir.clone();
        let mut store = Store::new(sessions_dir);
        if let Err(e) = store.load_all() {
            return err(&format!("failed to load sessions: {}", e));
        }

        let name = session_id.unwrap_or_else(|| ri::gen_id());
        let cwd_str = ctx.cwd.to_string_lossy().to_string();
        let parent = ctx.session_id.as_ref();
        let mut msg_ids = message_ids;
        let file_id = match store.create_session(&name, &cwd_str, parent) {
            Ok(v) => v,
            Err(e) => return err(&format!("failed to create session: {}", e)),
        };

        // Optionally write user prompt.
        if let Some(text) = &user_prompt {
            match store.write_message(
                &file_id,
                Role::User,
                vec![ContentBlock::text(text)],
                None,
            ) {
                Ok(msg) => msg_ids.push(msg.id),
                Err(e) => return err(&format!("failed to write user message: {}", e)),
            }
        }

        // Spawn background single-turn task.
        let session_id_clone = file_id.clone();
        tokio::spawn(async move {
            if let Err(e) = run_background_turn(
                provider,
                model,
                store,
                msg_ids,
                thinking,
                max_tokens,
                session_id_clone,
            )
            .await
            {
                tracing::error!("background turn failed: {}", e);
            }
        });

        ToolOutput {
            text: format!("Single turn started on session '{}'", file_id),
            is_error: false,
            details: Some(json!({ "session_id": file_id })),
        }
    }
}

/// Single LLM call that owns all its data. Runs in a background task.
async fn run_background_turn(
    provider: Box<dyn LlmProvider>,
    model: Model,
    mut store: Store,
    message_ids: Vec<MessageId>,
    thinking: ThinkingLevel,
    max_tokens: Option<usize>,
    session_id: SessionId,
) -> eyre::Result<()> {
    let system_prompt = store
        .pool
        .resolve(&message_ids)
        .iter()
        .find(|m| m.role == Role::System)
        .and_then(|m| {
            m.content.iter().find_map(|b| {
                if let ContentBlock::Text { text, .. } = b {
                    Some(text.clone())
                } else {
                    None
                }
            })
        })
        .unwrap_or_default();

    let messages: Vec<Message> = store
        .pool
        .resolve(&message_ids)
        .into_iter()
        .cloned()
        .collect();

    let opts = RequestOptions {
        model: model.clone(),
        system_prompt,
        messages,
        tools: Vec::new(),
        thinking,
        max_tokens,
        native_tools: true,
    };

    let mut turn = match Turn::start(provider.as_ref(), opts).await {
        Ok(t) => t,
        Err(e) => {
            let msg = store.write_message(
                &session_id,
                Role::Assistant,
                vec![ContentBlock::error(e.to_string())],
                Some(serde_json::json!({
                    "model": model.id,
                    "ts": chrono::Utc::now().to_rfc3339(),
                })),
            )?;
            let mut ids = message_ids;
            ids.push(msg.id);
            store.checkpoint(&session_id, &ids, None)?;
            return Ok(());
        }
    };

    while let Some(result) = turn.next().await {
        if let Err(_) = result { break; }
    }
    let (content, usage) = turn.finish();

    let assistant_msg = store.write_message(
        &session_id,
        Role::Assistant,
        content,
        Some(serde_json::json!({
            "model": model.id,
            "ts": chrono::Utc::now().to_rfc3339(),
            "usage": usage,
            "turn": true,
        })),
    )?;
    let mut ids = message_ids;
    ids.push(assistant_msg.id);
    store.checkpoint(&session_id, &ids, None)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// readContextGraph
// ---------------------------------------------------------------------------

/// DAG neighborhood explorer for the CLI. Creates a fresh Store each call.
struct ReadContextGraphTool {
    sessions_dir: PathBuf,
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
        let depth = input.get("depth")
            .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
            .map(|n| n as usize)
            .unwrap_or(GRAPH_DEPTH);

        let mut store = Store::new(self.sessions_dir.clone());
        if let Err(e) = store.load_all() {
            return err(&format!("failed to load store: {}", e));
        }

        // Resolve entry point.
        let context_id_str = input.get("context_id").and_then(|v| v.as_str());
        let session_id_str = input.get("session_id").and_then(|v| v.as_str());

        let entry_id = if let Some(cid) = context_id_str {
            cid.to_string()
        } else if let Some(sid) = session_id_str {
            match store.get_session(sid).map(|s| s.head.to_string()) {
                Some(head) => head,
                None => return err(&format!("session '{}' not found", sid)),
            }
        } else {
            return err("either 'session_id' or 'context_id' is required");
        };

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
                    format_message_list(&mut out, pool, &ctx.messages);
                }
            } else {
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

// ---------------------------------------------------------------------------
// readMessage
// ---------------------------------------------------------------------------

struct ReadMessageTool {
    sessions_dir: PathBuf,
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
        let message_id = match input.get("message_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return err("missing 'message_id' parameter"),
        };

        match find_message_on_disk(message_id, &self.sessions_dir) {
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
struct AppendMessageTool {
    sessions_dir: PathBuf,
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
            Some(id) => id.clone(),
            None => return err("no calling session -- appendMessage requires a session context"),
        };

        let mut store = Store::new(self.sessions_dir.clone());
        if let Err(e) = store.load_all() {
            return err(&format!("failed to load store: {}", e));
        }

        // Resolve the parent context's messages (if any).
        let parent_messages: Vec<MessageId> = if let Some(cid) = parent_context_id {
            match store.pool.get_context(cid) {
                Some(ctx) => ctx.messages.clone(),
                None => return err(&format!("context '{}' not found", cid)),
            }
        } else {
            Vec::new()
        };

        // Write the new message.
        let msg = match store.write_message(
            &session_id,
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

        let new_ctx = match store.write_context(
            &session_id,
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
struct CreateContextTool {
    sessions_dir: PathBuf,
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
            Some(id) => id.clone(),
            None => return err("no calling session -- createContext requires a session context"),
        };

        let mut store = Store::new(self.sessions_dir.clone());
        if let Err(e) = store.load_all() {
            return err(&format!("failed to load store: {}", e));
        }

        let context = match store.write_context(
            &session_id,
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

fn find_message_on_disk(message_id: &str, sessions_dir: &std::path::Path) -> Option<Message> {
    let mut store = Store::new(sessions_dir.to_path_buf());
    store.load_all().ok()?;
    store.pool.get_message(message_id).cloned()
}
