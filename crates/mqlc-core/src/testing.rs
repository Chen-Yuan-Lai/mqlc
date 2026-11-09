//! Test doubles. The scripted LLM is the only one: everything else in tests is
//! real (MongoDB runs in a container).

use std::collections::VecDeque;
use std::sync::Mutex;

use crate::llm::{LlmClient, LlmError, LlmRequest, LlmResponse, Usage};

/// One canned reply.
pub enum Scripted {
    Text(String),
    /// A reply that stopped at `max_tokens`.
    Truncated(String),
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
pub struct ScriptedLlm {
    model: String,
    usage: Usage,
    replies: Mutex<VecDeque<Scripted>>,
    requests: Mutex<Vec<LlmRequest>>,
}

impl ScriptedLlm {
    pub fn new<S: Into<Scripted>>(replies: impl IntoIterator<Item = S>) -> Self {
        Self {
            model: "scripted".to_string(),
            usage: Usage::default(),
            replies: Mutex::new(replies.into_iter().map(Into::into).collect()),
            requests: Mutex::default(),
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

    pub fn requests(&self) -> Vec<LlmRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl LlmClient for ScriptedLlm {
    fn model(&self) -> &str {
        &self.model
    }

    async fn complete(&self, request: &LlmRequest) -> Result<LlmResponse, LlmError> {
        self.requests.lock().unwrap().push(request.clone());
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| LlmError::Other("scripted LLM has no replies left".into()))?;
        let (text, truncated) = match reply {
            Scripted::Text(text) => (text, false),
            Scripted::Truncated(text) => (text, true),
        };
        Ok(LlmResponse {
            text,
            usage: self.usage,
            truncated,
        })
    }
}
