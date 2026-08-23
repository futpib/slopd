use rmcp::model::{Tool, ToolAnnotations};
use serde_json::{Value, json};

use crate::schema;

const SIMPLE_NAMES: &[&str] = &[
    "get_work_overview",
    "start_new_agent",
    "message_existing_agent",
    "get_agent_result",
];

pub fn simple_names() -> &'static [&'static str] {
    SIMPLE_NAMES
}

pub fn simple() -> Vec<Tool> {
    all()
        .into_iter()
        .filter(|tool| SIMPLE_NAMES.contains(&tool.name.as_ref()))
        .collect()
}

pub fn all() -> Vec<Tool> {
    let tools = vec![
        tool(
            "get_status",
            "Show slopd daemon uptime and state.",
            empty_schema(),
        ),
        tool(
            "get_work_overview",
            "See which agents exist, what they are doing, and where work left off. Use this to identify an existing agent from human context before calling message_existing_agent. Do not call it when the user explicitly asks for a new agent. Call with no arguments for every live pane or pane_id for one pane. If more context is needed, call this same tool again with larger context_before or context_after. The backend field is the agent type; title is only a label.",
            overview_schema(),
        ),
        tool(
            "start_new_agent",
            "Start exactly one new independent agent. Use only when the user explicitly asks to start, create, or add a new, separate, or additional independent agent. Never use for a correction, clarification, update, redirect, continuation, retry, progress question, or result question about prior work: use message_existing_agent or get_agent_result. Referring to the same, first, second, prior, or additional agent means an existing agent, not a new one. A changed or repeated wording does not mean the user wants another agent. Call this tool before saying work started. This tool waits three seconds by default; after it returns pending or completed, obey follow_up_instruction, speak the answer, and call no more tools in that turn. This tool never accepts or reuses a pane_id. Omit backend for the usual or default agent. Translate the user's task to English unless the user explicitly requests another language; preserve the agent reply's language.",
            json!({
                "type": "object",
                "properties": {
                    "backend": backend_schema(),
                    "account": { "type": "string" },
                    "prompt": { "type": "string", "description": "Translate the user's task to English for this prompt unless the user explicitly requests another language." },
                    "wait_seconds": { "type": "integer", "minimum": 0, "maximum": 300, "default": 3, "description": "Seconds to wait before returning a pending background mailbox request." }
                },
                "required": ["prompt"],
                "additionalProperties": false
            }),
        ),
        tool(
            "message_existing_agent",
            "Send one task or follow-up to an existing agent. When the user says to tell, ask, or have the same, first, second, prior, or additional agent do something, call this tool before saying the work started. Also use it for corrections, clarifications, updates, redirects, continuations, and retries concerning prior work; never call start_new_agent for those. This tool never creates an agent. Use pane_id from the prior tool result or get_work_overview; omit it for the most recently contacted existing agent. It waits three seconds by default; after it returns pending or completed, obey follow_up_instruction, speak the answer, and call no more tools in that turn. Omit backend unless the user explicitly names an agent type. Translate the user's task to English unless the user explicitly requests another language; preserve the agent reply's language.",
            json!({
                "type": "object",
                "properties": {
                    "pane_id": pane_id_schema(),
                    "tag": { "type": "string" },
                    "backend": backend_schema(),
                    "account": { "type": "string" },
                    "prompt": { "type": "string", "description": "Translate the user's task to English for this prompt unless the user explicitly requests another language." },
                    "wait_seconds": { "type": "integer", "minimum": 0, "maximum": 300, "default": 3, "description": "Seconds to wait before returning a pending background mailbox request." },
                    "interrupt": { "type": "boolean", "default": false, "description": "True only when the user explicitly asks to interrupt or redirect a busy agent." }
                },
                "required": ["prompt"],
                "additionalProperties": false
            }),
        ),
        tool(
            "list_panes",
            "List live slopd panes and state counts without reading their conversations. Use get_work_overview when the user asks what agents are doing or where work left off. busy means actively working; ready means idle and available; awaiting_input means blocked on the user; booting_up means starting. Optional filters are AND-ed. The backend field is authoritative; title is only a label. Returns compact records unless raw is true.",
            filter_schema(),
        ),
        tool(
            "create_pane",
            "Low-level: create an idle agent pane without giving it work. Never use this before or instead of ask_or_tell_agent. For any request that asks or tells an agent to do something, call ask_or_tell_agent exactly once.",
            spawn_schema(false),
        ),
        tool(
            "fork_pane",
            "Fork a pane into an independent agent session. Reuse the returned pane_id exactly, including its leading %.",
            spawn_schema(true),
        ),
        tool(
            "kill_pane",
            "Terminate a managed agent pane.",
            pane_schema(),
        ),
        tool(
            "ask_or_tell_agent",
            "The one lower-level tool for asking or telling an agent to do work. Call it exactly once for requests including ask, tell it, tell the same agent, update it, correct it, change direction, continue, or try something else; never answer those from conversation memory. It selects or creates the agent, sends the prompt, waits three seconds by default, and records success or failure. Never call create_pane, send_prompt, wait_for_reply, or get_work_overview for the same request. Omit backend unless the user names an agent type. Set new_agent=true only when the user explicitly asks to start a new, separate, or additional independent agent; corrections, clarifications, updates, redirects, continuations, retries, and result questions are not new-agent requests. Repeated identical calls within one minute return the original request and never create duplicate panes. Treat status, agent_reply_received, answer, and follow_up_instruction as authoritative: pending continues in the background, so obey follow_up_instruction and stop calling tools for this turn; failed means report the failure and do not retry or claim acceptance. Write the prompt in English unless another language is explicitly requested, and preserve the agent reply's language.",
            json!({
                "type": "object",
                "properties": {
                    "pane_id": pane_id_schema(),
                    "tag": { "type": "string" },
                    "backend": backend_schema(),
                    "account": { "type": "string" },
                    "prompt": { "type": "string" },
                    "wait_seconds": { "type": "integer", "minimum": 0, "maximum": 300, "default": 3, "description": "Seconds to wait before returning a pending background mailbox request." },
                    "interrupt": { "type": "boolean", "default": false },
                    "new_agent": { "type": "boolean", "default": false, "description": "True only when the user explicitly asks for a new, separate, or additional agent." }
                },
                "required": ["prompt"],
                "additionalProperties": false
            }),
        ),
        tool(
            "get_agent_result",
            "Get the mailbox state or completed answer for prior agent work, including creation or startup failures, so an old unrelated result is never substituted. Always call this on every new user turn asking whether work is ready, what happened, current progress, when the result will arrive, or what the result was. Never answer those questions from conversation memory or elapsed time. Call with no arguments first unless a prior tool result supplied the exact request_id or pane_id. Never call start_new_agent or resend the prompt for a status, progress, completion, or result question. After this returns pending or completed, obey follow_up_instruction, speak the answer, and call no more tools in that turn. The top-level status, answer, and follow_up_instruction are authoritative.",
            json!({
                "type": "object",
                "properties": {
                    "request_id": { "type": "string" },
                    "pane_id": pane_id_schema(),
                    "wait_seconds": { "type": "integer", "minimum": 0, "maximum": 300, "default": 0 }
                },
                "additionalProperties": false
            }),
        ),
        tool(
            "send_prompt",
            "Low-level asynchronous send for explicit pane control. Use ask_or_tell_agent for ordinary agent work. This returns acceptance, not an agent reply; do not claim completion from it. Never combine it with ask_or_tell_agent for the same request.",
            json!({
                "type": "object",
                "properties": {
                    "pane_id": pane_id_schema(),
                    "tag": { "type": "string" },
                    "backend": backend_schema(),
                    "account": { "type": "string" },
                    "prompt": { "type": "string" },
                    "select": { "type": "string", "enum": ["one", "any", "all"], "default": "one" },
                    "interrupt": { "type": "boolean", "default": false },
                    "timeout": { "type": "integer", "minimum": 1, "maximum": 300, "default": 60 }
                },
                "required": ["prompt"],
                "additionalProperties": false
            }),
        ),
        tool(
            "wait_for_reply",
            "Wait for the agent's completed reply after send_prompt. Progress updates, reasoning, and tool calls are ignored.",
            json!({
                "type": "object",
                "properties": {
                    "pane_id": pane_id_schema(),
                    "timeout": { "type": "integer", "minimum": 1, "maximum": 300, "default": 120 }
                },
                "required": ["pane_id"],
                "additionalProperties": false
            }),
        ),
        tool(
            "interrupt_pane",
            "Send Ctrl+C, Ctrl+D, and Escape to interrupt a pane.",
            pane_schema(),
        ),
        tool(
            "read_transcript",
            "Read only useful conversation text from a pane. Set advanced=true for diagnostic records; internal context, progress, reasoning, tools, payloads, event names, and cursors are otherwise omitted.",
            transcript_schema(),
        ),
        tool("add_tag", "Add a tag to a pane.", tag_schema()),
        tool("remove_tag", "Remove a tag from a pane.", tag_schema()),
        tool("list_tags", "List all tags on a pane.", pane_schema()),
        tool(
            "create_backup",
            "Write a lifecycle-journal checkpoint now.",
            empty_schema(),
        ),
        tool(
            "restore_backup",
            "Restore missing panes from the pending or latest checkpoint.",
            empty_schema(),
        ),
        tool(
            "list_dead_panes",
            "List durable pane-death records, newest first. Returns compact records unless raw is true.",
            json!({
                "type": "object",
                "properties": {
                    "boot": { "type": "integer" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 500, "default": 50 },
                    "raw": { "type": "boolean", "default": false }
                },
                "additionalProperties": false
            }),
        ),
        tool(
            "revive_pane",
            "Resume a pane retained in the lifecycle graveyard.",
            json!({
                "type": "object",
                "properties": {
                    "target": { "type": "string" },
                    "boot": { "type": "integer" },
                    "env": string_array("Extra KEY=VALUE environment entries."),
                    "env_files": string_array("Server-local dotenv files, loaded in order.")
                },
                "additionalProperties": false
            }),
        ),
    ];
    tools
}

fn tool(name: &'static str, description: &'static str, input_schema: Value) -> Tool {
    let (title, read_only, destructive, idempotent) = metadata(name);
    Tool::new(name, description, schema(input_schema))
        .with_title(title)
        .with_raw_output_schema(schema(output_schema(name)))
        .with_annotations(
            ToolAnnotations::with_title(title)
                .read_only(read_only)
                .destructive(destructive)
                .idempotent(idempotent)
                .open_world(false),
        )
}

fn metadata(name: &str) -> (&'static str, bool, bool, bool) {
    match name {
        "get_status" => ("Get daemon status", true, false, true),
        "get_work_overview" => ("Get work overview", true, false, true),
        "start_new_agent" => ("Start new agent", false, false, false),
        "message_existing_agent" => ("Message existing agent", false, false, false),
        "list_panes" => ("List live panes", true, false, true),
        "create_pane" => ("Create pane", false, false, false),
        "fork_pane" => ("Fork pane", false, false, false),
        "kill_pane" => ("Kill pane", false, true, true),
        "ask_or_tell_agent" => ("Ask or tell agent", false, false, false),
        "get_agent_result" => ("Get agent result", true, false, true),
        "send_prompt" => ("Send prompt", false, false, false),
        "wait_for_reply" => ("Wait for reply", true, false, true),
        "interrupt_pane" => ("Interrupt pane", false, true, false),
        "collect_events" => ("Collect events", true, false, true),
        "wait_for_event" => ("Wait for event", true, false, true),
        "read_transcript" => ("Read transcript", true, false, true),
        "add_tag" => ("Add tag", false, false, true),
        "remove_tag" => ("Remove tag", false, true, true),
        "list_tags" => ("List tags", true, false, true),
        "create_backup" => ("Create backup", false, false, false),
        "restore_backup" => ("Restore backup", false, false, true),
        "list_dead_panes" => ("List dead panes", true, false, true),
        "revive_pane" => ("Revive pane", false, false, false),
        _ => unreachable!("missing MCP tool metadata for {name}"),
    }
}

fn output_schema(name: &str) -> Value {
    let properties = match name {
        "get_status" => json!({
            "uptime_secs": { "type": "integer" },
            "subscriber_count": { "type": "integer" },
            "config_generation": { "type": "integer" },
            "pending_restore": { "type": ["integer", "null"] }
        }),
        "get_work_overview" => json!({
            "count": { "type": "integer" },
            "state_counts": {
                "type": "object",
                "properties": {
                    "busy": { "type": "integer" },
                    "ready": { "type": "integer" },
                    "awaiting_input": { "type": "integer" },
                    "booting_up": { "type": "integer" }
                },
                "required": ["busy", "ready", "awaiting_input", "booting_up"],
                "additionalProperties": false
            },
            "panes": { "type": "array", "items": overview_pane_schema() },
            "answer": { "type": "string", "description": "Authoritative English status summary with agent excerpts preserved in their original language." }
        }),
        "list_panes" => json!({
            "count": { "type": "integer" },
            "state_counts": {
                "type": "object",
                "properties": {
                    "busy": { "type": "integer", "description": "Panes actively working." },
                    "ready": { "type": "integer", "description": "Idle panes available for a prompt." },
                    "awaiting_input": { "type": "integer", "description": "Panes blocked on user input." },
                    "booting_up": { "type": "integer", "description": "Panes still starting." }
                },
                "required": ["busy", "ready", "awaiting_input", "booting_up"],
                "additionalProperties": false
            },
            "panes": { "type": "array", "items": compact_pane_schema() }
        }),
        "create_pane" => json!({
            "pane_id": pane_id_schema(),
            "ready": { "type": "boolean" }
        }),
        "fork_pane" => json!({
            "pane_id": pane_id_schema(),
            "session_id": { "type": "string" },
            "ready": { "type": "boolean" }
        }),
        "kill_pane" | "interrupt_pane" => json!({ "pane_id": pane_id_schema() }),
        "start_new_agent" | "message_existing_agent" | "ask_or_tell_agent" => {
            let mut properties = mailbox_entry_schema()["properties"].clone();
            properties["request_reused"] = json!({ "type": "boolean", "description": "True when an identical recent call returned the original request instead of submitting duplicate work." });
            properties
        }
        "get_agent_result" => json!({
            "found": { "type": "boolean" },
            "request_id": { "type": ["string", "null"] },
            "pane_id": { "anyOf": [pane_id_schema(), { "type": "null" }] },
            "prompt": { "type": ["string", "null"] },
            "created_at_unix_ms": { "type": ["integer", "null"] },
            "status": { "type": "string", "enum": ["not_found", "pending", "completed", "failed"] },
            "finished": { "type": ["boolean", "null"], "description": "True means the request is no longer running. With status completed, answer yes when asked whether the agent finished." },
            "reply": { "type": ["string", "null"], "description": "Verbatim agent reply in its original language." },
            "error": { "type": ["string", "null"] },
            "answer": { "type": "string", "description": "Authoritative answer to give the user without translating it." },
            "agent_reply_received": { "type": "boolean", "description": "True only when reply contains a real completed agent reply." },
            "follow_up_instruction": { "type": "string", "description": "Required routing rule for a later user turn about this agent." }
        }),
        "send_prompt" => json!({
            "pane_ids": { "type": "array", "items": pane_id_schema() }
        }),
        "wait_for_reply" => json!({
            "pane_id": pane_id_schema(),
            "reply": { "type": "string", "description": "Verbatim agent reply in its original language; do not translate unless asked." }
        }),
        "collect_events" => json!({
            "records": { "type": "array", "items": { "type": "object" } },
            "timed_out": { "type": "boolean" }
        }),
        "wait_for_event" => json!({
            "record": { "type": "object" },
            "snapshot": { "type": "boolean" }
        }),
        "read_transcript" => json!({
            "count": { "type": "integer" },
            "records": { "type": "array", "items": { "type": "object" } }
        }),
        "add_tag" | "remove_tag" => json!({
            "pane_id": pane_id_schema(),
            "tag": { "type": "string" }
        }),
        "list_tags" => json!({
            "pane_id": pane_id_schema(),
            "tags": { "type": "array", "items": { "type": "string" } }
        }),
        "create_backup" => json!({ "count": { "type": "integer" } }),
        "restore_backup" => json!({ "restored": { "type": "integer" } }),
        "list_dead_panes" => json!({
            "count": { "type": "integer" },
            "entries": { "type": "array", "items": { "type": "object" } }
        }),
        "revive_pane" => json!({
            "pane_id": pane_id_schema(),
            "grave_id": { "type": "string" }
        }),
        _ => unreachable!("missing MCP output schema for {name}"),
    };
    json!({ "type": "object", "properties": properties, "additionalProperties": true })
}

fn mailbox_entry_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "request_id": { "type": "string" },
            "pane_id": { "anyOf": [pane_id_schema(), { "type": "null" }], "description": "The selected pane, or null if startup failed before one was allocated." },
            "prompt": { "type": "string" },
            "created_at_unix_ms": { "type": "integer" },
            "status": { "type": "string", "enum": ["pending", "completed", "failed"] },
            "finished": { "type": "boolean", "description": "True means the request is no longer running. With status completed, answer yes when asked whether the agent finished." },
            "reply": { "type": ["string", "null"], "description": "Verbatim agent reply in its original language." },
            "error": { "type": ["string", "null"] },
            "answer": { "type": "string", "description": "Authoritative answer to give the user without translating it." },
            "agent_reply_received": { "type": "boolean", "description": "True only when reply contains a real completed agent reply." },
            "follow_up_instruction": { "type": "string", "description": "Required routing rule for a later user turn about this agent." }
        },
        "required": ["request_id", "pane_id", "prompt", "created_at_unix_ms", "status", "finished", "reply", "error", "answer", "agent_reply_received", "follow_up_instruction"],
        "additionalProperties": false
    })
}

fn compact_pane_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pane_id": pane_id_schema(),
            "backend": backend_schema(),
            "account": { "type": "string" },
            "state": {
                "type": "string",
                "enum": ["busy", "ready", "awaiting_input", "booting_up"],
                "description": "busy is actively working; ready is idle; awaiting_input is blocked on the user; booting_up is starting."
            },
            "detailed_state": { "type": "string" },
            "tags": { "type": "array", "items": { "type": "string" } },
            "title": { "type": ["string", "null"], "description": "User-facing pane label, not the agent type. Use backend for the agent type." },
            "working_dir": { "type": "string" },
            "parent_pane_id": { "anyOf": [pane_id_schema(), { "type": "null" }] }
        },
        "required": ["pane_id", "backend", "account", "state", "detailed_state", "tags", "title", "working_dir", "parent_pane_id"],
        "additionalProperties": true
    })
}

fn overview_pane_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pane_id": pane_id_schema(),
            "backend": backend_schema(),
            "account": { "type": "string" },
            "state": { "type": "string", "enum": ["busy", "ready", "awaiting_input", "booting_up"] },
            "detailed_state": { "type": "string" },
            "tags": { "type": "array", "items": { "type": "string" } },
            "title": { "type": ["string", "null"], "description": "User-facing label, not the agent type." },
            "working_dir": { "type": ["string", "null"] },
            "last_request_excerpt": { "type": ["string", "null"], "description": "Verbatim excerpt in its original language." },
            "task_context_excerpt": { "type": ["string", "null"], "description": "Nearest preceding agent message, useful for resolving short references in the latest user prompt." },
            "current_activity_excerpt": { "type": ["string", "null"], "description": "Latest agent progress message since the latest user prompt." },
            "latest_tool_name": { "type": ["string", "null"], "description": "Latest tool name recorded since the latest user prompt; a fallback when no progress message is available." },
            "latest_reply_excerpt": { "type": ["string", "null"], "description": "Verbatim agent excerpt in its original language; do not translate unless asked." },
            "reply_complete": { "type": "boolean" },
            "context_before": { "type": "array", "items": overview_context_schema(), "description": "Requested messages immediately before the latest user prompt, oldest first." },
            "context_after": { "type": "array", "items": overview_context_schema(), "description": "Requested agent messages after the latest user prompt, oldest first. The newest requested messages are returned." },
            "more_before": { "type": "boolean", "description": "More meaningful messages exist before context_before. Increase context_before to retrieve them." },
            "more_after": { "type": "boolean", "description": "More meaningful messages exist after context_after. Increase context_after to retrieve them." },
            "transcript_error": { "type": ["string", "null"] }
        },
        "required": ["pane_id", "backend", "account", "state", "detailed_state", "tags", "title", "working_dir", "last_request_excerpt", "task_context_excerpt", "current_activity_excerpt", "latest_tool_name", "latest_reply_excerpt", "reply_complete", "context_before", "context_after", "more_before", "more_after", "transcript_error"],
        "additionalProperties": false
    })
}

fn overview_context_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "role": { "type": "string", "enum": ["user", "assistant"] },
            "kind": { "type": "string", "enum": ["request", "progress", "reply"] },
            "text": { "type": "string", "description": "Verbatim compact excerpt in its original language." }
        },
        "required": ["role", "kind", "text"],
        "additionalProperties": false
    })
}

fn transcript_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pane_id": pane_id_schema(),
            "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 20 },
            "advanced": { "type": "boolean", "default": false, "description": "Return diagnostic transcript records instead of conversation-only text." }
        },
        "required": ["pane_id"],
        "additionalProperties": false
    })
}

fn empty_schema() -> Value {
    json!({ "type": "object", "properties": {}, "additionalProperties": false })
}

fn pane_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "pane_id": pane_id_schema() },
        "required": ["pane_id"],
        "additionalProperties": false
    })
}

fn pane_id_schema() -> Value {
    json!({
        "type": "string",
        "pattern": "^%[0-9]+$",
        "description": "Exact tmux pane_id returned by create_pane or list_panes, including the leading % (for example %146). Do not use 146 or percent146."
    })
}

fn tag_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pane_id": pane_id_schema(),
            "tag": { "type": "string" }
        },
        "required": ["pane_id", "tag"],
        "additionalProperties": false
    })
}

fn backend_schema() -> Value {
    json!({
        "type": "string",
        "enum": ["claude", "opencode", "codex", "grok"],
        "description": "Set only when the user explicitly names Claude, OpenCode, Codex, or Grok. Omit for 'default', 'usual', or an unspecified agent type so slopd chooses its configured default. Never infer this from the current voice model or client."
    })
}

fn filter_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "tag": { "type": "string" },
            "backend": backend_schema(),
            "account": { "type": "string" },
            "raw": { "type": "boolean", "default": false }
        },
        "additionalProperties": false
    })
}

fn overview_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pane_id": pane_id_schema(),
            "tag": { "type": "string" },
            "backend": backend_schema(),
            "account": { "type": "string" },
            "context_before": { "type": "integer", "minimum": 0, "maximum": 100, "default": 0, "description": "Number of meaningful messages to return from immediately before the latest user prompt. Increase this when task_context_excerpt is not enough." },
            "context_after": { "type": "integer", "minimum": 0, "maximum": 100, "default": 0, "description": "Number of the newest agent progress or reply messages to return from after the latest user prompt. Increase this when current_activity_excerpt is not enough." }
        },
        "additionalProperties": false
    })
}

fn string_array(description: &'static str) -> Value {
    json!({ "type": "array", "items": { "type": "string" }, "description": description })
}

fn spawn_schema(fork: bool) -> Value {
    let mut schema = json!({
        "type": "object",
        "properties": {
            "start_directory": { "type": "string", "description": "Absolute, ~, or environment-expanded server path." },
            "env": string_array("Extra KEY=VALUE environment entries."),
            "env_files": string_array("Server-local dotenv files, loaded in order."),
            "extra_args": string_array("Extra arguments passed to the agent executable."),
            "no_wait": { "type": "boolean", "default": false },
            "ready_timeout": { "type": "integer", "minimum": 1, "maximum": 300, "default": 30 }
        },
        "additionalProperties": false
    });
    let properties = schema["properties"].as_object_mut().unwrap();
    if fork {
        properties.insert("pane_id".into(), pane_id_schema());
        schema["required"] = json!(["pane_id"]);
    } else {
        properties.insert("parent_pane_id".into(), pane_id_schema());
        properties.insert("account".into(), json!({ "type": "string" }));
        properties.insert("backend".into(), backend_schema());
    }
    schema
}
