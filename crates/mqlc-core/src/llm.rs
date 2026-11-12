//! LLM backends behind one trait.

use std::future::Future;
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq)]
pub struct LlmRequest {
    pub system: String,
    /// Start of the user turn shared by many requests (the schema block);
    /// backends that support prompt caching cache up to here.
    pub context: String,
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
    /// The reply stopped at `max_tokens`.
    pub truncated: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("API returned {status}: {body}")]
    Api {
        status: u16,
        body: String,
        /// The server's `retry-after`, if it sent one.
        retry_after: Option<Duration>,
    },
    #[error("{0}")]
    Other(String),
}

/// One request of a batch, by the id it was submitted under.
#[derive(Debug, Clone, PartialEq)]
pub struct BatchItem {
    pub custom_id: String,
    /// `Err` for a request that errored, expired or was canceled: none of
    /// these is an answer, and none is billed.
    pub outcome: Result<LlmResponse, String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BatchPoll {
    InProgress,
    Ended(Vec<BatchItem>),
}

pub trait LlmClient: Send + Sync {
    /// The model id, as recorded in results and manifests.
    fn model(&self) -> &str;

    /// Which kind of API this is (`anthropic`, `openai`), for manifests.
    fn backend(&self) -> &str;

    /// The API's base URL, for manifests.
    fn endpoint(&self) -> &str;

    /// Tokens the model accepts in one prompt. Longer prompts are not sent:
    /// servers would truncate them silently.
    fn context_window(&self) -> u32;

    fn complete(
        &self,
        request: &LlmRequest,
    ) -> impl Future<Output = Result<LlmResponse, LlmError>> + Send;

    /// Submits requests as one asynchronous batch and returns its id.
    fn submit_batch(
        &self,
        requests: &[(String, LlmRequest)],
    ) -> impl Future<Output = Result<String, LlmError>> + Send;

    fn poll_batch(&self, id: &str) -> impl Future<Output = Result<BatchPoll, LlmError>> + Send;
}

/// Retries of a call the API rejected as rate limited (429) or overloaded
/// (529). Any other API error, or running out of retries, is returned.
const MAX_RETRIES: u32 = 3;
const MAX_RETRY_WAIT: Duration = Duration::from_secs(60);

/// Calls `call`, waiting out rate limits and overloads: for the server's
/// `retry-after`, or 5, 10 and 20 s when it sends none, never over a minute.
pub async fn with_retries<T, Fut>(mut call: impl FnMut() -> Fut) -> Result<T, LlmError>
where
    Fut: Future<Output = Result<T, LlmError>>,
{
    let mut retries = 0;
    loop {
        match call().await {
            Err(LlmError::Api {
                status: 429 | 529,
                retry_after,
                ..
            }) if retries < MAX_RETRIES => {
                let backoff = Duration::from_secs(5 << retries);
                tokio::time::sleep(retry_after.unwrap_or(backoff).min(MAX_RETRY_WAIT)).await;
                retries += 1;
            }
            result => return result,
        }
    }
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

impl From<MessagesResponse> for LlmResponse {
    fn from(m: MessagesResponse) -> Self {
        Self {
            text: m.content.into_iter().map(|b| b.text).collect(),
            usage: m.usage,
            truncated: m.stop_reason.as_deref() == Some("max_tokens"),
        }
    }
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(default)]
    text: String,
}

#[derive(Deserialize)]
struct BatchResponse {
    id: String,
    processing_status: String,
    results_url: Option<String>,
}

/// One line of a batch's results file.
#[derive(Deserialize)]
struct BatchResultLine {
    custom_id: String,
    result: BatchResult,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum BatchResult {
    Succeeded { message: MessagesResponse },
    Errored { error: serde_json::Value },
    Canceled,
    Expired,
}

impl AnthropicClient {
    fn params(&self, request: &LlmRequest) -> serde_json::Value {
        serde_json::json!({
            "model": self.model,
            "max_tokens": request.max_tokens,
            "temperature": request.temperature,
            "system": [{
                "type": "text",
                "text": request.system,
                "cache_control": { "type": "ephemeral" },
            }],
            "messages": [{
                "role": "user",
                "content": [
                    {
                        "type": "text",
                        "text": request.context,
                        "cache_control": { "type": "ephemeral" },
                    },
                    { "type": "text", "text": request.user },
                ],
            }],
        })
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<reqwest::Response, LlmError> {
        let resp = request
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .send()
            .await?;
        checked(resp).await
    }
}

/// `resp` if it succeeded, otherwise its status, body and `retry-after`.
async fn checked(resp: reqwest::Response) -> Result<reqwest::Response, LlmError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs);
    Err(LlmError::Api {
        status: status.as_u16(),
        body: resp.text().await.unwrap_or_default(),
        retry_after,
    })
}

/// Claude Haiku, Sonnet and Opus 4.5 all take 200K tokens.
pub const ANTHROPIC_CONTEXT_WINDOW: u32 = 200_000;

impl LlmClient for AnthropicClient {
    fn model(&self) -> &str {
        &self.model
    }

    fn backend(&self) -> &str {
        "anthropic"
    }

    fn endpoint(&self) -> &str {
        &self.base_url
    }

    fn context_window(&self) -> u32 {
        ANTHROPIC_CONTEXT_WINDOW
    }

    async fn complete(&self, request: &LlmRequest) -> Result<LlmResponse, LlmError> {
        let url = format!("{}/v1/messages", self.base_url);
        let resp = self
            .send(self.http.post(url).json(&self.params(request)))
            .await?;
        Ok(resp.json::<MessagesResponse>().await?.into())
    }

    async fn submit_batch(&self, requests: &[(String, LlmRequest)]) -> Result<String, LlmError> {
        let requests: Vec<_> = requests
            .iter()
            .map(|(id, r)| serde_json::json!({ "custom_id": id, "params": self.params(r) }))
            .collect();
        let url = format!("{}/v1/messages/batches", self.base_url);
        let resp = self
            .send(
                self.http
                    .post(url)
                    .json(&serde_json::json!({ "requests": requests })),
            )
            .await?;
        Ok(resp.json::<BatchResponse>().await?.id)
    }

    async fn poll_batch(&self, id: &str) -> Result<BatchPoll, LlmError> {
        let url = format!("{}/v1/messages/batches/{id}", self.base_url);
        let batch: BatchResponse = self.send(self.http.get(url)).await?.json().await?;
        if batch.processing_status != "ended" {
            return Ok(BatchPoll::InProgress);
        }
        let url = batch
            .results_url
            .ok_or_else(|| LlmError::Other(format!("batch {} ended without results", batch.id)))?;
        let text = self.send(self.http.get(url)).await?.text().await?;
        let items = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|line| {
                let line: BatchResultLine = serde_json::from_str(line)
                    .map_err(|e| LlmError::Other(format!("bad batch result line: {e}")))?;
                let outcome = match line.result {
                    BatchResult::Succeeded { message } => Ok(message.into()),
                    BatchResult::Errored { error } => Err(format!("errored: {error}")),
                    BatchResult::Canceled => Err("canceled".to_string()),
                    BatchResult::Expired => Err("expired".to_string()),
                };
                Ok(BatchItem {
                    custom_id: line.custom_id,
                    outcome,
                })
            })
            .collect::<Result<_, LlmError>>()?;
        Ok(BatchPoll::Ended(items))
    }
}

/// Any OpenAI-compatible `/chat/completions` endpoint: local Ollama, vLLM,
/// OpenRouter. The context window must match what the server is configured
/// for (for Ollama, `OLLAMA_CONTEXT_LENGTH`), since servers truncate longer
/// prompts without saying so.
pub struct OpenAiClient {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    model: String,
    context_window: u32,
}

impl OpenAiClient {
    /// `base_url` ends before `/chat/completions`, e.g.
    /// `http://localhost:11434/v1`.
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        model: impl Into<String>,
        context_window: u32,
    ) -> Self {
        Self {
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                // Local models on a CPU are slow.
                .timeout(Duration::from_secs(900))
                .build()
                .expect("TLS backend available"),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
            model: model.into(),
            context_window,
        }
    }
}

const NO_BATCH_MODE: &str = "the OpenAI-compatible backend has no batch mode; use --mode sync";

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: ChatUsage,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatMessage,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ChatMessage {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize, Default)]
struct ChatUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

impl LlmClient for OpenAiClient {
    fn model(&self) -> &str {
        &self.model
    }

    fn backend(&self) -> &str {
        "openai"
    }

    fn endpoint(&self) -> &str {
        &self.base_url
    }

    fn context_window(&self) -> u32 {
        self.context_window
    }

    async fn complete(&self, request: &LlmRequest) -> Result<LlmResponse, LlmError> {
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": request.max_tokens,
            "temperature": request.temperature,
            "messages": [
                { "role": "system", "content": request.system },
                { "role": "user", "content": format!("{}\n\n{}", request.context, request.user) },
            ],
        });
        let mut http = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .json(&body);
        if let Some(key) = &self.api_key {
            http = http.bearer_auth(key);
        }
        let parsed: ChatResponse = checked(http.send().await?).await?.json().await?;
        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| LlmError::Other("response has no choices".into()))?;
        Ok(LlmResponse {
            text: choice.message.content.unwrap_or_default(),
            usage: Usage {
                input_tokens: parsed.usage.prompt_tokens,
                output_tokens: parsed.usage.completion_tokens,
                ..Usage::default()
            },
            truncated: choice.finish_reason.as_deref() == Some("length"),
        })
    }

    async fn submit_batch(&self, _requests: &[(String, LlmRequest)]) -> Result<String, LlmError> {
        Err(LlmError::Other(NO_BATCH_MODE.into()))
    }

    async fn poll_batch(&self, _id: &str) -> Result<BatchPoll, LlmError> {
        Err(LlmError::Other(NO_BATCH_MODE.into()))
    }
}
