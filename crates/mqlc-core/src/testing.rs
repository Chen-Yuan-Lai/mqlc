//! Test doubles. The scripted LLM is the only one: everything else in tests is
//! real (MongoDB runs in a container).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::llm::{BatchItem, BatchPoll, LlmClient, LlmError, LlmRequest, LlmResponse, Usage};

/// One canned reply.
pub enum Scripted {
    Text(String),
    /// A reply that stopped at `max_tokens`.
    Truncated(String),
    /// An HTTP error from the API, with a zero `retry-after`.
    ApiError {
        status: u16,
        body: String,
    },
}

impl Scripted {
    pub fn api_error(status: u16, body: &str) -> Self {
        Self::ApiError {
            status,
            body: body.to_string(),
        }
    }
}

impl From<String> for Scripted {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}

impl From<&str> for Scripted {
    fn from(text: &str) -> Self {
        Self::Text(text.to_string())
    }
}

/// Replies with canned texts in order and records every request it gets.
/// Running out of replies fails like an unavailable API.
///
/// Clones share one state, the way every invocation of a run talks to the
/// same API: a batch submitted through one clone is polled through another.
#[derive(Clone)]
pub struct ScriptedLlm {
    model: String,
    usage: Usage,
    context_window: u32,
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    replies: VecDeque<Scripted>,
    requests: Vec<LlmRequest>,
    batches: Vec<Batch>,
}

struct Batch {
    requests: Vec<(String, LlmRequest)>,
    /// Set by `finish_batches`.
    results: Option<Vec<BatchItem>>,
}

impl ScriptedLlm {
    pub fn new<S: Into<Scripted>>(replies: impl IntoIterator<Item = S>) -> Self {
        Self {
            model: "scripted".to_string(),
            usage: Usage::default(),
            context_window: 1_000_000,
            state: Arc::new(Mutex::new(State {
                replies: replies.into_iter().map(Into::into).collect(),
                ..State::default()
            })),
        }
    }

    pub fn with_model(mut self, model: &str) -> Self {
        self.model = model.to_string();
        self
    }

    /// Usage reported for every reply.
    pub fn with_usage(mut self, usage: Usage) -> Self {
        self.usage = usage;
        self
    }

    pub fn with_context_window(mut self, tokens: u32) -> Self {
        self.context_window = tokens;
        self
    }

    /// Requests made synchronously.
    pub fn requests(&self) -> Vec<LlmRequest> {
        self.state.lock().unwrap().requests.clone()
    }

    /// The requests of every submitted batch, in submission order.
    pub fn batches(&self) -> Vec<Vec<LlmRequest>> {
        let state = self.state.lock().unwrap();
        state
            .batches
            .iter()
            .map(|b| b.requests.iter().map(|(_, r)| r.clone()).collect())
            .collect()
    }

    /// Ends every batch still processing, answering its requests with the
    /// next replies. Until then, polls report the batches as in progress.
    pub fn finish_batches(&self) {
        let mut state = self.state.lock().unwrap();
        let State {
            replies, batches, ..
        } = &mut *state;
        for batch in batches.iter_mut().filter(|b| b.results.is_none()) {
            let results = batch
                .requests
                .iter()
                .map(|(id, _)| BatchItem {
                    custom_id: id.clone(),
                    outcome: match replies.pop_front() {
                        Some(reply) => self.response(reply).map_err(|e| format!("errored: {e}")),
                        None => Err("expired".to_string()),
                    },
                })
                .collect();
            batch.results = Some(results);
        }
    }

    fn response(&self, reply: Scripted) -> Result<LlmResponse, LlmError> {
        let (text, truncated) = match reply {
            Scripted::Text(text) => (text, false),
            Scripted::Truncated(text) => (text, true),
            Scripted::ApiError { status, body } => {
                return Err(LlmError::Api {
                    status,
                    body,
                    retry_after: Some(Duration::ZERO),
                });
            }
        };
        Ok(LlmResponse {
            text,
            usage: self.usage,
            truncated,
        })
    }
}

impl LlmClient for ScriptedLlm {
    fn model(&self) -> &str {
        &self.model
    }

    fn backend(&self) -> &str {
        "scripted"
    }

    fn endpoint(&self) -> &str {
        "scripted"
    }

    fn context_window(&self) -> u32 {
        self.context_window
    }

    async fn complete(&self, request: &LlmRequest) -> Result<LlmResponse, LlmError> {
        let mut state = self.state.lock().unwrap();
        state.requests.push(request.clone());
        let reply = state
            .replies
            .pop_front()
            .ok_or_else(|| LlmError::Other("scripted LLM has no replies left".into()))?;
        self.response(reply)
    }

    async fn submit_batch(&self, requests: &[(String, LlmRequest)]) -> Result<String, LlmError> {
        let mut state = self.state.lock().unwrap();
        state.batches.push(Batch {
            requests: requests.to_vec(),
            results: None,
        });
        Ok(format!("batch_{}", state.batches.len()))
    }

    async fn poll_batch(&self, id: &str) -> Result<BatchPoll, LlmError> {
        let state = self.state.lock().unwrap();
        let batch = id
            .strip_prefix("batch_")
            .and_then(|n| n.parse::<usize>().ok())
            .and_then(|n| state.batches.get(n.wrapping_sub(1)))
            .ok_or_else(|| LlmError::Other(format!("no batch {id}")))?;
        Ok(match &batch.results {
            None => BatchPoll::InProgress,
            Some(results) => BatchPoll::Ended(results.clone()),
        })
    }
}
