//! Agent loop for ri-web: call LLM, execute tools, persist messages, repeat.
//!
//! Unlike ri-cli's agent loop (which returns a Stream), this one broadcasts
//! events through a tokio::sync::broadcast channel. Multiple SSE clients
//! can observe the same run simultaneously.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use ri::{
    ContentBlock, JsonMap, LlmProvider, Message, Model, Provenance,
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
    ToolEnd { id: String, output: String, is_error: bool },
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
                let _ = tx.send(AgentEvent::Error(e.to_string()));
                break;
            }
        };

        // Stream events.
        while let Some(result) = turn.next().await {
            if cancel.is_cancelled() { break; }
            match result {
                Ok(evt) => { let _ = tx.send(AgentEvent::Stream(evt)); }
                Err(e) => {
                    let _ = tx.send(AgentEvent::Error(e.to_string()));
                    break;
                }
            }
        }

        let (content, usage) = turn.finish();

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
                meta: None,
                extra: JsonMap::new(),
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
                results.push(ContentBlock::tool_result_text(call_id, "Cancelled", true));
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
                },
            };

            let _ = tx.send(AgentEvent::ToolEnd {
                id: call_id.clone(),
                output: output.text.clone(),
                is_error: output.is_error,
            });

            results.push(ContentBlock::tool_result_text(call_id, &output.text, output.is_error));
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
