//! The single-question API: question → prompt → LLM → pipeline → rows.

use std::time::Duration;

use futures_util::TryStreamExt;
use mongodb::Client;
use mongodb::bson::Document;

use crate::dataset::{Dataset, DatasetError, Sample};
use crate::llm::{LlmClient, LlmError, LlmRequest, Usage};
use crate::reply::{ParseError, parse_reply};

const MAX_TOKENS: u32 = 4096;
const WRITE_STAGES: [&str; 2] = ["$out", "$merge"];
const QUERY_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
pub enum AskError {
    #[error(transparent)]
    Dataset(#[from] DatasetError),
    #[error("LLM call failed: {0}")]
    Llm(#[from] LlmError),
    #[error("cannot parse LLM reply: {0}")]
    Parse(#[from] ParseError),
    #[error("refusing to run a pipeline with a {0} stage: mqlc is read-only")]
    WriteStage(String),
    #[error("query failed: {0}")]
    Mongo(#[from] mongodb::error::Error),
}

#[derive(Debug, Clone)]
pub struct Answer {
    pub collection: String,
    pub pipeline: Vec<Document>,
    pub rows: Vec<Document>,
    pub usage: Usage,
    pub reply: String,
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

    /// Answers one question with the official prompt and schema description
    /// (no evidence), at temperature 0.
    pub async fn ask(&self, sample: &Sample) -> Result<Answer, AskError> {
        let prompt = self.dataset.prompt();
        let schema = self
            .dataset
            .schema_description(&sample.split, &sample.db_id)?;
        let request = LlmRequest {
            system: prompt.system.clone(),
            user: prompt.user_message(&schema, &sample.question, None),
            max_tokens: MAX_TOKENS,
            temperature: 0.0,
        };
        let response = self.llm.complete(&request).await?;
        let parsed = parse_reply(&response.text)?;
        // The `mqlc` database user is read-only too; this check fails fast and
        // also covers connections made with a more privileged user.
        if let Some(stage) = parsed
            .pipeline
            .iter()
            .flat_map(|s| s.keys())
            .find(|k| WRITE_STAGES.contains(&k.as_str()))
        {
            return Err(AskError::WriteStage(stage.clone()));
        }

        let rows = self
            .mongo
            .database(&sample.db_id)
            .collection::<Document>(&parsed.collection)
            .aggregate(parsed.pipeline.clone())
            .max_time(QUERY_TIMEOUT)
            .await?
            .try_collect()
            .await?;

        Ok(Answer {
            collection: parsed.collection,
            pipeline: parsed.pipeline,
            rows,
            usage: response.usage,
            reply: response.text,
        })
    }
}
