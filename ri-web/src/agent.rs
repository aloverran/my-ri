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
    ContentBlock, LlmProvider, Message, MessageId, Model, RequestOptions, Role, StreamEvent,
    ThinkingLevel, Tool, ToolContext, ToolOutput, ToolSchema,
};
use ri_ai::Turn;

use crate::state::{GlobalEvent, SessionState};

/// Events broadcast to SSE clients during an agent run.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    Stream(StreamEvent),
    ToolStart {
        id: String,
        name: String,
    },
    ToolEnd {
        id: String,
        output: String,
        is_error: bool,
        details: Option<serde_json::Value>,
    },
    MessageComplete(Message),
    /// Auto-generated title update from background title generation.
    TitleUpdate(String),
    Error(String),
    Done,
}

/// Spawn the agent loop as a tokio task. Returns the JoinHandle.
///
/// The loop writes user message, runs LLM turns, executes tools, and
/// broadcasts all events through the session's broadcast channel.
/// When finished, it clears `current_run` in the SessionState and
/// emits a global SessionDone event for desktop notifications.
pub fn spawn_agent_loop(
    session: Arc<Mutex<SessionState>>,
    user_text: String,
    provider: Arc<dyn LlmProvider>,
    model: Model,
    tools: Vec<Arc<dyn Tool>>,
    thinking: ThinkingLevel,
    cancel: CancellationToken,
    global_tx: tokio::sync::broadcast::Sender<GlobalEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let result = run_agent_loop(
            &session,
            &user_text,
            provider.as_ref(),
            &model,
            &tools,
            thinking,
            &cancel,
        )
        .await;

        if let Err(e) = result {
            // Persist the error into the context so readers can see what happened,
            // then do a best-effort checkpoint to save all accumulated work.
            let mut lock = session.lock().await;
            let sid = lock.file_id.clone();
            if let Ok(msg) = lock.store.write_message(
                &sid,
                Role::Assistant,
                vec![ContentBlock::error(e.to_string())],
                None,
            ) {
                lock.message_ids.push(msg.id.clone());
            }
            let ids = lock.message_ids.clone();
            let _ = lock.store.checkpoint(&sid, &ids, None);
            let _ = lock.events_tx.send(AgentEvent::Error(e.to_string()));
        }

        // Always clear current_run when the task exits.
        // Also emit a global SessionDone so the frontend can fire a notification.
        let mut lock = session.lock().await;
        lock.current_run = None;

        let preview = last_assistant_preview(&lock.store.pool.resolve(&lock.message_ids));
        let _ = global_tx.send(GlobalEvent::SessionDone {
            session_id: lock.file_id.to_string(),
            name: lock.name.clone(),
            preview,
            parent: lock.parent.as_ref().map(|p| p.to_string()),
        });
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
    // Write user message under brief lock, then release.
    {
        let mut lock = session.lock().await;
        let sid = lock.file_id.clone();
        let user_msg = lock.store.write_message(
            &sid,
            Role::User,
            vec![ContentBlock::text(user_text)],
            None,
        )?;
        lock.message_ids.push(user_msg.id.clone());
        let _ = lock.events_tx.send(AgentEvent::MessageComplete(user_msg));
    }

    // Kick off title generation with the new user message context.
    spawn_title_generation(session.clone());

    run_loop(session, provider, model, tools, thinking, None, cancel).await
}

/// The core agent loop: resolve messages, call the LLM, execute tool calls,
/// persist everything, repeat until the model stops calling tools.
///
/// The system prompt is extracted from the first System-role message in
/// the session's history. This means the system prompt is just another
/// message in the store -- no special string threading needed.
pub(crate) async fn run_loop(
    session: &Arc<Mutex<SessionState>>,
    provider: &dyn LlmProvider,
    model: &Model,
    tools: &[Arc<dyn Tool>],
    thinking: ThinkingLevel,
    max_tokens: Option<usize>,
    cancel: &CancellationToken,
) -> eyre::Result<()> {
    let (tx, session_id) = {
        let lock = session.lock().await;
        (lock.events_tx.clone(), lock.file_id.clone())
    };
    let thinking_str = thinking_to_str(thinking).to_string();

    // Extract the system prompt from the session's messages once, before looping.
    let system_prompt = {
        let lock = session.lock().await;
        extract_system_prompt(&lock.store.pool.resolve(&lock.message_ids))
    };

    let tool_schemas: Vec<ToolSchema> = tools.iter().map(|t| t.schema()).collect();
    let tool_map: HashMap<&str, &dyn Tool> = tools
        .iter()
        .map(|t| (t.name(), t.as_ref() as &dyn Tool))
        .collect();

    loop {
        if cancel.is_cancelled() {
            break;
        }

        // Resolve messages from the pool -- brief lock.
        let messages = {
            let lock = session.lock().await;
            let msgs: Vec<Message> = lock
                .store
                .pool
                .resolve(&lock.message_ids)
                .into_iter()
                .cloned()
                .collect();
            msgs
        };

        let opts = RequestOptions {
            model: model.clone(),
            system_prompt: system_prompt.to_string(),
            messages,
            tools: tool_schemas.clone(),
            thinking,
            max_tokens,
            native_tools: false,
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
                    let msg = lock.store.write_message(
                        &session_id,
                        Role::Assistant,
                        vec![ContentBlock::error(msg_text)],
                        Some(serde_json::json!({
                            "model": model.id,
                            "ts": chrono::Utc::now().to_rfc3339(),
                            "thinking": thinking_str,
                        })),
                    )?;
                    lock.message_ids.push(msg.id.clone());
                    msg
                };
                let _ = tx.send(AgentEvent::MessageComplete(assistant_msg));
                break;
            }
        };

        // Stream events.
        let mut turn_error = None;
        while let Some(result) = turn.next().await {
            if cancel.is_cancelled() {
                break;
            }
            match result {
                Ok(evt) => {
                    let _ = tx.send(AgentEvent::Stream(evt));
                }
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
            let msg = lock.store.write_message(
                &session_id,
                Role::Assistant,
                content.clone(),
                Some(serde_json::json!({
                    "model": model.id,
                    "ts": chrono::Utc::now().to_rfc3339(),
                    "usage": usage,
                    "thinking": thinking_str,
                })),
            )?;
            lock.message_ids.push(msg.id.clone());
            msg
        };
        let _ = tx.send(AgentEvent::MessageComplete(assistant_msg.clone()));

        // Trigger title generation if this assistant message has text content
        // (not purely tool calls). Runs in background, won't block the loop.
        let has_text = content.iter().any(|b| matches!(b, ContentBlock::Text { .. }));
        if has_text {
            spawn_title_generation(session.clone());
        }

        // Extract tool calls.
        let calls: Vec<(String, String, serde_json::Value)> = content
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

        // Execute tool calls.
        let (cwd, session_id) = {
            let lock = session.lock().await;
            (lock.cwd.clone(), lock.file_id.clone())
        };
        let mut results: Vec<ContentBlock> = Vec::new();
        for (call_id, call_name, call_input) in &calls {
            if cancel.is_cancelled() {
                results.push(ContentBlock::tool_result_text(
                    call_id,
                    "Cancelled",
                    true,
                    None,
                ));
                continue;
            }

            let _ = tx.send(AgentEvent::ToolStart {
                id: call_id.clone(),
                name: call_name.clone(),
            });

            let output = match tool_map.get(call_name.as_str()) {
                Some(tool) => {
                    let ctx = ToolContext {
                        cwd: cwd.clone(),
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

            let _ = tx.send(AgentEvent::ToolEnd {
                id: call_id.clone(),
                output: output.text.clone(),
                is_error: output.is_error,
                details: output.details.clone(),
            });

            results.push(ContentBlock::tool_result_text(
                call_id,
                &output.text,
                output.is_error,
                output.details,
            ));
        }

        // Persist tool results -- brief lock.
        let tool_msg = {
            let mut lock = session.lock().await;
            let msg = lock
                .store
                .write_message(&session_id, Role::User, results, None)?;
            lock.message_ids.push(msg.id.clone());
            msg
        };
        let _ = tx.send(AgentEvent::MessageComplete(tool_msg));

        // Discover and inject AGENTS.md files near files accessed by tool calls.
        inject_discovered_agents(&calls, &cwd, session, &tx).await?;

        // Progressive checkpoint: persist context after each complete tool cycle
        // so observers (readContextGraph) see progress and crashes don't lose work.
        {
            let mut lock = session.lock().await;
            let sid = lock.file_id.clone();
            let ids = lock.message_ids.clone();
            lock.store.checkpoint(&sid, &ids, None)?;
        }
    }

    // Final checkpoint: covers the last iteration where the model stopped
    // calling tools (break at line 284) -- that assistant message isn't
    // captured by the in-loop checkpoint above.
    {
        let mut lock = session.lock().await;
        let sid = lock.file_id.clone();
        let ids = lock.message_ids.clone();
        lock.store.checkpoint(&sid, &ids, None)?;
    }

    let _ = tx.send(AgentEvent::Done);
    Ok(())
}

/// Extract the system prompt text from the first System-role message.
/// Falls back to the base prompt if none is found.
fn extract_system_prompt(messages: &[&Message]) -> String {
    messages
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
        .unwrap_or_else(|| ri_tools::resources::BASE_SYSTEM_PROMPT.to_string())
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
        lock.store
            .pool
            .resolve(&lock.message_ids)
            .iter()
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

    if new_files.is_empty() {
        return Ok(());
    }

    let mut text = String::from("# Context Files (discovered)\n");
    let mut paths = Vec::new();
    for cf in &new_files {
        text.push_str(&format!("\n## {}\n\n{}\n", cf.path.display(), cf.content));
        if let Ok(c) = cf.path.canonicalize() {
            if let Some(s) = c.to_str() {
                paths.push(s.to_string());
            }
        }
    }

    let mut lock = session.lock().await;
    let sid = lock.file_id.clone();
    let msg = lock.store.write_message(
        &sid,
        Role::User,
        vec![ContentBlock::text(text)],
        Some(serde_json::json!({ "agents_context": paths })),
    )?;
    lock.message_ids.push(msg.id.clone());
    drop(lock);
    let _ = tx.send(AgentEvent::MessageComplete(msg));
    Ok(())
}

// ---------------------------------------------------------------------------
// Background title generation
// ---------------------------------------------------------------------------

const TITLE_MODEL_ID: &str = "claude-haiku-4-5-20251001";

const TITLE_SYSTEM_PROMPT: &str = "\
You generate short titles for coding agent sessions. You are part of a tool \
called 'ri' -- a terminal-based coding agent that helps engineers with \
software tasks: fixing bugs, adding features, refactoring, exploring codebases.

Given a conversation between a user and the coding agent, produce a short \
title (3-7 words) that captures the specific topic or task. Titles should \
read like short descriptions of what is being worked on, in sentence form \
(e.g. 'Fix login redirect loop', 'Add dark mode toggle', 'Refactor auth middleware').

Rules:
- Output ONLY the title text, nothing else.
- If there is not yet enough information to generate a meaningful title \
  (e.g. the conversation just started, or the user's intent is unclear), \
  output exactly the word DEFER.
- Focus on the specific technical task, not generic descriptions.
- Do not use quotes, colons, or prefixes like 'Title:'.
- Use sentence case.";

/// Spawn a background task that generates a title for the session from its
/// current messages. The task resolves a cheap model, builds a condensed
/// view of the conversation, and calls the LLM. If it gets a non-DEFER
/// response, it updates the session name, persists the title, and
/// broadcasts a title_update SSE event.
///
/// Safe to call frequently: each call increments title_gen_seq so only
/// the latest task's result is applied.
fn spawn_title_generation(session: Arc<Mutex<SessionState>>) {
    tokio::spawn(async move {
        // Skip if the session already has a generated title.
        {
            let lock = session.lock().await;
            if lock.name != crate::state::DEFAULT_SESSION_NAME {
                return;
            }
        }
        if let Err(e) = generate_title(session).await {
            tracing::debug!("Title generation skipped: {}", e);
        }
    });
}

async fn generate_title(session: Arc<Mutex<SessionState>>) -> eyre::Result<()> {
    // Snapshot the messages under lock, but defer the seq increment until we
    // know there's actually content to title. This prevents an empty-context
    // call from bumping the counter and silently discarding a valid in-flight task.
    let (seq, title_messages, session_id, tx) = {
        let mut lock = session.lock().await;
        let messages = build_title_context(&lock.store.pool.resolve(&lock.message_ids));
        if messages.is_empty() {
            return Ok(());
        }
        lock.title_gen_seq += 1;
        let seq = lock.title_gen_seq;
        (seq, messages, lock.file_id.clone(), lock.events_tx.clone())
    };

    // Resolve the title model. If the provider isn't authenticated, bail quietly.
    let (provider, model) = ri_ai::registry::resolve(TITLE_MODEL_ID).await?;

    let opts = RequestOptions {
        model,
        system_prompt: TITLE_SYSTEM_PROMPT.to_string(),
        messages: title_messages,
        tools: Vec::new(),
        thinking: ThinkingLevel::Off,
        max_tokens: Some(80),
        native_tools: false,
    };

    // Run the LLM call and collect the response text.
    let mut turn = Turn::start(provider.as_ref(), opts).await?;
    while let Some(result) = turn.next().await {
        result?; // Consume events, propagate errors.
    }
    let (content, _usage) = turn.finish();

    let response_text: String = content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text.trim().to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");

    // Collapse whitespace (newlines, double spaces) into clean single-line title.
    let title: String = response_text.split_whitespace().collect::<Vec<_>>().join(" ");

    // DEFER or empty: nothing to do.
    if title.is_empty() || title.eq_ignore_ascii_case("DEFER") {
        tracing::debug!("Title generation deferred for session [{}]", session_id);
        return Ok(());
    }

    // Apply the title only if our sequence is still current (no newer task
    // has been spawned since we started).
    let mut lock = session.lock().await;
    if lock.title_gen_seq != seq {
        tracing::debug!("Title generation for [{}] superseded (seq {} vs {})",
            session_id, seq, lock.title_gen_seq);
        return Ok(());
    }

    lock.name = title.clone();
    lock.store.write_title(&session_id, &title)?;
    let _ = tx.send(AgentEvent::TitleUpdate(title.clone()));

    tracing::info!("Generated title [{}] for session [{}]", title, session_id);
    Ok(())
}

/// Build a single-message context for title generation. Extracts user and
/// assistant text content, formats it as a labelled transcript, and wraps
/// it in one user message. This avoids three problems with sending the
/// original multi-turn conversation: OAuth system-prompt injection causing
/// identity confusion, assistant-last messages triggering prefill, and
/// tool-message filtering creating consecutive same-role messages.
fn build_title_context(messages: &[&Message]) -> Vec<Message> {
    // Extract (role_label, text) pairs from messages with text content.
    let mut entries: Vec<(&str, String)> = Vec::new();
    for msg in messages {
        let label = match msg.role {
            Role::System => continue,
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        for block in &msg.content {
            if let ContentBlock::Text { text, .. } = block {
                let trimmed = text.trim();
                if trimmed.is_empty() { continue; }
                let truncated = if trimmed.len() > 600 {
                    let end = trimmed.floor_char_boundary(600);
                    format!("{}...", &trimmed[..end])
                } else {
                    trimmed.to_string()
                };
                entries.push((label, truncated));
            }
        }
    }

    if entries.is_empty() {
        return Vec::new();
    }

    // Keep the first entry (usually the user's initial question) plus
    // recent entries from the end, capped at ~4000 chars total.
    let max_chars = 4000;
    let mut selected = vec![&entries[0]];
    let mut total_chars = entries[0].1.len();

    if entries.len() > 1 {
        let mut tail: Vec<&(&str, String)> = Vec::new();
        for entry in entries[1..].iter().rev() {
            if total_chars + entry.1.len() > max_chars { break; }
            total_chars += entry.1.len();
            tail.push(entry);
        }
        tail.reverse();
        selected.extend(tail);
    }

    // Format as a labelled transcript inside a single user message.
    let transcript: String = selected.iter()
        .map(|(label, text)| format!("[{}]: {}", label, text))
        .collect::<Vec<_>>()
        .join("\n\n");

    vec![Message {
        id: MessageId::new("title_ctx"),
        role: Role::User,
        content: vec![ContentBlock::text(format!(
            "Generate a title for this conversation:\n\n{}", transcript
        ))],
        meta: None,
    }]
}

/// Extract a short text preview from the last assistant message in a context.
/// Returns None if the last assistant message has no text content (e.g. only tool calls).
fn last_assistant_preview(messages: &[&Message]) -> Option<String> {
    let msg = messages.iter().rev().find(|m| m.role == Role::Assistant)?;

    let text: String = msg
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");

    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Truncate to ~100 chars at a word boundary for a clean notification body.
    let max = 100;
    if trimmed.len() <= max {
        Some(trimmed.to_string())
    } else {
        let boundary = trimmed.floor_char_boundary(max);
        let end = trimmed[..boundary].rfind(' ').unwrap_or(boundary);
        Some(format!("{}...", &trimmed[..end]))
    }
}
