use rmcp::model::{Tool, ToolAnnotations};
use serde_json::{Value, json};

use crate::schema;

pub fn all() -> Vec<Tool> {
    vec![
        tool(
            "get_status",
            "Show slopd daemon uptime and state.",
            empty_schema(),
        ),
        tool(
            "list_panes",
            "List live slopd panes. Optional filters are AND-ed. Returns compact records unless raw is true.",
            filter_schema(),
        ),
        tool(
            "create_pane",
            "Create an agent pane. Reuse the returned pane_id exactly, including its leading %.",
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
            "send_prompt",
            "Submit a prompt and wait only until slopd accepts it. Copy pane_id exactly from create_pane or list_panes, including %. To retrieve the reply, call wait_for_event with transcripts=[\"assistant\"], then read_transcript with the same pane_id.",
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
            "interrupt_pane",
            "Send Ctrl+C, Ctrl+D, and Escape to interrupt a pane.",
            pane_schema(),
        ),
        tool(
            "collect_events",
            "Collect matching slopd events. MCP bounds the CLI stream by limit and timeout.",
            event_schema(false),
        ),
        tool(
            "wait_for_event",
            "Wait for the first matching event. After send_prompt, use the same exact pane_id and transcripts=[\"assistant\"] before calling read_transcript.",
            event_schema(true),
        ),
        tool(
            "read_transcript",
            "Read historical transcript records from a pane. Returns compact records unless raw is true.",
            json!({
                "type": "object",
                "properties": {
                    "pane_id": pane_id_schema(),
                    "before": { "type": "integer", "minimum": 0 },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 500, "default": 50 },
                    "raw": { "type": "boolean", "default": false }
                },
                "required": ["pane_id"],
                "additionalProperties": false
            }),
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
    ]
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
        "list_panes" => ("List live panes", true, false, true),
        "create_pane" => ("Create pane", false, false, false),
        "fork_pane" => ("Fork pane", false, false, false),
        "kill_pane" => ("Kill pane", false, true, true),
        "send_prompt" => ("Send prompt", false, false, false),
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
        "list_panes" => json!({
            "count": { "type": "integer" },
            "panes": { "type": "array", "items": { "type": "object" } }
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
        "send_prompt" => json!({
            "pane_ids": { "type": "array", "items": pane_id_schema() }
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
    json!({ "type": "string", "enum": ["claude", "opencode", "codex", "grok"] })
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

fn event_schema(wait: bool) -> Value {
    let transcripts = if wait {
        string_array(
            "Transcript types. Portable aliases include assistant and user; backend-native types are also accepted.",
        )
    } else {
        string_array("Backend-native transcript record types.")
    };
    let mut schema = json!({
        "type": "object",
        "properties": {
            "hooks": string_array("Hook event names."),
            "events": string_array("slopd event names."),
            "transcripts": transcripts,
            "pane_id": pane_id_schema(),
            "session_id": { "type": "string" },
            "where": string_array("Server-side payload predicates in PATH=VALUE form."),
            "timeout": { "type": "integer", "minimum": 0, "maximum": 300, "default": 60 }
        },
        "additionalProperties": false
    });
    let properties = schema["properties"].as_object_mut().unwrap();
    if wait {
        properties.insert(
            "until".into(),
            string_array("Stop predicates in PATH=VALUE form."),
        );
        properties.insert(
            "no_snapshot".into(),
            json!({ "type": "boolean", "default": false }),
        );
    } else {
        properties.insert(
            "replay".into(),
            json!({ "type": "integer", "minimum": 0, "maximum": 500 }),
        );
        properties.insert(
            "limit".into(),
            json!({ "type": "integer", "minimum": 1, "maximum": 500, "default": 50 }),
        );
    }
    schema
}
