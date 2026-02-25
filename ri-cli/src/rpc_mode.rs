use crate::agent::{self, AgentEvent};
use crate::print_mode;
use ri::{LlmProvider, Model, SessionStore, ThinkingLevel, Tool};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::Write;
use std::path::PathBuf;

use futures::StreamExt;
use tokio::io::AsyncBufReadExt;

#[derive(Debug, Deserialize)]
struct RpcCommand {
    #[serde(rename = "type")]
    command_type: String,
    #[serde(flatten)]
    data: Value,
}

fn output_json(value: &Value) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = serde_json::to_writer(&mut out, value);
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

fn handle_event(evt: &AgentEvent) {
    output_json(&print_mode::event_to_json(evt));
}

pub async fn run(
    provider: Box<dyn LlmProvider>,
    model: Model,
    tools: Vec<Box<dyn Tool>>,
    cwd: PathBuf,
    initial_prompt: Option<String>,
    thinking: ThinkingLevel,
) {
    let mut seen_agents = std::collections::HashSet::new();
    let system_prompt = {
        let context_files = ri_tools::resources::discover_context_files(&cwd);
        ri_tools::resources::build_system_prompt(&context_files)
    };
    let cwd_str = cwd.to_string_lossy().to_string();
    let sessions_dir = match SessionStore::default_dir() {
        Ok(d) => d,
        Err(e) => {
            output_json(
                &json!({"type": "error", "message": format!("Failed to find sessions dir: {}", e)}),
            );
            return;
        }
    };
    let mut store = SessionStore::new(sessions_dir);
    if let Err(e) = store.load_all() {
        output_json(
            &json!({"type": "error", "message": format!("Failed to load sessions: {}", e)}),
        );
        return;
    }
    let file_id = match store.create_session("rpc", &cwd_str, None, &[]) {
        Ok(id) => id,
        Err(e) => {
            output_json(
                &json!({"type": "error", "message": format!("Failed to create session: {}", e)}),
            );
            return;
        }
    };
    let sys_msg = match store.write_message(
        &file_id,
        ri::Role::System,
        vec![ri::ContentBlock::text(&system_prompt)],
        None,
        None,
    ) {
        Ok(m) => m,
        Err(e) => {
            output_json(
                &json!({"type": "error", "message": format!("Failed to write system message: {}", e)}),
            );
            return;
        }
    };
    let mut message_ids = vec![sys_msg.id];

    if let Some(prompt) = initial_prompt {
        let cancel = tokio_util::sync::CancellationToken::new();
        let events = match agent::submit(
            &prompt,
            provider.as_ref(),
            &model,
            &tools,
            &mut store,
            &mut message_ids,
            &cwd,
            thinking,
            &file_id,
            &mut seen_agents,
            cancel,
        ) {
            Ok(s) => s,
            Err(e) => {
                output_json(
                    &json!({"type": "error", "message": format!("Failed to submit: {}", e)}),
                );
                return;
            }
        };
        tokio::pin!(events);
        while let Some(evt) = events.next().await {
            handle_event(&evt);
        }
    }

    let stdin = tokio::io::stdin();
    let reader = tokio::io::BufReader::new(stdin);
    let mut lines = reader.lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let text = line.trim().to_string();
        if text.is_empty() {
            continue;
        }

        let cmd: RpcCommand = match serde_json::from_str(&text) {
            Ok(c) => c,
            Err(e) => {
                output_json(&json!({"type": "error", "message": format!("Invalid JSON: {}", e)}));
                continue;
            }
        };

        match cmd.command_type.as_str() {
            "prompt" | "follow_up" => {
                let message = cmd
                    .data
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if message.is_empty() {
                    output_json(&json!({"type": "error", "message": "Missing 'message'"}));
                    continue;
                }

                let cancel = tokio_util::sync::CancellationToken::new();
                let events = match agent::submit(
                    message,
                    provider.as_ref(),
                    &model,
                    &tools,
                    &mut store,
                    &mut message_ids,
                    &cwd,
                    thinking,
                    &file_id,
                    &mut seen_agents,
                    cancel,
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        output_json(
                            &json!({"type": "error", "message": format!("Failed to submit: {}", e)}),
                        );
                        continue;
                    }
                };
                tokio::pin!(events);
                while let Some(evt) = events.next().await {
                    handle_event(&evt);
                }

                output_json(&json!({"type": "response", "command": "prompt", "success": true}));
            }

            "abort" => {
                output_json(&json!({"type": "response", "command": "abort", "success": true}));
                break;
            }

            other => {
                output_json(
                    &json!({"type": "error", "message": format!("Unknown command: {}", other)}),
                );
            }
        }
    }
}
