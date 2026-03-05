//! Agent loop: call LLM, execute tools, persist messages, repeat.
//!
//! This is application-level composition of ri primitives (Turn, Tool,
//! Store). It returns a stream of AgentEvents so the caller can
//! drive display, logging, or RPC output with plain iteration.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use async_stream::stream;
use futures::Stream;

use ri::{
    ContentBlock, LlmProvider, Message, MessageId, Model, RequestOptions, Role, SessionId, Store,
    StreamEvent, ThinkingLevel, Tool, ToolContext, ToolOutput, ToolSchema,
};
use ri_ai::Turn;

/// Events yielded by the agent loop.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// A raw stream event from the LLM provider.
    Stream(StreamEvent),
    /// A tool is about to be executed.
    ToolStart { id: String, name: String },
    /// A tool has finished executing.
    ToolEnd {
        id: String,
        output: String,
        is_error: bool,
        details: Option<serde_json::Value>,
    },
    /// A message (assistant or tool-result) has been fully constructed and persisted.
    MessageComplete(Message),
    /// A non-fatal error occurred.
    Error(String),
}

/// Persist a user message and start the agent loop. This is the standard
/// entry point -- it creates the user message, writes it to the store,
/// then delegates to `run`.
pub fn submit<'a>(
    text: &str,
    provider: &'a dyn LlmProvider,
    model: &'a Model,
    tools: &'a [Box<dyn Tool>],
    store: &'a mut Store,
    message_ids: &'a mut Vec<MessageId>,
    cwd: &'a Path,
    thinking: ThinkingLevel,
    session_id: &'a SessionId,
    seen_agents: &'a mut HashSet<PathBuf>,
    cancel: tokio_util::sync::CancellationToken,
) -> eyre::Result<impl Stream<Item = AgentEvent> + 'a> {
    let user_msg = store.write_message(
        session_id,
        Role::User,
        vec![ContentBlock::text(text)],
        None,
    )?;
    message_ids.push(user_msg.id);

    Ok(run(
        provider,
        model,
        tools,
        store,
        message_ids,
        cwd,
        thinking,
        None,
        session_id,
        seen_agents,
        cancel,
    ))
}

/// Run the agent loop: stream LLM response, execute tool calls, persist
/// everything, repeat until the model stops issuing tool calls.
///
/// The system prompt is extracted from the first System-role message in
/// the message history. No special string threading needed.
///
/// Yields `AgentEvent`s for the caller to observe. Fatal errors stop the
/// stream after yielding an `AgentEvent::Error`.
pub fn run<'a>(
    provider: &'a dyn LlmProvider,
    model: &'a Model,
    tools: &'a [Box<dyn Tool>],
    store: &'a mut Store,
    message_ids: &'a mut Vec<MessageId>,
    cwd: &'a Path,
    thinking: ThinkingLevel,
    max_tokens: Option<usize>,
    session_id: &'a SessionId,
    seen_agents: &'a mut HashSet<PathBuf>,
    cancel: tokio_util::sync::CancellationToken,
) -> impl Stream<Item = AgentEvent> + 'a {
    stream! {
        let tool_schemas: Vec<ToolSchema> = tools.iter().map(|t| t.schema()).collect();
        let tool_map: HashMap<&str, &dyn Tool> = tools.iter()
            .map(|t| (t.name(), t.as_ref()))
            .collect();

        let system_prompt = extract_system_prompt(store, message_ids);

        loop {
            if cancel.is_cancelled() { break; }

            let input_ids: Vec<MessageId> = message_ids.clone();
            let messages: Vec<Message> = store.pool.resolve(&input_ids)
                .into_iter()
                .cloned()
                .collect();

            let opts = RequestOptions {
                model: model.clone(),
                system_prompt: system_prompt.to_string(),
                messages,
                tools: tool_schemas.clone(),
                thinking,
                max_tokens,
                native_tools: false,
            };

            let mut turn = match Turn::start(provider, opts).await {
                Ok(t) => t,
                Err(e) => {
                    let msg_text = e.to_string();
                    yield AgentEvent::Error(msg_text.clone());

                    let assistant_msg = store.write_message(session_id,
                        Role::Assistant,
                        vec![ContentBlock::error(msg_text)],
                        Some(serde_json::json!({
                            "model": model.id,
                            "ts": chrono::Utc::now().to_rfc3339(),
                        })),
                    );
                    match assistant_msg {
                        Ok(msg) => {
                            message_ids.push(msg.id.clone());
                            yield AgentEvent::MessageComplete(msg);
                        }
                        Err(e) => yield AgentEvent::Error(e.to_string()),
                    }
                    break;
                }
            };

            let mut turn_error = None;
            while let Some(result) = turn.next().await {
                if cancel.is_cancelled() { break; }
                match result {
                    Ok(evt) => yield AgentEvent::Stream(evt),
                    Err(e) => {
                        let msg_text = e.to_string();
                        yield AgentEvent::Error(msg_text.clone());
                        turn_error = Some(msg_text);
                        break;
                    }
                }
            }

            let (mut content, usage) = turn.finish();
            if let Some(err) = turn_error {
                content.push(ContentBlock::error(err));
            }

            // Persist the assistant message.
            let assistant_msg = match store.write_message(session_id,
                Role::Assistant,
                content.clone(),
                Some(serde_json::json!({
                    "model": model.id,
                    "ts": chrono::Utc::now().to_rfc3339(),
                    "usage": usage,
                })),
            ) {
                Ok(msg) => msg,
                Err(e) => {
                    yield AgentEvent::Error(e.to_string());
                    break;
                }
            };
            message_ids.push(assistant_msg.id.clone());
            yield AgentEvent::MessageComplete(assistant_msg);

            // Extract tool calls.
            let calls: Vec<(String, String, serde_json::Value)> = content.iter().filter_map(|c| {
                if let ContentBlock::ToolUse { id, name, input, .. } = c {
                    Some((id.clone(), name.clone(), input.clone()))
                } else {
                    None
                }
            }).collect();

            if calls.is_empty() { break; }

            // Execute tool calls.
            let mut results: Vec<ContentBlock> = Vec::new();
            for (call_id, call_name, call_input) in &calls {
                if cancel.is_cancelled() {
                    results.push(ContentBlock::tool_result_text(call_id, "Cancelled", true, None));
                    continue;
                }

                inject_context_for_tool(call_name, call_input, cwd, seen_agents, &mut results);

                yield AgentEvent::ToolStart { id: call_id.clone(), name: call_name.clone() };

                let output = match tool_map.get(call_name.as_str()) {
                    Some(tool) => {
                        let ctx = ToolContext {
                            cwd: cwd.to_path_buf(),
                            session_id: Some(session_id.clone()),
                        };
                        tool.run(call_input.clone(), ctx, cancel.clone()).await
                    }
                    None => ToolOutput {
                        text: format!("Tool '{}' not found", call_name),
                        is_error: true,
                        details: None,
                    },
                };

                yield AgentEvent::ToolEnd {
                    id: call_id.clone(),
                    output: output.text.clone(),
                    is_error: output.is_error,
                    details: output.details.clone(),
                };

                results.push(ContentBlock::tool_result_text(call_id, &output.text, output.is_error, output.details));
            }

            // Persist tool results.
            let tool_msg = match store.write_message(session_id,
                Role::User, results, None,
            ) {
                Ok(msg) => msg,
                Err(e) => {
                    yield AgentEvent::Error(e.to_string());
                    break;
                }
            };
            message_ids.push(tool_msg.id.clone());
            yield AgentEvent::MessageComplete(tool_msg);
        }

        // Persist the final context as a step so the session can be reloaded from disk.
        if let Err(e) = store.checkpoint(session_id, message_ids, None) {
            yield AgentEvent::Error(format!("failed to checkpoint session: {}", e));
        }
    }
}

/// Extract the system prompt text from the first System-role message.
fn extract_system_prompt(store: &Store, message_ids: &[MessageId]) -> String {
    store
        .pool
        .resolve(message_ids)
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
        .unwrap_or_else(|| ri_tools::resources::BASE_SYSTEM_PROMPT.to_string())
}

/// For file-related tools, discover any AGENTS.md files in the directory
/// hierarchy above the target file that haven't been seen yet.
fn inject_context_for_tool(
    tool_name: &str,
    input: &serde_json::Value,
    cwd: &Path,
    seen: &mut HashSet<PathBuf>,
    results: &mut Vec<ContentBlock>,
) {
    let path_str = match tool_name {
        "read" | "write" | "edit" | "Read" | "Write" | "Edit" => {
            match input.get("path").and_then(|v| v.as_str()) {
                Some(p) => p,
                None => return,
            }
        }
        _ => return,
    };

    let resolved = if Path::new(path_str).is_absolute() {
        PathBuf::from(path_str)
    } else {
        cwd.join(path_str)
    };
    let dir = match resolved.parent() {
        Some(d) => d,
        None => return,
    };

    let new_files = ri_tools::resources::find_context_files(dir);
    let mut injected = Vec::new();
    for cf in new_files {
        let canonical = cf.path.canonicalize().unwrap_or_else(|_| cf.path.clone());
        if seen.insert(canonical) {
            injected.push(cf);
        }
    }

    if !injected.is_empty() {
        let mut text = String::from("# Context Files (discovered)\n");
        for cf in &injected {
            text.push_str(&format!("\n## {}\n\n{}\n", cf.path.display(), cf.content));
        }
        results.push(ContentBlock::text(text));
    }
}
