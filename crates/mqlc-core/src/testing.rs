//! Test doubles. The scripted LLM is the only one: everything else in tests is
//! real (MongoDB runs in a container).

use std::collections::VecDeque;
use std::sync::Mutex;

use crate::llm::{LlmClient, LlmError, LlmRequest, LlmResponse, Usage};

/// Replies with canned texts in order and records every request it gets.
#[derive(Default)]
pub struct ScriptedLlm {
    replies: Mutex<VecDeque<String>>,
    requests: Mutex<Vec<LlmRequest>>,
}

impl ScriptedLlm {
    pub fn new<S: Into<String>>(replies: impl IntoIterator<Item = S>) -> Self {
        Self {
            replies: Mutex::new(replies.into_iter().map(Into::into).collect()),
            requests: Mutex::default(),
        }
    }

    pub fn requests(&self) -> Vec<LlmRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl LlmClient for ScriptedLlm {
    async fn complete(&self, request: &LlmRequest) -> Result<LlmResponse, LlmError> {
        self.requests.lock().unwrap().push(request.clone());
        let text = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| LlmError::Other("scripted LLM has no replies left".into()))?;
        Ok(LlmResponse {
            text,
            usage: Usage::default(),
        })
    }
}
