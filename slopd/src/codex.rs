//! Codex rollout transcript normalization.
//!
//! Codex panes are deliberately standalone processes. slopd learns lifecycle
//! and transcript paths from Codex hooks, then tails each pane's rollout JSONL
//! just as it does Claude's transcript.

use serde_json::{Value, json};

pub type TranscriptRecord = (String, Value);

/// Convert one Codex rollout JSONL entry into slopd's public transcript shape.
///
/// `event_msg` user/agent messages duplicate `response_item` messages, so only
/// the latter are emitted. Metadata and lifecycle entries remain available to
/// the state backstop but are not exposed as transcript messages.
pub fn transcript_record(record: &Value) -> Option<TranscriptRecord> {
    if record.get("type").and_then(Value::as_str) != Some("response_item") {
        return None;
    }
    let payload = record.get("payload")?;
    match payload.get("type").and_then(Value::as_str)? {
        "message" => {
            let role = payload.get("role").and_then(Value::as_str)?;
            let event_type = match role {
                "user" => "userMessage",
                "assistant" => "agentMessage",
                // Never expose system/developer instructions as user transcript.
                _ => return None,
            };
            let text = content_text(payload.get("content"));
            if role == "user" && text.as_deref().is_some_and(is_internal_environment_context) {
                return None;
            }
            Some((
                event_type.to_string(),
                json!({
                    "role": role,
                    "text": text,
                    "content": payload.get("content").cloned().unwrap_or(Value::Null),
                }),
            ))
        }
        "reasoning" => Some((
            "reasoning".to_string(),
            json!({
                "text": content_text(payload.get("summary"))
                    .or_else(|| content_text(payload.get("content"))),
                "summary": payload.get("summary").cloned().unwrap_or(Value::Null),
                "content": payload.get("content").cloned().unwrap_or(Value::Null),
            }),
        )),
        "function_call" | "custom_tool_call" => {
            let name = payload
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            Some((
                tool_event_type(name).to_string(),
                json!({
                    "name": name,
                    "arguments": payload.get("arguments")
                        .or_else(|| payload.get("input"))
                        .cloned()
                        .unwrap_or(Value::Null),
                    "call_id": payload.get("call_id").cloned().unwrap_or(Value::Null),
                }),
            ))
        }
        "function_call_output" | "custom_tool_call_output" => Some((
            "toolResult".to_string(),
            json!({
                "call_id": payload.get("call_id").cloned().unwrap_or(Value::Null),
                "output": payload.get("output").cloned().unwrap_or(Value::Null),
            }),
        )),
        _ => None,
    }
}

fn content_text(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    let parts = value.as_array()?;
    let text = parts
        .iter()
        .filter_map(|part| {
            part.get("text")
                .and_then(Value::as_str)
                .or_else(|| part.get("input_text").and_then(Value::as_str))
                .or_else(|| part.get("output_text").and_then(Value::as_str))
        })
        .collect::<Vec<_>>()
        .join("");
    (!text.is_empty()).then_some(text)
}

fn is_internal_environment_context(text: &str) -> bool {
    let text = text.trim();
    text.starts_with("<environment_context>") && text.ends_with("</environment_context>")
}

fn tool_event_type(name: &str) -> &'static str {
    match name {
        "exec_command" | "write_stdin" => "commandExecution",
        "apply_patch" => "fileChange",
        "update_plan" => "plan",
        "web_search" | "web.run" => "webSearch",
        name if name.starts_with("mcp__") => "mcpToolCall",
        _ => "toolCall",
    }
}

/// State-machine backstop from raw rollout records. Hooks are authoritative;
/// this keeps restart recovery and a briefly missed hook correct.
pub fn transcript_state(record: &Value) -> Option<libslop::PaneDetailedState> {
    match (
        record.get("type").and_then(Value::as_str),
        record.pointer("/payload/type").and_then(Value::as_str),
    ) {
        (Some("event_msg"), Some("task_started")) => {
            Some(libslop::PaneDetailedState::BusyProcessing)
        }
        (Some("event_msg"), Some("task_complete" | "turn_aborted" | "error")) => {
            Some(libslop::PaneDetailedState::Ready)
        }
        (Some("response_item"), Some("function_call" | "custom_tool_call")) => {
            Some(libslop::PaneDetailedState::BusyToolUse)
        }
        (Some("response_item"), Some("function_call_output" | "custom_tool_call_output")) => {
            Some(libslop::PaneDetailedState::BusyProcessing)
        }
        _ => None,
    }
}

pub fn prompt_submitted(record: &Value) -> bool {
    record.get("type").and_then(Value::as_str) == Some("event_msg")
        && record.pointer("/payload/type").and_then(Value::as_str) == Some("task_started")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_messages_without_exposing_instructions() {
        let user = json!({
            "type": "response_item",
            "payload": {"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}
        });
        let (kind, payload) = transcript_record(&user).unwrap();
        assert_eq!(kind, "userMessage");
        assert_eq!(payload["text"], "hello");

        let developer = json!({
            "type": "response_item",
            "payload": {"type":"message","role":"developer","content":[{"type":"input_text","text":"secret"}]}
        });
        assert!(transcript_record(&developer).is_none());

        let environment = json!({
            "type": "response_item",
            "payload": {
                "type":"message",
                "role":"user",
                "content":[{
                    "type":"input_text",
                    "text":"<environment_context>\n  <cwd>/work</cwd>\n</environment_context>"
                }]
            }
        });
        assert!(transcript_record(&environment).is_none());
    }

    #[test]
    fn normalizes_tools_and_lifecycle() {
        let call = json!({
            "type":"response_item",
            "payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"pwd\"}","call_id":"1"}
        });
        assert_eq!(transcript_record(&call).unwrap().0, "commandExecution");
        assert_eq!(
            transcript_state(&call),
            Some(libslop::PaneDetailedState::BusyToolUse)
        );
        let started = json!({"type":"event_msg","payload":{"type":"task_started"}});
        assert!(prompt_submitted(&started));
        assert_eq!(
            transcript_state(&started),
            Some(libslop::PaneDetailedState::BusyProcessing)
        );
    }
}
