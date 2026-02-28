//! Meta-tools for orchestrating ri from within an agent loop.
//!
//! Three tools that let an LLM agent control ri itself:
//! - `runAgent`: spawn a sub-agent loop asynchronously
//! - `readSession`: read a session's message history
//! - `readMessage`: inspect a single message with provenance
//!
//! In the CLI, these tools own all the state they need (sessions_dir)
//! and create fresh Stores when reading. runAgent spawns a fully
//! self-contained background task.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use ri::{
    ContentBlock, LlmProvider, Message, MessageId, Model, RequestOptions, Role, SessionId, Store,
    ThinkingLevel, Tool, ToolContext, ToolOutput, ToolSchema,
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
        Box::new(ReadSessionTool {
            sessions_dir: sessions_dir.clone(),
        }),
        Box::new(ReadMessageTool { sessions_dir }),
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
        "Starts a single turn of an LLM agent, async, writing the resulting \
         assistant messages and tool call user messages back into the message \
         store, and updating the session to point at the final message. \
         Session can be a new name and the corresponding session will be \
         created. If not provided a random one will be created and returned."
    }

    fn parameters(&self) -> Value {
        let models = ri_ai::registry::available_model_ids().join(", ");
        json!({
            "type": "object",
            "properties": {
                "message_ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "A list of message ids making up the prompt history for this turn to start from."
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
            "required": ["message_ids", "model_id"]
        })
    }

    async fn run(&self, input: Value, ctx: ToolContext, _cancel: CancellationToken) -> ToolOutput {
        // -- Parse inputs --

        let message_ids: Vec<MessageId> = match input.get("message_ids").and_then(|v| v.as_array()) {
            Some(arr) => arr
                .iter()
                .filter_map(|v| v.as_str().map(MessageId::from))
                .collect(),
            None => return err("missing 'message_ids' parameter"),
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
                if let ContentBlock::Text { text } = b {
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
            results.push(ContentBlock::tool_result_with_details(
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
// readSession
// ---------------------------------------------------------------------------

struct ReadSessionTool {
    sessions_dir: PathBuf,
}

#[async_trait]
impl Tool for ReadSessionTool {
    fn name(&self) -> &str {
        "readSession"
    }

    fn description(&self) -> &str {
        "Returns the reflog of the given session, in reverse-chronological \
         order (first is the current session pointer). Each entry contains \
         the message_id. Use the optional parameters to control how much of \
         the session to read or limit content blocks to a certain number of bytes."
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

    async fn run(&self, input: Value, _ctx: ToolContext, _cancel: CancellationToken) -> ToolOutput {
        let session_id = match input.get("session_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return err("missing 'session_id' parameter"),
        };
        let limit = input
            .get("limit")
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .map(|n| n as usize);
        let content_limit = input
            .get("contentLimit")
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .map(|n| n as usize);

        let path = self.sessions_dir.join(format!("{}.jsonl", session_id));
        if !path.exists() {
            return err(&format!("session '{}' not found", session_id));
        }

        match read_session_messages(&path, &self.sessions_dir) {
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

/// Read all messages from a session file using the Store loader.
fn read_session_messages(
    path: &std::path::Path,
    sessions_dir: &std::path::Path,
) -> eyre::Result<Vec<Message>> {
    let mut store = Store::new(sessions_dir.to_path_buf());
    store.load_all()?;

    // Get the file_id from the path stem.
    let file_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    // Collect all messages from the session's head context, or fall back
    // to all messages in the pool if no head exists.
    if let Some(session) = store.get_session(file_id) {
        if let Some(step) = store.pool.get_step(session.head.as_str()) {
            return Ok(store.pool.resolve_context(&step.context)
                .into_iter()
                .cloned()
                .collect());
        }
    }

    // Fallback: return all messages from the file in order.
    // Parse the file manually for message lines.
    let file = std::fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut messages = Vec::new();
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() { continue; }
        if let Ok(obj) = serde_json::from_str::<Value>(trimmed) {
            if let Some(msg_id) = obj.get("msg").and_then(|v| v.as_str()) {
                if let Some(msg) = store.pool.get_message(msg_id) {
                    messages.push(msg.clone());
                }
            }
        }
    }
    Ok(messages)
}

fn format_session_output(
    mut messages: Vec<Message>,
    limit: Option<usize>,
    _content_limit: Option<usize>,
) -> ToolOutput {
    if let Some(n) = limit {
        messages.truncate(n);
    }

    let summaries: Vec<Value> = messages
        .iter()
        .map(|msg| {
            json!({
                "id": msg.id,
                "summary": msg.summarize(),
            })
        })
        .collect();

    let text = serde_json::to_string_pretty(&summaries).unwrap_or_default();
    ToolOutput {
        text,
        is_error: false,
        details: Some(Value::Array(summaries)),
    }
}

fn find_message_on_disk(message_id: &str, sessions_dir: &std::path::Path) -> Option<Message> {
    let mut store = Store::new(sessions_dir.to_path_buf());
    store.load_all().ok()?;
    store.pool.get_message(message_id).cloned()
}
