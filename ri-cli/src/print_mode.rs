use crate::agent::AgentEvent;
use ri::StreamEvent;
use std::io::Write;

pub fn on_event_text(evt: &AgentEvent) {
    match evt {
        AgentEvent::Stream(se) => match se {
            StreamEvent::TextDelta(d) => {
                let stdout = std::io::stdout();
                let mut out = stdout.lock();
                let _ = out.write_all(d.as_bytes());
                let _ = out.flush();
            }
            StreamEvent::ToolCallStart { name, .. } => {
                eprintln!("\n[tool: {}]", name);
            }
            StreamEvent::Error(msg) => {
                eprintln!("Error: {}", msg);
            }
            _ => {}
        },
        AgentEvent::ToolEnd {
            output, is_error, ..
        } => {
            if *is_error {
                eprintln!("[tool error] {}", output);
            }
        }
        _ => {}
    }
}

pub fn on_event_json(evt: &AgentEvent) {
    let json = event_to_json(evt);
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = serde_json::to_writer(&mut out, &json);
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

pub fn event_to_json(evt: &AgentEvent) -> serde_json::Value {
    use serde_json::json;
    match evt {
        AgentEvent::Error(msg) => json!({"type": "error", "message": msg}),
        AgentEvent::Stream(se) => match se {
            StreamEvent::TextStart => json!({"type": "text_start"}),
            StreamEvent::TextDelta(d) => json!({"type": "text_delta", "delta": d}),
            StreamEvent::TextEnd { .. } => json!({"type": "text_end"}),
            StreamEvent::ThinkingStart => json!({"type": "thinking_start"}),
            StreamEvent::ThinkingDelta(d) => json!({"type": "thinking_delta", "delta": d}),
            StreamEvent::ThinkingEnd { .. } => json!({"type": "thinking_end"}),
            StreamEvent::ToolCallStart { id, name } => {
                json!({"type": "toolcall_start", "id": id, "name": name})
            }
            StreamEvent::ToolCallDelta { id, json_fragment } => {
                json!({"type": "toolcall_delta", "id": id, "delta": json_fragment})
            }
            StreamEvent::ToolCallEnd { id, .. } => json!({"type": "toolcall_end", "id": id}),
            StreamEvent::Usage(u) => {
                json!({"type": "usage", "input_tokens": u.input_tokens, "output_tokens": u.output_tokens, "cache_read_tokens": u.cache_read_tokens, "cache_write_tokens": u.cache_write_tokens})
            }
            StreamEvent::Done => json!({"type": "done"}),
            StreamEvent::Error(msg) => json!({"type": "stream_error", "message": msg}),
        },
        AgentEvent::ToolStart { id, name } => json!({"type": "tool_start", "id": id, "name": name}),
        AgentEvent::ToolEnd {
            id,
            output,
            is_error,
            details,
        } => json!({
            "type": "tool_end", "id": id, "output": output, "is_error": is_error, "details": details
        }),
        AgentEvent::MessageComplete(msg) => json!({
            "type": "message_complete", "id": &msg.id, "role": msg.role
        }),
    }
}
