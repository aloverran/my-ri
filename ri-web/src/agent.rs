//! Agent loop for ri-web: call LLM, execute tools, persist messages, repeat.
//!
//! Unlike ri-cli's agent loop (which returns a Stream), this one broadcasts
//! events through a tokio::sync::broadcast channel. Multiple SSE clients
//! can observe the same run simultaneously.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use ri::{
    ContentBlock, LlmProvider, Message, Model, Provenance,
    RequestOptions, Role, StreamEvent, ThinkingLevel,
    Tool, ToolOutput, ToolSchema,
};
use ri_ai::Turn;

use crate::state::SessionState;

/// Events broadcast to SSE clients during an agent run.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    Stream(StreamEvent),
    ToolStart { id: String, name: String },
    ToolEnd { id: String, output: String, is_error: bool, details: Option<serde_json::Value> },
    MessageComplete(Message),
    Error(String),
    Done,
}

/// Spawn the agent loop as a tokio task. Returns the JoinHandle.
///
/// The loop writes user message, runs LLM turns, executes tools, and
/// broadcasts all events through the session's broadcast channel.
/// When finished, it clears `current_run` in the SessionState.
pub fn spawn_agent_loop(
    session: Arc<Mutex<SessionState>>,
    user_text: String,
    provider: Arc<dyn LlmProvider>,
    model: Model,
    tools: Vec<Arc<dyn Tool>>,
    thinking: ThinkingLevel,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let result = run_agent_loop(
            &session, &user_text, provider.as_ref(), &model, &tools, thinking, &cancel,
        ).await;

        if let Err(e) = result {
            // Best effort: broadcast the error.
            let lock = session.lock().await;
            let _ = lock.events_tx.send(AgentEvent::Error(e.to_string()));
        }

        // Always clear current_run when the task exits.
        let mut lock = session.lock().await;
        lock.current_run = None;
    })
}

fn thinking_to_str(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off => "off",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::XHigh => "xhigh",
    }
}

async fn run_agent_loop(
    session: &Arc<Mutex<SessionState>>,
    user_text: &str,
    provider: &dyn LlmProvider,
    model: &Model,
    tools: &[Arc<dyn Tool>],
    thinking: ThinkingLevel,
    cancel: &CancellationToken,
) -> eyre::Result<()> {
    // Read cwd and tx before any lock-heavy work.
    let (tx, cwd) = {
        let lock = session.lock().await;
        (lock.events_tx.clone(), lock.cwd.clone())
    };

    // Build system prompt outside any lock (does blocking file I/O).
    let system_prompt = build_system_prompt(&cwd);

    let thinking_str = thinking_to_str(thinking).to_string();

    // Write user message under brief lock, then release.
    {
        let mut lock = session.lock().await;
        let user_id = lock.store.next_id();
        let user_msg = Message::new(user_id.clone(), Role::User, vec![ContentBlock::text(user_text)]);
        lock.store.write_message(user_msg.clone())?;
        lock.message_ids.push(user_id);
        let _ = tx.send(AgentEvent::MessageComplete(user_msg));
    }

    let tool_schemas: Vec<ToolSchema> = tools.iter().map(|t| t.schema()).collect();
    let tool_map: HashMap<&str, &dyn Tool> = tools.iter()
        .map(|t| (t.name(), t.as_ref() as &dyn Tool))
        .collect();

    loop {
        if cancel.is_cancelled() { break; }

        // Resolve messages from the pool -- brief lock.
        let (input_ids, messages) = {
            let lock = session.lock().await;
            let ids = lock.message_ids.clone();
            let msgs: Vec<Message> = lock.store.pool.resolve_existing(&ids)
                .into_iter()
                .cloned()
                .collect();
            (ids, msgs)
        };

        let opts = RequestOptions {
            model: model.clone(),
            system_prompt: system_prompt.clone(),
            messages,
            tools: tool_schemas.clone(),
            thinking,
            max_tokens: None,
        };

        // Start the LLM turn.
        let mut turn = match Turn::start(provider, opts).await {
            Ok(t) => t,
            Err(e) => {
                let msg_text = e.to_string();
                let _ = tx.send(AgentEvent::Error(msg_text.clone()));
                
                // Build and persist assistant message with error content block.
                let assistant_msg = {
                    let mut lock = session.lock().await;
                    let assistant_id = lock.store.next_id();
                    let msg = Message {
                        id: assistant_id.clone(),
                        role: Role::Assistant,
                        content: vec![ContentBlock::error(msg_text)],
                        provenance: Some(Provenance {
                            input: input_ids,
                            model: model.id.clone(),
                            ts: chrono::Utc::now().to_rfc3339(),
                            usage: None,
                        }),
                        meta: Some(serde_json::json!({ "thinking": thinking_str })),
                    };
                    lock.store.write_message(msg.clone())?;
                    lock.message_ids.push(assistant_id);
                    msg
                };
                let _ = tx.send(AgentEvent::MessageComplete(assistant_msg));
                break;
            }
        };

        // Stream events.
        let mut turn_error = None;
        while let Some(result) = turn.next().await {
            if cancel.is_cancelled() { break; }
            match result {
                Ok(evt) => { let _ = tx.send(AgentEvent::Stream(evt)); }
                Err(e) => {
                    let msg_text = e.to_string();
                    let _ = tx.send(AgentEvent::Error(msg_text.clone()));
                    turn_error = Some(msg_text);
                    break;
                }
            }
        }

        let (mut content, usage) = turn.finish();
        if let Some(err) = turn_error {
            content.push(ContentBlock::error(err));
        }

        // Build and persist assistant message -- brief lock.
        let assistant_msg = {
            let mut lock = session.lock().await;
            let assistant_id = lock.store.next_id();
            let msg = Message {
                id: assistant_id.clone(),
                role: Role::Assistant,
                content: content.clone(),
                provenance: Some(Provenance {
                    input: input_ids,
                    model: model.id.clone(),
                    ts: chrono::Utc::now().to_rfc3339(),
                    usage,
                }),
                meta: Some(serde_json::json!({ "thinking": thinking_str })),
            };
            lock.store.write_message(msg.clone())?;
            lock.message_ids.push(assistant_id);
            msg
        };
        let _ = tx.send(AgentEvent::MessageComplete(assistant_msg.clone()));

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
        let cwd = session.lock().await.cwd.clone();
        let mut results: Vec<ContentBlock> = Vec::new();
        for (call_id, call_name, call_input) in &calls {
            if cancel.is_cancelled() {
                results.push(ContentBlock::tool_result_with_details(call_id, "Cancelled", true, None));
                continue;
            }

            let _ = tx.send(AgentEvent::ToolStart { id: call_id.clone(), name: call_name.clone() });

            let output = match tool_map.get(call_name.as_str()) {
                Some(tool) => {
                    tool.run(call_input.clone(), cwd.clone(), cancel.clone()).await
                }
                None => ToolOutput {
                    text: format!("Tool '{}' not found", call_name),
                    is_error: true,
                    details: None,
                },
            };

            let _ = tx.send(AgentEvent::ToolEnd {
                id: call_id.clone(),
                output: output.text.clone(),
                is_error: output.is_error,
                details: output.details.clone(),
            });

            results.push(ContentBlock::tool_result_with_details(call_id, &output.text, output.is_error, output.details));
        }

        // Persist tool results -- brief lock.
        let tool_msg = {
            let mut lock = session.lock().await;
            let tool_id = lock.store.next_id();
            let msg = Message::new(tool_id.clone(), Role::User, results);
            lock.store.write_message(msg.clone())?;
            lock.message_ids.push(tool_id);
            msg
        };
        let _ = tx.send(AgentEvent::MessageComplete(tool_msg));

        // Discover and inject AGENTS.md files near files accessed by tool calls.
        inject_discovered_agents(&calls, &cwd, session, &tx).await?;
    }

    let _ = tx.send(AgentEvent::Done);
    Ok(())
}

/// Build the system prompt for a session, discovering context files from
/// ~/.config/agents/ and project-local .agents/ directories.
pub fn build_system_prompt(cwd: &std::path::Path) -> String {
    let context_files = ri_tools::resources::discover_context_files(cwd);
    ri_tools::resources::build_system_prompt(&context_files)
}

/// Discover AGENTS.md files near files that tool calls accessed, and inject
/// any new ones as a user message. The set of already-injected files is derived
/// from `agents_context` meta tags in the current message history, so it stays
/// correct across compaction and session repointing.
async fn inject_discovered_agents(
    calls: &[(String, String, serde_json::Value)],
    cwd: &Path,
    session: &Arc<Mutex<SessionState>>,
    tx: &tokio::sync::broadcast::Sender<AgentEvent>,
) -> eyre::Result<()> {
    // Rebuild seen set from current history each time -- history can change
    // between loop iterations (compaction, repointing).
    let mut seen: HashSet<PathBuf> = {
        let lock = session.lock().await;
        lock.store.pool.resolve_existing(&lock.message_ids).iter()
            .filter_map(|m| m.meta.as_ref()?.get("agents_context")?.as_array())
            .flatten()
            .filter_map(|v| v.as_str().map(PathBuf::from))
            .collect()
    };

    let mut new_files = Vec::new();
    for (_, name, input) in calls {
        let path_str = match name.as_str() {
            "read" | "write" | "edit" | "Read" | "Write" | "Edit" => {
                match input.get("path").and_then(|v| v.as_str()) {
                    Some(p) => p,
                    None => continue,
                }
            }
            _ => continue,
        };
        let resolved = if Path::new(path_str).is_absolute() {
            PathBuf::from(path_str)
        } else {
            cwd.join(path_str)
        };
        let dir = match resolved.parent() {
            Some(d) => d,
            None => continue,
        };
        for cf in ri_tools::resources::find_context_files(dir) {
            let canonical = cf.path.canonicalize().unwrap_or_else(|_| cf.path.clone());
            if seen.insert(canonical) {
                new_files.push(cf);
            }
        }
    }

    if new_files.is_empty() { return Ok(()); }

    let mut text = String::from("# Context Files (discovered)\n");
    let mut paths = Vec::new();
    for cf in &new_files {
        text.push_str(&format!("\n## {}\n\n{}\n", cf.path.display(), cf.content));
        if let Ok(c) = cf.path.canonicalize() {
            if let Some(s) = c.to_str() { paths.push(s.to_string()); }
        }
    }

    let mut lock = session.lock().await;
    let id = lock.store.next_id();
    let msg = Message {
        id: id.clone(),
        role: Role::User,
        content: vec![ContentBlock::text(text)],
        provenance: None,
        meta: Some(serde_json::json!({ "agents_context": paths })),
    };
    lock.store.write_message(msg.clone())?;
    lock.message_ids.push(id);
    drop(lock);
    let _ = tx.send(AgentEvent::MessageComplete(msg));
    Ok(())
}
