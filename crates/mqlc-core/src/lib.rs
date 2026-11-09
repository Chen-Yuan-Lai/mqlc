//! Core of mqlc: everything except the CLI and MCP front ends.

pub use mongodb;

pub mod dataset;
pub mod engine;
pub mod llm;
mod reply;

#[cfg(feature = "testing")]
pub mod testing;

pub use dataset::{Dataset, DatasetError, Sample};
pub use engine::{Answer, AskError, Engine};
pub use llm::{AnthropicClient, LlmClient, LlmError, LlmRequest, LlmResponse, Usage};
pub use reply::ParseError;
