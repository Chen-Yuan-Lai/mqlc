//! LLM backends behind one trait.

use std::future::Future;
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq)]
pub struct LlmRequest {
    pub system: String,
    pub user: String,
    pub max_tokens: u32,
    pub temperature: f32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LlmResponse {
    pub text: String,
    pub usage: Usage,
}

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("API returned {status}: {body}")]
    Api { status: u16, body: String },
    #[error("reply was cut off at max_tokens ({0})")]
    Truncated(u32),
    #[error("{0}")]
    Other(String),
}

pub trait LlmClient: Send + Sync {
    fn complete(
        &self,
        request: &LlmRequest,
    ) -> impl Future<Output = Result<LlmResponse, LlmError>> + Send;
}

/// Synchronous calls to the Anthropic Messages API.
pub struct AnthropicClient {
    http: reqwest::Client,
    api_key: String,
    model: String,
    base_url: String,
}

impl AnthropicClient {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(300))
                .build()
                .expect("TLS backend available"),
            api_key: api_key.into(),
            model: model.into(),
            base_url: "https://api.anthropic.com".to_string(),
        }
    }
}

#[derive(Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
    stop_reason: Option<String>,
    usage: Usage,
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(default)]
    text: String,
}

impl LlmClient for AnthropicClient {
    async fn complete(&self, request: &LlmRequest) -> Result<LlmResponse, LlmError> {
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": request.max_tokens,
            "temperature": request.temperature,
            "system": request.system,
            "messages": [{ "role": "user", "content": request.user }],
        });
        let resp = self
            .http
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(LlmError::Api {
                status: status.as_u16(),
                body: resp.text().await.unwrap_or_default(),
            });
        }
        let parsed: MessagesResponse = resp.json().await?;
        if parsed.stop_reason.as_deref() == Some("max_tokens") {
            return Err(LlmError::Truncated(request.max_tokens));
        }
        Ok(LlmResponse {
            text: parsed.content.into_iter().map(|b| b.text).collect(),
            usage: parsed.usage,
        })
    }
}
