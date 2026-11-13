//! Core of mqlc: everything except the CLI and MCP front ends.

pub use mongodb;

pub mod bench;
pub mod dataset;
pub mod engine;
pub mod ir;
pub mod llm;
pub mod mdl;
mod pricing;
pub mod profile;
mod reply;

#[cfg(feature = "testing")]
pub mod testing;

pub use dataset::{Dataset, DatasetError, Sample};
pub use engine::{Answer, AnswerError, Arm, AskError, Engine, Query, Settings};
pub use llm::{
    AnthropicClient, BatchItem, BatchPoll, LlmClient, LlmError, LlmRequest, LlmResponse,
    OpenAiClient, Usage,
};
pub use reply::ParseError;
