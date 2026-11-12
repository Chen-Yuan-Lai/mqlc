//! Core of mqlc: everything except the CLI and MCP front ends.

pub use mongodb;

pub mod bench;
pub mod dataset;
pub mod engine;
pub mod llm;
mod pricing;
mod reply;

#[cfg(feature = "testing")]
pub mod testing;

pub use dataset::{Dataset, DatasetError, Sample};
pub use engine::{Answer, AnswerError, AskError, Engine, Query, Settings};
pub use llm::{
    AnthropicClient, BatchItem, BatchPoll, LlmClient, LlmError, LlmRequest, LlmResponse, Usage,
};
pub use reply::ParseError;
