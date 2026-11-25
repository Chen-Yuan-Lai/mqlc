//! Core of mqlc: everything except the CLI and MCP front ends.

pub use mongodb;

pub mod bench;
mod claude_code;
pub mod dataset;
pub mod engine;
pub mod ir;
pub mod llm;
pub mod mdl;
mod pricing;
pub mod profile;
mod reply;
mod values;

#[cfg(feature = "testing")]
pub mod testing;

pub use claude_code::{ClaudeCodeClient, McpServers};
pub use dataset::{Dataset, DatasetError, Sample};
pub use engine::{
    Answer, AnswerError, Arm, AskError, Engine, McpTools, Problem, Query, Repair, RepairKind,
    RepairRound, Settings,
};
pub use llm::{
    AnthropicClient, BatchItem, BatchPoll, HostRun, LlmClient, LlmError, LlmRequest, LlmResponse,
    OpenAiClient, Turn, Usage,
};
pub use reply::ParseError;
