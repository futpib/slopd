use rmcp::model::Tool;
use serde_json::{Value, json};

use crate::schema;

pub fn all() -> Vec<Tool> {
    vec![
        tool(
            "status",
            "Show slopd daemon uptime and state.",
            empty_schema(),
        ),
        tool(
            "ps",
            "List live slopd panes. Optional filters are AND-ed.",
            filter_schema(),
        ),
        tool("run", "Create an agent pane.", spawn_schema(false)),
        tool(
            "fork",
            "Fork a pane into an independent agent session.",
            spawn_schema(true),
        ),
        tool("kill", "Terminate a managed agent pane.", pane_schema()),
        tool(
            "send",
            "Submit a prompt to panes and wait until slopd accepts it.",
            json!({
                "type": "object",
                "properties": {
                    "pane_id": { "type": "string" },
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
            "interrupt",
            "Send Ctrl+C, Ctrl+D, and Escape to interrupt a pane.",
            pane_schema(),
        ),
        tool(
            "listen",
            "Collect matching slopd events. MCP bounds the CLI stream by limit and timeout.",
            event_schema(false),
        ),
        tool(
            "wait",
            "Wait for the first event matching filters and predicates.",
            event_schema(true),
        ),
        tool(
            "transcript",
            "Read historical transcript records from a pane.",
            json!({
                "type": "object",
                "properties": {
                    "pane_id": { "type": "string" },
                    "before": { "type": "integer", "minimum": 0 },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 500, "default": 50 }
                },
                "required": ["pane_id"],
                "additionalProperties": false
            }),
        ),
        tool("tag", "Add a tag to a pane.", tag_schema()),
        tool("untag", "Remove a tag from a pane.", tag_schema()),
        tool("tags", "List all tags on a pane.", pane_schema()),
        tool(
            "backup",
            "Write a lifecycle-journal checkpoint now.",
            empty_schema(),
        ),
        tool(
            "restore",
            "Restore missing panes from the pending or latest checkpoint.",
            empty_schema(),
        ),
        tool(
            "graveyard",
            "List durable pane-death records, newest first.",
            json!({
                "type": "object",
                "properties": {
                    "boot": { "type": "integer" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 500, "default": 50 }
                },
                "additionalProperties": false
            }),
        ),
        tool(
            "revive",
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
    Tool::new(name, description, schema(input_schema))
}

fn empty_schema() -> Value {
    json!({ "type": "object", "properties": {}, "additionalProperties": false })
}

fn pane_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "pane_id": { "type": "string" } },
        "required": ["pane_id"],
        "additionalProperties": false
    })
}

fn tag_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pane_id": { "type": "string" },
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
            "account": { "type": "string" }
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
        properties.insert("pane_id".into(), json!({ "type": "string" }));
        schema["required"] = json!(["pane_id"]);
    } else {
        properties.insert("parent_pane_id".into(), json!({ "type": "string" }));
        properties.insert("account".into(), json!({ "type": "string" }));
        properties.insert("backend".into(), backend_schema());
    }
    schema
}

fn event_schema(wait: bool) -> Value {
    let mut schema = json!({
        "type": "object",
        "properties": {
            "hooks": string_array("Hook event names."),
            "events": string_array("slopd event names."),
            "transcripts": string_array("Transcript record types."),
            "pane_id": { "type": "string" },
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
