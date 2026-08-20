use std::path::{Path, PathBuf};

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer};
use serde_json::{Map, Value, json};
use tokio::net::UnixStream;

use crate::schema;

#[derive(Clone)]
pub struct SlopdMcp {
    socket: PathBuf,
    allow_run: bool,
}

impl SlopdMcp {
    pub fn new(socket: PathBuf, allow_run: bool) -> Self {
        Self { socket, allow_run }
    }

    pub fn tools(&self) -> Vec<Tool> {
        let mut tools = vec![
            tool(
                "status",
                "Show slopd daemon uptime and subscriber count.",
                json!({ "type": "object", "properties": {}, "additionalProperties": false }),
            ),
            tool(
                "ps",
                "List live slopd panes. Optional tag/backend/account filters are AND-ed.",
                json!({
                    "type": "object",
                    "properties": {
                        "tag": { "type": "string", "description": "Require this pane tag." },
                        "backend": {
                            "type": "string",
                            "enum": ["claude", "opencode", "codex", "grok"],
                            "description": "Require this agent backend."
                        },
                        "account": { "type": "string", "description": "Require this slopd account." }
                    },
                    "additionalProperties": false
                }),
            ),
            tool(
                "transcript",
                "Read recent transcript records from a pane. Does not wait for a turn to finish.",
                json!({
                    "type": "object",
                    "properties": {
                        "pane_id": {
                            "type": "string",
                            "description": "Tmux pane id, for example %42."
                        },
                        "before": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "Return records strictly before this byte cursor."
                        },
                        "limit": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": 500,
                            "description": "Maximum records to return. Default 50."
                        }
                    },
                    "required": ["pane_id"],
                    "additionalProperties": false
                }),
            ),
            tool(
                "send",
                "Type a prompt into a pane and wait only until slopd accepts it. The agent may keep working after this returns; call transcript later for the answer. Identify the pane with pane_id and/or tag/backend/account filters.",
                json!({
                    "type": "object",
                    "properties": {
                        "pane_id": {
                            "type": "string",
                            "description": "Tmux pane id, for example %42."
                        },
                        "tag": { "type": "string" },
                        "backend": {
                            "type": "string",
                            "enum": ["claude", "opencode", "codex", "grok"]
                        },
                        "account": { "type": "string" },
                        "prompt": { "type": "string", "description": "Text to submit as a user prompt." },
                        "interrupt": {
                            "type": "boolean",
                            "description": "Interrupt the pane before sending. Default false."
                        },
                        "timeout": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": 300,
                            "description": "Seconds to wait for send confirmation. Default 60."
                        }
                    },
                    "required": ["prompt"],
                    "additionalProperties": false
                }),
            ),
            tool(
                "interrupt",
                "Send Ctrl+C/Ctrl+D/Escape to interrupt a running agent pane.",
                json!({
                    "type": "object",
                    "properties": {
                        "pane_id": {
                            "type": "string",
                            "description": "Tmux pane id, for example %42."
                        }
                    },
                    "required": ["pane_id"],
                    "additionalProperties": false
                }),
            ),
        ];
        if self.allow_run {
            tools.push(tool(
                "run",
                "Spawn a new agent pane. Off by default; only advertised when slopd-mcp is started with --allow-run.",
                json!({
                    "type": "object",
                    "properties": {
                        "start_directory": {
                            "type": "string",
                            "description": "Absolute working directory for the new pane."
                        },
                        "account": { "type": "string" },
                        "backend": {
                            "type": "string",
                            "enum": ["claude", "opencode", "codex", "grok"]
                        },
                        "extra_args": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Extra arguments passed to the agent executable."
                        }
                    },
                    "additionalProperties": false
                }),
            ));
        }
        tools
    }

    async fn dispatch(&self, request: CallToolRequestParams) -> Result<CallToolResult, McpError> {
        match request.name.as_ref() {
            "status" => self.status().await,
            "ps" => self.ps(request.arguments.as_ref()).await,
            "transcript" => self.transcript(request.arguments.as_ref()).await,
            "send" => self.send(request.arguments.as_ref()).await,
            "interrupt" => self.interrupt(request.arguments.as_ref()).await,
            "run" if self.allow_run => self.run(request.arguments.as_ref()).await,
            name => Err(McpError::invalid_params(
                format!("unknown tool {name}"),
                None,
            )),
        }
    }

    async fn status(&self) -> Result<CallToolResult, McpError> {
        let mut client = connect(&self.socket).await?;
        let state = client.status().await.map_err(slopd_error)?;
        ok_json(json!({
            "uptime_secs": state.uptime_secs,
            "subscriber_count": state.subscriber_count,
            "config_generation": state.config_generation,
            "pending_restore": state.pending_restore,
        }))
    }

    async fn ps(&self, arguments: Option<&Map<String, Value>>) -> Result<CallToolResult, McpError> {
        let filters = pane_filters(arguments)?;
        let mut client = connect(&self.socket).await?;
        let panes = client.ps().await.map_err(slopd_error)?;
        let panes = libslopctl::apply_filters(panes, &filters);
        ok_json(json!({ "panes": panes }))
    }

    async fn transcript(
        &self,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<CallToolResult, McpError> {
        let pane_id = required_string(arguments, "pane_id")?;
        let before = optional_u64(arguments, "before")?;
        let limit = optional_u64(arguments, "limit")?
            .unwrap_or(50)
            .clamp(1, 500);
        let mut client = connect(&self.socket).await?;
        let records = client
            .read_transcript(pane_id, before, limit)
            .await
            .map_err(slopd_error)?;
        ok_json(json!({ "records": records }))
    }

    async fn send(
        &self,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<CallToolResult, McpError> {
        let prompt = required_string(arguments, "prompt")?;
        if prompt.trim().is_empty() {
            return tool_error("prompt must not be empty");
        }
        let interrupt = optional_bool(arguments, "interrupt")?.unwrap_or(false);
        let timeout = optional_u64(arguments, "timeout")?
            .unwrap_or(60)
            .clamp(1, 300);
        let pane_id = optional_string(arguments, "pane_id");
        let filters = pane_filters(arguments)?;
        if pane_id.is_none() && filters.is_empty() {
            return tool_error("send requires pane_id or at least one of tag, backend, account");
        }

        let mut client = connect(&self.socket).await?;
        let pane_ids = if filters.is_empty() {
            let pane_id = pane_id.expect("pane_id present when filters empty");
            vec![
                client
                    .send_prompt(pane_id, prompt, timeout, interrupt)
                    .await
                    .map_err(slopd_error)?,
            ]
        } else {
            if let Some(pane_id) = pane_id {
                client
                    .send_prompt(pane_id, prompt, timeout, interrupt)
                    .await
                    .map_err(slopd_error)
                    .map(|id| vec![id])?
            } else {
                client
                    .send_filtered(
                        &filters,
                        &prompt,
                        &libslopctl::SelectMode::One,
                        timeout,
                        interrupt,
                    )
                    .await
                    .map_err(slopd_error)?
            }
        };
        ok_json(json!({ "pane_ids": pane_ids }))
    }

    async fn interrupt(
        &self,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<CallToolResult, McpError> {
        let pane_id = required_string(arguments, "pane_id")?;
        let mut client = connect(&self.socket).await?;
        let pane_id = client.interrupt(pane_id).await.map_err(slopd_error)?;
        ok_json(json!({ "pane_id": pane_id }))
    }

    async fn run(
        &self,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<CallToolResult, McpError> {
        let start_directory = optional_string(arguments, "start_directory").map(PathBuf::from);
        if let Some(path) = start_directory.as_ref()
            && !path.is_absolute()
        {
            return tool_error("start_directory must be an absolute path");
        }
        let account = optional_string(arguments, "account");
        let backend = match optional_string(arguments, "backend") {
            Some(name) => Some(
                parse_backend(&name).map_err(|message| McpError::invalid_params(message, None))?,
            ),
            None => None,
        };
        let extra_args = optional_string_array(arguments, "extra_args")?;
        let mut client = connect(&self.socket).await?;
        let pane_id = client
            .run(
                None,
                extra_args,
                start_directory,
                Vec::new(),
                account,
                backend,
            )
            .await
            .map_err(slopd_error)?;
        ok_json(json!({ "pane_id": pane_id }))
    }
}

impl ServerHandler for SlopdMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "slopd-mcp",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Supervisor for slopd-managed agent panes. Call ps to find a pane, send to submit a prompt, then transcript to read the answer. send returns when slopd accepts the prompt, not when the agent finishes. interrupt stops an in-flight turn. run exists only if this server was started with --allow-run.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(self.tools()))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tools().into_iter().find(|tool| tool.name == name)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        self.dispatch(request).await.map(CallToolResponse::Complete)
    }
}

fn tool(name: &'static str, description: &'static str, input_schema: Value) -> Tool {
    Tool::new(name, description, schema(input_schema))
}

async fn connect(
    socket: &Path,
) -> Result<
    libslopctl::Client<tokio::net::unix::OwnedReadHalf, tokio::net::unix::OwnedWriteHalf>,
    McpError,
> {
    let stream = UnixStream::connect(socket).await.map_err(|error| {
        McpError::internal_error(
            format!("failed to connect to {}: {error}", socket.display()),
            None,
        )
    })?;
    let (reader, writer) = stream.into_split();
    Ok(libslopctl::Client::new(reader, writer))
}

fn pane_filters(arguments: Option<&Map<String, Value>>) -> Result<Vec<(String, String)>, McpError> {
    let mut filters = Vec::new();
    if let Some(tag) = optional_string(arguments, "tag") {
        filters.push(("tag".into(), tag));
    }
    if let Some(backend) = optional_string(arguments, "backend") {
        parse_backend(&backend).map_err(|message| McpError::invalid_params(message, None))?;
        filters.push(("backend".into(), backend));
    }
    if let Some(account) = optional_string(arguments, "account") {
        filters.push(("account".into(), account));
    }
    Ok(filters)
}

pub fn parse_backend(name: &str) -> Result<libslop::Backend, String> {
    match name {
        "claude" => Ok(libslop::Backend::Claude),
        "opencode" => Ok(libslop::Backend::Opencode),
        "codex" => Ok(libslop::Backend::Codex),
        "grok" => Ok(libslop::Backend::Grok),
        other => Err(format!(
            "unknown backend {other:?}; expected claude, opencode, codex, or grok"
        )),
    }
}

fn optional_string(arguments: Option<&Map<String, Value>>, key: &str) -> Option<String> {
    arguments?
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.is_empty())
}

fn required_string(arguments: Option<&Map<String, Value>>, key: &str) -> Result<String, McpError> {
    optional_string(arguments, key)
        .ok_or_else(|| McpError::invalid_params(format!("{key} is required"), None))
}

fn optional_bool(
    arguments: Option<&Map<String, Value>>,
    key: &str,
) -> Result<Option<bool>, McpError> {
    match arguments.and_then(|args| args.get(key)) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(McpError::invalid_params(
            format!("{key} must be a boolean"),
            None,
        )),
    }
}

fn optional_u64(
    arguments: Option<&Map<String, Value>>,
    key: &str,
) -> Result<Option<u64>, McpError> {
    match arguments.and_then(|args| args.get(key)) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_u64()
            .ok_or_else(|| {
                McpError::invalid_params(format!("{key} must be a non-negative integer"), None)
            })
            .map(Some),
        Some(_) => Err(McpError::invalid_params(
            format!("{key} must be a non-negative integer"),
            None,
        )),
    }
}

fn optional_string_array(
    arguments: Option<&Map<String, Value>>,
    key: &str,
) -> Result<Vec<String>, McpError> {
    match arguments.and_then(|args| args.get(key)) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str().map(str::to_string).ok_or_else(|| {
                    McpError::invalid_params(format!("{key} items must be strings"), None)
                })
            })
            .collect(),
        Some(_) => Err(McpError::invalid_params(
            format!("{key} must be an array of strings"),
            None,
        )),
    }
}

fn slopd_error(error: libslopctl::Error) -> McpError {
    match error {
        libslopctl::Error::SelectError(message) | libslopctl::Error::FilterError(message) => {
            McpError::invalid_params(message, None)
        }
        other => McpError::internal_error(other.to_string(), None),
    }
}

fn ok_json(value: Value) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![ContentBlock::text(
        serde_json::to_string(&value).unwrap_or_else(|_| value.to_string()),
    )]))
}

fn tool_error(message: impl Into<String>) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::error(vec![ContentBlock::text(
        message.into(),
    )]))
}

#[cfg(test)]
mod tests {
    use super::parse_backend;

    #[test]
    fn parse_backend_accepts_canonical_names() {
        assert_eq!(parse_backend("grok").unwrap(), libslop::Backend::Grok);
        assert!(parse_backend("claude-code").is_err());
    }
}
