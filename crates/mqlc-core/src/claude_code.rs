//! Claude Code as the model (experiment E, ADR 0015): `claude -p` answers
//! one question at a time with MCP tools on the database, and its final
//! reply is the candidate.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::AsyncWriteExt;

use crate::engine::McpTools;
use crate::llm::{
    ANTHROPIC_CONTEXT_WINDOW, BatchPoll, HostRun, LlmClient, LlmError, LlmRequest, LlmResponse,
    Usage,
};

/// Agent turns (a reply or a round of tool calls) Claude Code gets per
/// question before it gives up.
const MAX_HOST_TURNS: u32 = 30;
/// A question still running after this long is a hung host or tool.
const TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Credentials Claude Code would bill instead of its own login.
const API_CREDENTIALS: [&str; 2] = ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"];

/// Where the MCP servers are and what they connect to.
#[derive(Debug, Clone)]
pub struct McpServers {
    pub mongo_uri: String,
    /// `dist/esm/index.js` of the pinned `mongodb-mcp-server` package.
    pub official: PathBuf,
    /// The `mqlc-mcp` binary.
    pub mqlc_mcp: PathBuf,
}

pub struct ClaudeCodeClient {
    program: PathBuf,
    model: String,
    /// The `--mcp-config` JSON.
    mcp_config: String,
    /// The servers in `mcp_config`, each of which must connect.
    servers: Vec<String>,
    /// `--allowedTools`: every tool of these servers and nothing else.
    allowed_tools: String,
    endpoint: String,
    timeout: Duration,
}

impl ClaudeCodeClient {
    /// `program` is `claude`, or a stand-in for it.
    pub fn new(
        program: impl Into<PathBuf>,
        model: impl Into<String>,
        tools: McpTools,
        servers: &McpServers,
    ) -> Self {
        let mut config = serde_json::Map::new();
        config.insert(
            "mongodb".to_string(),
            serde_json::json!({
                "command": "node",
                "args": [servers.official, "--readOnly"],
                "env": {
                    "MDB_MCP_CONNECTION_STRING": servers.mongo_uri,
                    "MDB_MCP_TELEMETRY": "disabled",
                },
            }),
        );
        if tools == McpTools::OfficialMqlc {
            config.insert(
                "mqlc".to_string(),
                serde_json::json!({
                    "command": servers.mqlc_mcp,
                    "args": [servers.mongo_uri],
                }),
            );
        }
        let names: Vec<String> = config.keys().cloned().collect();
        let allowed_tools = names
            .iter()
            .map(|n| format!("mcp__{n}"))
            .collect::<Vec<_>>()
            .join(",");
        let endpoint = format!("claude -p, MCP servers: {}", names.join(", "));
        Self {
            program: program.into(),
            model: model.into(),
            mcp_config: serde_json::json!({ "mcpServers": config }).to_string(),
            servers: names,
            allowed_tools,
            endpoint,
            timeout: TIMEOUT,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// A directory with no CLAUDE.md or settings for Claude Code to pick up.
    fn workdir() -> Result<PathBuf, LlmError> {
        let dir = std::env::temp_dir().join("mqlc-claude-code");
        std::fs::create_dir_all(&dir)
            .map_err(|e| LlmError::Other(format!("cannot create {}: {e}", dir.display())))?;
        Ok(dir)
    }

    async fn run(&self, prompt: &str, system: &str, cwd: &Path) -> Result<Exited, LlmError> {
        let mut command = tokio::process::Command::new(&self.program);
        command
            .args(["-p", "--output-format", "stream-json", "--verbose"])
            .args(["--model", &self.model])
            .args(["--setting-sources", ""])
            .args(["--strict-mcp-config", "--mcp-config", &self.mcp_config])
            .args(["--tools", ""])
            .args(["--allowedTools", &self.allowed_tools])
            .args(["--disable-slash-commands", "--no-session-persistence"])
            .args(["--max-turns", &MAX_HOST_TURNS.to_string()])
            .args(["--append-system-prompt", system])
            .current_dir(cwd)
            // Its own group, so a timeout also ends the MCP servers it started.
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for name in API_CREDENTIALS {
            command.env_remove(name);
        }
        let mut child = command.spawn().map_err(|e| {
            LlmError::Other(format!("cannot start {}: {e}", self.program.display()))
        })?;
        let group = child.id();
        let mut stdin = child.stdin.take().expect("stdin is piped");
        stdin
            .write_all(prompt.as_bytes())
            .await
            .map_err(|e| LlmError::Other(format!("cannot send the prompt: {e}")))?;
        drop(stdin);
        let output = match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Ok(output) => output.map_err(|e| LlmError::Other(format!("claude failed: {e}")))?,
            Err(_) => {
                if let Some(group) = group {
                    // Best effort: dropping the child kills `claude` itself anyway.
                    let _ = std::process::Command::new("kill")
                        .args(["-KILL", "--", &format!("-{group}")])
                        .status();
                }
                return Err(LlmError::Other(format!(
                    "claude still running after {}s",
                    self.timeout.as_secs()
                )));
            }
        };
        Ok(Exited {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            how: format!(
                "claude exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        })
    }
}

/// What a finished `claude` printed.
struct Exited {
    stdout: String,
    /// Its exit status and stderr, for when it printed no result.
    how: String,
}

/// One line of `--output-format stream-json`, as far as we read it.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Event {
    System {
        subtype: String,
        #[serde(default)]
        mcp_servers: Vec<McpServerStatus>,
    },
    Assistant {
        message: Message,
    },
    Result(Outcome),
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct McpServerStatus {
    name: String,
    status: String,
}

#[derive(Deserialize)]
struct Message {
    content: Vec<Content>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Content {
    ToolUse {
        name: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct Outcome {
    subtype: String,
    is_error: bool,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    total_cost_usd: f64,
    #[serde(default)]
    usage: Usage,
    #[serde(default)]
    api_error_status: Option<serde_json::Value>,
    #[serde(default)]
    errors: Vec<String>,
}

/// The reply in a transcript. Per ADR 0011, any of `servers` that did not
/// connect, output we cannot read and an error of Claude Code or its API are
/// infrastructure; running out of turns is the model's answer.
fn parse_transcript(exited: &Exited, servers: &[String]) -> Result<LlmResponse, LlmError> {
    let mut host = HostRun::default();
    let mut outcome = None;
    let mut connected: Option<Vec<String>> = None;
    for line in exited.stdout.lines().filter(|l| !l.trim().is_empty()) {
        let event: Event = serde_json::from_str(line)
            .map_err(|e| LlmError::Other(format!("bad claude output line ({e}): {line}")))?;
        match event {
            Event::System {
                subtype,
                mcp_servers,
            } if subtype == "init" => {
                if let Some(s) = mcp_servers.iter().find(|s| s.status != "connected") {
                    return Err(LlmError::Other(format!(
                        "MCP server {} is {}",
                        s.name, s.status
                    )));
                }
                connected = Some(mcp_servers.into_iter().map(|s| s.name).collect());
            }
            Event::Assistant { message } => {
                for content in message.content {
                    if let Content::ToolUse { name } = content {
                        *host.tool_calls.entry(name).or_default() += 1;
                    }
                }
            }
            Event::Result(o) => outcome = Some(o),
            _ => {}
        }
    }
    let outcome = outcome.ok_or_else(|| LlmError::Other(format!("no result: {}", exited.how)))?;
    let connected = connected.unwrap_or_default();
    if let Some(name) = servers.iter().find(|n| !connected.contains(n)) {
        return Err(LlmError::Other(format!("MCP server {name} is missing")));
    }
    host.cost_usd = outcome.total_cost_usd;
    if outcome.subtype == "error_max_turns" {
        host.out_of_turns = Some(MAX_HOST_TURNS);
    } else if outcome.is_error {
        return Err(LlmError::Other(format!(
            "claude failed ({}, API status {}): {}",
            outcome.subtype,
            outcome
                .api_error_status
                .filter(|s| !s.is_null())
                .map_or("none".to_string(), |s| s.to_string()),
            outcome.result.unwrap_or_else(|| outcome.errors.join("; "))
        )));
    }
    Ok(LlmResponse {
        text: outcome.result.unwrap_or_default(),
        usage: outcome.usage,
        truncated: false,
        host: Some(host),
    })
}

impl LlmClient for ClaudeCodeClient {
    fn model(&self) -> &str {
        &self.model
    }

    fn backend(&self) -> &str {
        "claude-code"
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn context_window(&self) -> u32 {
        ANTHROPIC_CONTEXT_WINDOW
    }

    async fn complete(&self, request: &LlmRequest) -> Result<LlmResponse, LlmError> {
        if !request.turns.is_empty() {
            return Err(LlmError::Other(
                "the claude-code backend answers in one session and takes no repairs".into(),
            ));
        }
        let prompt = format!("{}\n\n{}", request.context, request.user);
        let exited = self
            .run(&prompt, &request.system, &Self::workdir()?)
            .await?;
        parse_transcript(&exited, &self.servers)
    }

    async fn submit_batch(&self, _: &[(String, LlmRequest)]) -> Result<String, LlmError> {
        Err(no_batches())
    }

    async fn poll_batch(&self, _: &str) -> Result<BatchPoll, LlmError> {
        Err(no_batches())
    }
}

fn no_batches() -> LlmError {
    LlmError::Other("the claude-code backend has no batch API".into())
}
