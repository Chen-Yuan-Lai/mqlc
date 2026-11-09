//! The single-question API: question → prompt → LLM → pipeline → rows.

use std::time::{Duration, Instant};

use futures_util::TryStreamExt;
use mongodb::Client;
use mongodb::bson::Document;
use serde::{Deserialize, Serialize};

use crate::dataset::{Dataset, DatasetError, Sample};
use crate::llm::{LlmClient, LlmError, LlmRequest, Usage};
use crate::reply::{ParseError, parse_reply};

const MAX_TOKENS: u32 = 4096;
const TEMPERATURE: f32 = 0.0;
const WRITE_STAGES: [&str; 2] = ["$out", "$merge"];
const QUERY_TIMEOUT: Duration = Duration::from_secs(60);

/// Failures that leave no model answer to record.
#[derive(Debug, thiserror::Error)]
pub enum AskError {
    #[error(transparent)]
    Dataset(#[from] DatasetError),
    #[error("LLM call failed: {0}")]
    Llm(#[from] LlmError),
    #[error("database unavailable: {0}")]
    Database(mongodb::error::Error),
}

/// Failures of the model's answer itself: these are recorded and scored as
/// wrong, not retried.
#[derive(Debug, thiserror::Error)]
pub enum AnswerError {
    #[error("reply was cut off at max_tokens ({0})")]
    Truncated(u32),
    #[error("cannot parse LLM reply: {0}")]
    Parse(#[from] ParseError),
    #[error("refusing to run a pipeline with a {0} stage: mqlc is read-only")]
    WriteStage(String),
    #[error("query failed: {0}")]
    Mongo(mongodb::error::Error),
}

impl AnswerError {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Truncated(_) => "truncated",
            Self::Parse(_) => "parse",
            Self::WriteStage(_) => "write_stage",
            Self::Mongo(_) => "execution",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub collection: String,
    pub pipeline: Vec<Document>,
}

#[derive(Debug)]
pub struct Answer {
    pub reply: String,
    pub usage: Usage,
    pub latency: Duration,
    /// `None` when the reply could not be parsed.
    pub query: Option<Query>,
    pub rows: Result<Vec<Document>, AnswerError>,
}

/// Generation settings, recorded in run manifests.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub temperature: f32,
    pub max_tokens: u32,
    pub thinking: bool,
    /// Only `full` until retrieval lands.
    pub knowledge_mode: String,
    /// Whether the static-check repair loop is on (not implemented yet).
    pub ir: bool,
}

pub struct Engine<L> {
    llm: L,
    mongo: Client,
    dataset: Dataset,
}

impl<L: LlmClient> Engine<L> {
    pub fn new(llm: L, mongo: Client, dataset: Dataset) -> Self {
        Self {
            llm,
            mongo,
            dataset,
        }
    }

    pub fn dataset(&self) -> &Dataset {
        &self.dataset
    }

    pub fn llm(&self) -> &L {
        &self.llm
    }

    pub fn settings(&self) -> Settings {
        Settings {
            temperature: TEMPERATURE,
            max_tokens: MAX_TOKENS,
            thinking: false,
            knowledge_mode: "full".to_string(),
            ir: false,
        }
    }

    /// Answers one question with the official prompt and schema description
    /// (no evidence), at temperature 0.
    pub async fn ask(&self, sample: &Sample) -> Result<Answer, AskError> {
        let prompt = self.dataset.prompt();
        let schema = self
            .dataset
            .schema_description(&sample.split, &sample.db_id)?;
        let request = LlmRequest {
            system: prompt.system.clone(),
            context: prompt.schema_block(&schema),
            user: prompt.question_block(&sample.question, None),
            max_tokens: MAX_TOKENS,
            temperature: TEMPERATURE,
        };
        let started = Instant::now();
        let response = self.llm.complete(&request).await?;
        let latency = started.elapsed();

        let parsed = if response.truncated {
            Err(AnswerError::Truncated(request.max_tokens))
        } else {
            parse_reply(&response.text).map_err(AnswerError::from)
        };
        let (query, rows) = match parsed {
            Err(e) => (None, Err(e)),
            Ok(parsed) => {
                let query = Query {
                    collection: parsed.collection,
                    pipeline: parsed.pipeline,
                };
                let rows = self.execute(&sample.db_id, &query).await?;
                (Some(query), rows)
            }
        };
        Ok(Answer {
            reply: response.text,
            usage: response.usage,
            latency,
            query,
            rows,
        })
    }

    /// The outer error is the database failing; the inner one is the query
    /// failing, which is the model's answer being wrong.
    async fn execute(
        &self,
        db_id: &str,
        query: &Query,
    ) -> Result<Result<Vec<Document>, AnswerError>, AskError> {
        // The `mqlc` database user is read-only too; this check fails fast and
        // also covers connections made with a more privileged user.
        if let Some(stage) = query
            .pipeline
            .iter()
            .flat_map(|s| s.keys())
            .find(|k| WRITE_STAGES.contains(&k.as_str()))
        {
            return Ok(Err(AnswerError::WriteStage(stage.clone())));
        }
        let rows = async {
            self.mongo
                .database(db_id)
                .collection::<Document>(&query.collection)
                .aggregate(query.pipeline.clone())
                .max_time(QUERY_TIMEOUT)
                .await?
                .try_collect()
                .await
        }
        .await;
        match rows {
            Ok(rows) => Ok(Ok(rows)),
            Err(e) if is_unavailable(&e) => Err(AskError::Database(e)),
            Err(e) => Ok(Err(AnswerError::Mongo(e))),
        }
    }
}

/// Errors that say nothing about the query: the server is unreachable or
/// rejects our credentials.
fn is_unavailable(e: &mongodb::error::Error) -> bool {
    use mongodb::error::ErrorKind;
    matches!(
        *e.kind,
        ErrorKind::ServerSelection { .. }
            | ErrorKind::Io(_)
            | ErrorKind::ConnectionPoolCleared { .. }
            | ErrorKind::Authentication { .. }
            | ErrorKind::DnsResolve { .. }
    )
}
