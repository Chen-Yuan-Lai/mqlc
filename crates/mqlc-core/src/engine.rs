//! The single-question API: question → prompt → LLM → pipeline → rows.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use futures_util::TryStreamExt;
use mongodb::Client;
use mongodb::bson::Document;
use serde::{Deserialize, Serialize};

use crate::dataset::{Dataset, DatasetError, Sample, sha256};
use crate::llm::{LlmClient, LlmError, LlmRequest, LlmResponse, Usage, with_retries};
use crate::mdl::{MdlError, MdlProject};
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
    /// The arm's knowledge is missing or broken: a setup failure, like a
    /// missing dataset file.
    #[error("cannot load knowledge: {0}")]
    Knowledge(#[from] MdlError),
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
    /// Not the model's failure: the prompt was never sent. Recorded so a
    /// rerun does not ask again, and left out of the scores.
    #[error(
        "prompt of about {estimated} tokens plus the reply does not fit the {window}-token context window"
    )]
    ContextOverflow { estimated: u64, window: u32 },
}

impl AnswerError {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Truncated(_) => "truncated",
            Self::Parse(_) => "parse",
            Self::WriteStage(_) => "write_stage",
            Self::Mongo(_) => "execution",
            Self::ContextOverflow { .. } => "skipped_context_overflow",
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
    /// `None` for batch answers, which have no per-request latency.
    pub latency: Option<Duration>,
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

/// Where the schema knowledge in the prompt comes from (experiment A).
#[derive(Debug, Clone, PartialEq)]
pub enum Arm {
    /// The official schema description, the paper's baseline.
    Official,
    /// The auto MDL that `mqlc profile` wrote to `dir/<db_id>/`, in place of
    /// the official description.
    AutoMdl { dir: PathBuf },
}

impl Arm {
    /// The arm's name in records and manifests.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Official => "official",
            Self::AutoMdl { .. } => "auto-mdl",
        }
    }
}

pub struct Engine<L> {
    llm: L,
    mongo: Client,
    dataset: Dataset,
    arm: Arm,
    /// Rendered MDL per database, loaded once so every prompt of a run
    /// matches the hash in its manifest.
    mdl: Mutex<HashMap<String, String>>,
}

impl<L: LlmClient> Engine<L> {
    pub fn new(llm: L, mongo: Client, dataset: Dataset) -> Self {
        Self {
            llm,
            mongo,
            dataset,
            arm: Arm::Official,
            mdl: Mutex::default(),
        }
    }

    pub fn with_arm(mut self, arm: Arm) -> Self {
        self.arm = arm;
        self
    }

    pub fn arm(&self) -> &Arm {
        &self.arm
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

    /// Answers one question with the official prompt and the arm's knowledge
    /// (no evidence), at temperature 0.
    pub async fn ask(&self, sample: &Sample) -> Result<Answer, AskError> {
        let request = self.request(sample)?;
        if let Some(skipped) = self.overflow_answer(&request) {
            return Ok(skipped);
        }
        let (response, latency) = with_retries(|| async {
            let started = Instant::now();
            let response = self.llm.complete(&request).await?;
            Ok((response, started.elapsed()))
        })
        .await?;
        self.answer(sample, response, Some(latency)).await
    }

    /// The answer for a request whose prompt, with room for the longest
    /// reply, does not fit the backend's context window; such a request is
    /// not sent (see `estimate_tokens`).
    pub fn overflow_answer(&self, request: &LlmRequest) -> Option<Answer> {
        let estimated = [&request.system, &request.context, &request.user]
            .into_iter()
            .map(|text| estimate_tokens(text))
            .sum();
        let window = self.llm.context_window();
        (estimated + u64::from(request.max_tokens) > u64::from(window)).then(|| Answer {
            reply: String::new(),
            usage: Usage::default(),
            latency: None,
            query: None,
            rows: Err(AnswerError::ContextOverflow { estimated, window }),
        })
    }

    /// A hash per database of the knowledge the arm sends for it, recorded in
    /// run manifests so a run does not resume on different knowledge. Empty
    /// for the official description, which the dataset revision fixes.
    pub fn knowledge_hashes<'a>(
        &self,
        db_ids: impl IntoIterator<Item = &'a str>,
    ) -> Result<BTreeMap<String, String>, MdlError> {
        if self.arm == Arm::Official {
            return Ok(BTreeMap::new());
        }
        db_ids
            .into_iter()
            .map(|db_id| Ok((db_id.to_string(), sha256(self.mdl(db_id)?.as_bytes()))))
            .collect()
    }

    /// The auto MDL of `db_id`, rendered for the prompt.
    fn mdl(&self, db_id: &str) -> Result<String, MdlError> {
        let Arm::AutoMdl { dir } = &self.arm else {
            unreachable!("only the auto-mdl arm has an MDL")
        };
        let mut cache = self.mdl.lock().unwrap();
        if let Some(text) = cache.get(db_id) {
            return Ok(text.clone());
        }
        let text = MdlProject::load(&dir.join(db_id))?.render();
        cache.insert(db_id.to_string(), text.clone());
        Ok(text)
    }

    /// The LLM request that `ask` sends for `sample`.
    pub fn request(&self, sample: &Sample) -> Result<LlmRequest, AskError> {
        let prompt = self.dataset.prompt();
        let schema = match &self.arm {
            Arm::Official => self
                .dataset
                .schema_description(&sample.split, &sample.db_id)?,
            Arm::AutoMdl { .. } => self.mdl(&sample.db_id)?,
        };
        Ok(LlmRequest {
            system: prompt.system.clone(),
            context: prompt.schema_block(&schema),
            user: prompt.question_block(&sample.question, None),
            max_tokens: MAX_TOKENS,
            temperature: TEMPERATURE,
        })
    }

    /// Parses and executes the reply to `sample`'s request, however it was
    /// obtained (synchronously or from a batch).
    pub async fn answer(
        &self,
        sample: &Sample,
        response: LlmResponse,
        latency: Option<Duration>,
    ) -> Result<Answer, AskError> {
        let parsed = if response.truncated {
            Err(AnswerError::Truncated(MAX_TOKENS))
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

/// Tokens of prompt text, at 3 bytes a token. That overestimates (English
/// runs near 4), so a prompt near the context limit is skipped rather than
/// silently truncated.
pub fn estimate_tokens(text: &str) -> u64 {
    text.len().div_ceil(3) as u64
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
