//! Agent loop: call LLM, execute tools, persist messages, repeat.
//!
//! This is application-level composition of ri primitives (Turn, Tool,
//! SessionStore). It returns a stream of AgentEvents so the caller can
//! drive display, logging, or RPC output with plain iteration.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use async_stream::stream;
use futures::Stream;

use ri::{
    ContentBlock, LlmProvider, Message, Model, Provenance,
    RequestOptions, Role, SessionStore, StreamEvent, ThinkingLevel,
    Tool, ToolOutput, ToolSchema,
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
    ToolEnd { id: String, output: String, is_error: bool, details: Option<serde_json::Value> },
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
    store: &'a mut SessionStore,
    message_ids: &'a mut Vec<String>,
    cwd: &'a Path,
    thinking: ThinkingLevel,
    seen_agents: &'a mut HashSet<PathBuf>,
    cancel: tokio_util::sync::CancellationToken,
) -> eyre::Result<impl Stream<Item = AgentEvent> + 'a> {
    let user_id = store.next_id();
    let user_msg = Message::new(user_id.clone(), Role::User, vec![ContentBlock::text(text)]);
    store.write_message(user_msg)?;
    message_ids.push(user_id);

    Ok(run(provider, model, tools, store, message_ids, cwd, thinking, None, seen_agents, cancel))
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
    store: &'a mut SessionStore,
    message_ids: &'a mut Vec<String>,
    cwd: &'a Path,
    thinking: ThinkingLevel,
    max_tokens: Option<usize>,
    seen_agents: &'a mut HashSet<PathBuf>,
    cancel: tokio_util::sync::CancellationToken,
) -> impl Stream<Item = AgentEvent> + 'a {
    stream! {
        let tool_schemas: Vec<ToolSchema> = tools.iter().map(|t| t.schema()).collect();
        let tool_map: HashMap<&str, &dyn Tool> = tools.iter()
            .map(|t| (t.name(), t.as_ref()))
            .collect();

        // Extract system prompt from the first System-role message.
        let system_prompt = extract_system_prompt(store, message_ids);

        loop {
            if cancel.is_cancelled() { break; }

            // Resolve messages from the pool for this turn.
            let input_ids: Vec<String> = message_ids.clone();
            let messages: Vec<Message> = store.pool.resolve_existing(&input_ids)
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
            };

            // Start the LLM turn.
            let mut turn = match Turn::start(provider, opts).await {
                Ok(t) => t,
                Err(e) => {
                    let msg_text = e.to_string();
                    yield AgentEvent::Error(msg_text.clone());

                    // Build and persist assistant message with error content block.
                    let assistant_id = store.next_id();
                    let assistant_msg = Message {
                        id: assistant_id.clone(),
                        role: Role::Assistant,
                        content: vec![ContentBlock::error(msg_text)],
                        provenance: Some(Provenance {
                            input: input_ids,
                            model: model.id.clone(),
                            ts: chrono::Utc::now().to_rfc3339(),
                            usage: None,
                        }),
                        meta: None,
                    };
                    let _ = store.write_message(assistant_msg.clone());
                    message_ids.push(assistant_id);
                    yield AgentEvent::MessageComplete(assistant_msg);
                    break;
                }
            };

            // Stream events to the caller and let Turn accumulate internally.
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

            // Build and persist the assistant message.
            let assistant_id = store.next_id();
            let assistant_msg = Message {
                id: assistant_id.clone(),
                role: Role::Assistant,
                content: content.clone(),
                provenance: Some(Provenance {
                    input: input_ids,
                    model: model.id.clone(),
                    ts: chrono::Utc::now().to_rfc3339(),
                    usage,
                }),
                meta: None,
            };
            if let Err(e) = store.write_message(assistant_msg.clone()) {
                yield AgentEvent::Error(e.to_string());
                break;
            }
            message_ids.push(assistant_id);
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
                    results.push(ContentBlock::tool_result_with_details(call_id, "Cancelled", true, None));
                    continue;
                }

                // Discover AGENTS.md files near the target path for file tools.
                inject_context_for_tool(call_name, call_input, cwd, seen_agents, &mut results);

                yield AgentEvent::ToolStart { id: call_id.clone(), name: call_name.clone() };

                let output = match tool_map.get(call_name.as_str()) {
                    Some(tool) => {
                        tool.run(call_input.clone(), cwd.to_path_buf(), cancel.clone()).await
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

                results.push(ContentBlock::tool_result_with_details(call_id, &output.text, output.is_error, output.details));
            }

            // Persist tool results.
            let tool_id = store.next_id();
            let tool_msg = Message::new(tool_id.clone(), Role::User, results);
            if let Err(e) = store.write_message(tool_msg.clone()) {
                yield AgentEvent::Error(e.to_string());
                break;
            }
            message_ids.push(tool_id);
            yield AgentEvent::MessageComplete(tool_msg);
        }
    }
}

/// Extract the system prompt text from the first System-role message.
/// Falls back to the base prompt if none is found.
fn extract_system_prompt(store: &SessionStore, message_ids: &[String]) -> String {
    store.pool.resolve_existing(message_ids).iter()
        .find(|m| m.role == Role::System)
        .and_then(|m| m.content.iter().find_map(|b| {
            if let ContentBlock::Text { text } = b { Some(text.clone()) } else { None }
        }))
        .unwrap_or_else(|| ri_tools::resources::BASE_SYSTEM_PROMPT.to_string())
}

/// For file-related tools (Read, Write, Edit), discover any AGENTS.md files
/// in the directory hierarchy above the target file that haven't been seen yet.
/// Injects them as Text content blocks before the tool result.
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
