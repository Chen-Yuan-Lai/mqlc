//! The single-question API: question → prompt → LLM → pipeline → rows.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::TryStreamExt;
use mongodb::Client;
use mongodb::bson::Document;
use serde::{Deserialize, Serialize};

use crate::dataset::{Dataset, DatasetError, Sample, sha256};
use crate::ir::{Pipeline, Schema};
use crate::llm::{LlmClient, LlmError, LlmRequest, LlmResponse, Turn, Usage, with_retries};
use crate::mdl::{MdlError, MdlProject};
use crate::reply::{ParseError, parse_reply};
use crate::values;

/// Room for the reply itself; thinking gets its budget on top.
const MAX_TOKENS: u32 = 4096;
/// The temperature of every run unless `Engine::with_temperature` says otherwise.
pub const DEFAULT_TEMPERATURE: f32 = 0.0;
const WRITE_STAGES: [&str; 2] = ["$out", "$merge"];
const QUERY_TIMEOUT: Duration = Duration::from_secs(60);
/// Repairs of each kind a question gets at most (experiment B).
pub const MAX_REPAIRS: u32 = 2;
/// Value corrections a question gets at most, on top of its repairs.
pub const MAX_VALUE_CORRECTIONS: u32 = 2;

/// Failures that leave no model answer to record.
#[derive(Debug, thiserror::Error)]
pub enum AskError {
    #[error(transparent)]
    Dataset(#[from] DatasetError),
    /// The arm's knowledge is missing or broken: a setup failure, like a
    /// missing dataset file.
    #[error("cannot load knowledge: {0}")]
    Knowledge(#[from] MdlError),
    /// A human MDL of a database outside the train split: human knowledge
    /// may only be written on train (ADR 0006).
    #[error(
        "refusing the human MDL of {db_id} ({split}): human knowledge may only be written for train databases"
    )]
    Leakage { db_id: String, split: String },
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

    /// For an execution error, the server's name for it (`TypeMismatch`,
    /// `Location40324`, ...), which tells execution errors apart.
    pub fn code(&self) -> Option<String> {
        use mongodb::error::ErrorKind;
        let Self::Mongo(e) = self else { return None };
        match &*e.kind {
            ErrorKind::Command(c) if !c.code_name.is_empty() => Some(c.code_name.clone()),
            ErrorKind::Command(c) => Some(c.code.to_string()),
            _ => None,
        }
    }
}

/// Which errors are fed back to the model for another reply (experiment B).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Repair {
    #[default]
    Off,
    /// An execution error, up to `MAX_REPAIRS` times: the control arm.
    Execution,
    /// The IR arm: before a candidate runs, a static check against the
    /// official schema description, whose errors are repaired up to
    /// `MAX_REPAIRS` times; a candidate still failing it then runs anyway.
    /// Execution errors are repaired as in `Execution`, with their own
    /// `MAX_REPAIRS`.
    StaticCheck,
}

impl Repair {
    /// Execution repairs per question.
    fn max_repairs(self) -> u32 {
        match self {
            Self::Off => 0,
            Self::Execution | Self::StaticCheck => MAX_REPAIRS,
        }
    }

    /// Whether candidates are statically checked: the IR arm.
    fn checks(self) -> bool {
        self == Self::StaticCheck
    }

    fn static_repairs(self) -> u32 {
        if self.checks() { MAX_REPAIRS } else { 0 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairKind {
    StaticCheck,
    Execution,
    /// A literal no document has (`Engine::with_value_correction`).
    ValueCorrection,
}

impl RepairKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::StaticCheck => "static_check",
            Self::Execution => "execution",
            Self::ValueCorrection => "value_correction",
        }
    }
}

/// One error found in a candidate.
#[derive(Debug, Clone, PartialEq)]
pub struct Problem {
    /// `execution`, `missing_value`, or a static check's `CheckKind` name.
    pub kind: String,
    /// An execution error's `AnswerError::code`.
    pub code: Option<String>,
    pub message: String,
}

/// One repair: what was wrong with the candidate before it, and the reply
/// the feedback got.
#[derive(Debug, Clone)]
pub struct RepairRound {
    pub kind: RepairKind,
    pub problems: Vec<Problem>,
    pub reply: String,
    pub usage: Usage,
    pub latency: Duration,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub collection: String,
    pub pipeline: Vec<Document>,
}

#[derive(Debug)]
pub struct Answer {
    /// The reply to the question; a repaired answer's later replies are in
    /// `repairs`.
    pub reply: String,
    /// Of the first call only; repairs have their own.
    pub usage: Usage,
    /// `None` for batch answers, which have no per-request latency.
    pub latency: Option<Duration>,
    /// The last candidate's, `None` when it could not be parsed.
    pub query: Option<Query>,
    pub rows: Result<Vec<Document>, AnswerError>,
    /// In order; the last one's reply is the candidate above.
    pub repairs: Vec<RepairRound>,
    /// What the static check still finds in the candidate above, which ran
    /// anyway once the static repairs were used up.
    pub check_problems: Vec<Problem>,
    /// Under the IR arm, what the check could not read in the candidate
    /// above (`Unsupported`, as in `ir-coverage`): it checked nothing there
    /// or after it.
    pub unchecked: Vec<String>,
}

impl Answer {
    /// Every call's usage, repairs included.
    pub fn total_usage(&self) -> Usage {
        self.repairs
            .iter()
            .fold(self.usage, |total, r| total + r.usage)
    }
}

/// Generation settings, recorded in run manifests.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub temperature: f32,
    pub max_tokens: u32,
    /// Always `thinking_budget.is_some()`; kept because every manifest has it.
    pub thinking: bool,
    /// Absent from manifests written before thinking could be turned on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_budget: Option<u32>,
    /// Only `full` until retrieval lands.
    pub knowledge_mode: String,
    /// Whether candidates are statically checked (and repaired) before they
    /// run.
    pub ir: bool,
    /// Repairs of each kind per question; 0 when repair is off. Absent from
    /// manifests written before repair existed.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub max_repairs: u32,
    /// The system prompt opens the user turn instead of going as the system
    /// prompt (#19). Absent from manifests written before it existed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub single_message: bool,
    /// Whether literals no document has are fed back with the closest values
    /// (#13). Absent from manifests written before it existed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub value_correction: bool,
}

/// Where the schema knowledge in the prompt comes from (experiment A).
#[derive(Debug, Clone, PartialEq)]
pub enum Arm {
    /// The official schema description, the paper's baseline.
    Official,
    /// The official schema description with each field rewritten to the keys
    /// the official prompt names (`data_type` as `data_format`, no
    /// `required`): a guess at how the paper sent it (#5).
    OfficialPromptKeys,
    /// The official schema description with each collection in a
    /// `<collection name="...">` tag instead of under a `Collection:` header
    /// (#19).
    OfficialTagged,
    /// The auto MDL that `mqlc profile` wrote to `dir/<db_id>/`, in place of
    /// the official description.
    AutoMdl { dir: PathBuf },
    /// The official schema description with that auto MDL merged into it
    /// field by field (`MdlProject::render_with_official`).
    OfficialAutoMdl { dir: PathBuf },
    /// A human MDL (an auto MDL a person added descriptions, formulas and
    /// instructions to) in `dir/<db_id>/`, in place of the official
    /// description. Train databases only.
    HumanMdl { dir: PathBuf },
    /// The official schema description with that human MDL merged into it.
    OfficialHumanMdl { dir: PathBuf },
    /// The official schema description with the sample's `mongodb_evidence`,
    /// the paper's "external knowledge": the upper bound of experiment A. A
    /// sample without evidence gets the official prompt unchanged.
    OfficialEvidence,
}

impl Arm {
    /// The directory of the arm's MDL projects, if it uses any.
    pub fn mdl_dir(&self) -> Option<&Path> {
        match self {
            Self::AutoMdl { dir }
            | Self::OfficialAutoMdl { dir }
            | Self::HumanMdl { dir }
            | Self::OfficialHumanMdl { dir } => Some(dir),
            _ => None,
        }
    }

    /// The arm's name in records and manifests.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Official => "official",
            Self::OfficialPromptKeys => "official-prompt-keys",
            Self::OfficialTagged => "official-tagged",
            Self::AutoMdl { .. } => "auto-mdl",
            Self::OfficialAutoMdl { .. } => "official-auto-mdl",
            Self::HumanMdl { .. } => "human-mdl",
            Self::OfficialHumanMdl { .. } => "official-human-mdl",
            Self::OfficialEvidence => "official-evidence",
        }
    }
}

pub struct Engine<L> {
    llm: L,
    mongo: Client,
    dataset: Dataset,
    arm: Arm,
    temperature: f32,
    single_message: bool,
    thinking_budget: Option<u32>,
    repair: Repair,
    value_correction: bool,
    /// Rendered MDL per database, loaded once so every prompt of a run
    /// matches the hash in its manifest.
    mdl: Mutex<HashMap<String, String>>,
    /// The static check's schema per database.
    schemas: Mutex<HashMap<String, Arc<Schema>>>,
}

impl<L: LlmClient> Engine<L> {
    pub fn new(llm: L, mongo: Client, dataset: Dataset) -> Self {
        Self {
            llm,
            mongo,
            dataset,
            arm: Arm::Official,
            temperature: DEFAULT_TEMPERATURE,
            single_message: false,
            thinking_budget: None,
            repair: Repair::Off,
            value_correction: false,
            mdl: Mutex::default(),
            schemas: Mutex::default(),
        }
    }

    pub fn with_arm(mut self, arm: Arm) -> Self {
        self.arm = arm;
        self
    }

    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = temperature;
        self
    }

    /// Sends the system prompt at the start of the user turn, before the
    /// schema block, and no system prompt.
    pub fn with_single_message(mut self, single_message: bool) -> Self {
        self.single_message = single_message;
        self
    }

    /// Turns on extended thinking with this many tokens to think in, on top
    /// of the reply's own room.
    pub fn with_thinking_budget(mut self, budget: Option<u32>) -> Self {
        self.thinking_budget = budget;
        self
    }

    pub fn with_repair(mut self, repair: Repair) -> Self {
        self.repair = repair;
        self
    }

    /// Once a candidate runs, checks that each string its `$match` stages
    /// require a field to equal is in the data; when one is not, but values
    /// like it are, feeds them back for another reply, up to
    /// `MAX_VALUE_CORRECTIONS` times. An empty result is never corrected by
    /// itself.
    pub fn with_value_correction(mut self, on: bool) -> Self {
        self.value_correction = on;
        self
    }

    fn max_tokens(&self) -> u32 {
        MAX_TOKENS + self.thinking_budget.unwrap_or(0)
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
            temperature: self.temperature,
            max_tokens: self.max_tokens(),
            thinking: self.thinking_budget.is_some(),
            thinking_budget: self.thinking_budget,
            knowledge_mode: "full".to_string(),
            ir: self.repair.checks(),
            max_repairs: self.repair.max_repairs(),
            single_message: self.single_message,
            value_correction: self.value_correction,
        }
    }

    /// Answers one question with the official prompt and the arm's knowledge,
    /// at temperature 0 unless `with_temperature` says otherwise.
    pub async fn ask(&self, sample: &Sample) -> Result<Answer, AskError> {
        let request = self.request(sample)?;
        if let Some(skipped) = self.overflow_answer(&request) {
            return Ok(skipped);
        }
        let (response, latency) = self.call(&request).await?;
        self.answer(sample, response, Some(latency)).await
    }

    async fn call(&self, request: &LlmRequest) -> Result<(LlmResponse, Duration), LlmError> {
        with_retries(|| async {
            let started = Instant::now();
            let response = self.llm.complete(request).await?;
            Ok((response, started.elapsed()))
        })
        .await
    }

    /// The answer for a request whose prompt, with room for the longest
    /// reply, does not fit the backend's context window; such a request is
    /// not sent (see `estimate_tokens`).
    pub fn overflow_answer(&self, request: &LlmRequest) -> Option<Answer> {
        let turns = request.turns.iter().flat_map(|t| [&t.reply, &t.feedback]);
        let estimated = [&request.system, &request.context, &request.user]
            .into_iter()
            .chain(turns)
            .map(|text| estimate_tokens(text))
            .sum();
        let window = self.llm.context_window();
        (estimated + u64::from(request.max_tokens) > u64::from(window)).then(|| Answer {
            reply: String::new(),
            usage: Usage::default(),
            latency: None,
            query: None,
            rows: Err(AnswerError::ContextOverflow { estimated, window }),
            repairs: Vec::new(),
            check_problems: Vec::new(),
            unchecked: Vec::new(),
        })
    }

    /// A hash per database of the knowledge the arm sends for it, recorded in
    /// run manifests so a run does not resume on different knowledge: the
    /// schema knowledge as sent, plus each question's evidence for the
    /// evidence arm and the official description the IR arm checks against.
    pub fn knowledge_hashes<'a>(
        &self,
        db_ids: impl IntoIterator<Item = &'a str>,
    ) -> Result<BTreeMap<String, String>, AskError> {
        db_ids
            .into_iter()
            .map(|db_id| {
                let split = self.dataset.split_of(db_id).unwrap_or_default();
                let mut knowledge = self.schema(split, db_id)?;
                if let Arm::OfficialEvidence = self.arm {
                    for sample in self.dataset.samples().iter().filter(|s| s.db_id == db_id) {
                        knowledge.push_str(&format!(
                            "\n\n{}\n{}",
                            sample.sample_id, sample.mongodb_evidence
                        ));
                    }
                }
                if self.repair.checks() {
                    knowledge.push_str("\n\nStatic check:\n");
                    knowledge.push_str(&self.dataset.schema_description(split, db_id)?);
                }
                Ok((db_id.to_string(), sha256(knowledge.as_bytes())))
            })
            .collect()
    }

    /// The schema knowledge the arm sends for every question on `db_id`.
    fn schema(&self, split: &str, db_id: &str) -> Result<String, AskError> {
        Ok(match &self.arm {
            Arm::Official | Arm::OfficialEvidence => {
                self.dataset.schema_description(split, db_id)?
            }
            Arm::OfficialPromptKeys => self.dataset.prompt_keys_schema_description(split, db_id)?,
            Arm::OfficialTagged => self.dataset.tagged_schema_description(split, db_id)?,
            Arm::AutoMdl { .. }
            | Arm::OfficialAutoMdl { .. }
            | Arm::HumanMdl { .. }
            | Arm::OfficialHumanMdl { .. } => self.mdl(db_id)?,
        })
    }

    /// The MDL of `db_id`, rendered for the prompt as the arm sends it.
    fn mdl(&self, db_id: &str) -> Result<String, AskError> {
        let dir = self.arm.mdl_dir().expect("only an MDL arm renders an MDL");
        let mut cache = self.mdl.lock().unwrap();
        if let Some(text) = cache.get(db_id) {
            return Ok(text.clone());
        }
        let split = self.dataset.split_of(db_id).unwrap_or_default();
        if matches!(
            self.arm,
            Arm::HumanMdl { .. } | Arm::OfficialHumanMdl { .. }
        ) && split != "train"
        {
            return Err(AskError::Leakage {
                db_id: db_id.to_string(),
                split: split.to_string(),
            });
        }
        let project = MdlProject::load(&dir.join(db_id))?;
        let text = match &self.arm {
            Arm::OfficialAutoMdl { .. } | Arm::OfficialHumanMdl { .. } => {
                project.render_with_official(&self.dataset.official_fields(split, db_id)?)
            }
            _ => project.render(),
        };
        cache.insert(db_id.to_string(), text.clone());
        Ok(text)
    }

    /// The LLM request that `ask` sends for `sample`.
    pub fn request(&self, sample: &Sample) -> Result<LlmRequest, AskError> {
        let prompt = self.dataset.prompt();
        let schema = self.schema(&sample.split, &sample.db_id)?;
        let evidence = match self.arm {
            // Sent as written: 48 evidences start or end with whitespace.
            Arm::OfficialEvidence => {
                Some(sample.mongodb_evidence.as_str()).filter(|e| !e.trim().is_empty())
            }
            _ => None,
        };
        let (system, context) = if self.single_message {
            (
                String::new(),
                format!("{}\n\n{}", prompt.system, prompt.schema_block(&schema)),
            )
        } else {
            (prompt.system.clone(), prompt.schema_block(&schema))
        };
        Ok(LlmRequest {
            system,
            context,
            user: prompt.question_block(&sample.question, evidence),
            turns: Vec::new(),
            max_tokens: self.max_tokens(),
            temperature: self.temperature,
            thinking_budget: self.thinking_budget,
        })
    }

    /// Parses and executes the reply to `sample`'s request, however it was
    /// obtained (synchronously or from a batch), and repairs it as the
    /// engine's `Repair` says. Repairs are always synchronous calls.
    pub async fn answer(
        &self,
        sample: &Sample,
        response: LlmResponse,
        latency: Option<Duration>,
    ) -> Result<Answer, AskError> {
        let mut static_left = self.repair.static_repairs();
        let mut execution_left = self.repair.max_repairs();
        // The last candidate whose values were fed back.
        let mut corrected: Option<Query> = None;
        let mut values_left = if self.value_correction {
            MAX_VALUE_CORRECTIONS
        } else {
            0
        };
        // Built at the first repair: most answers need none.
        let mut request: Option<LlmRequest> = None;
        let mut repairs = Vec::new();
        let mut text = response.text.clone();
        let mut truncated = response.truncated;
        let (query, rows, check_problems) = loop {
            let candidate = self.parse(&text, truncated);
            let check_problems = match &candidate {
                Ok(query) if self.repair.checks() => self.check(sample, query)?,
                _ => Vec::new(),
            };
            // The candidate's outcome, once it has run.
            let mut ran = None;
            let (kind, problems) = if !check_problems.is_empty() && static_left > 0 {
                static_left -= 1;
                (RepairKind::StaticCheck, check_problems)
            } else {
                let (query, rows) = match candidate {
                    Ok(query) => {
                        let rows = self.execute(&sample.db_id, &query).await?;
                        (Some(query), rows)
                    }
                    Err(e) => (None, Err(e)),
                };
                let repair = match (&query, &rows) {
                    (_, Err(e @ AnswerError::Mongo(_))) if execution_left > 0 => {
                        execution_left -= 1;
                        let problem = Problem {
                            kind: RepairKind::Execution.name().to_string(),
                            code: e.code(),
                            message: e.to_string(),
                        };
                        (RepairKind::Execution, vec![problem])
                    }
                    // The same pipeline again: the model kept the value, as
                    // the feedback allows.
                    (Some(query), Ok(_))
                        if values_left > 0 && corrected.as_ref() != Some(query) =>
                    {
                        let missing = self.missing_values(&sample.db_id, query).await?;
                        if missing.is_empty() {
                            break (Some(query.clone()), rows, check_problems);
                        }
                        values_left -= 1;
                        corrected = Some(query.clone());
                        (RepairKind::ValueCorrection, missing)
                    }
                    _ => break (query, rows, check_problems),
                };
                ran = Some((query, rows, check_problems));
                repair
            };
            if request.is_none() {
                request = Some(self.request(sample)?);
            }
            let request = request.as_mut().expect("just built");
            request.turns.push(Turn {
                reply: text.clone(),
                feedback: feedback(kind, &problems),
            });
            // No room for another exchange: the candidate stands as it is.
            if self.overflow_answer(request).is_some() {
                request.turns.pop();
                match ran {
                    Some(outcome) => break outcome,
                    None => {
                        (static_left, execution_left, values_left) = (0, 0, 0);
                        continue;
                    }
                }
            }
            let (repaired, latency) = self.call(request).await?;
            text = repaired.text.clone();
            truncated = repaired.truncated;
            repairs.push(RepairRound {
                kind,
                problems,
                reply: repaired.text,
                usage: repaired.usage,
                latency,
            });
        };
        let unchecked = match &query {
            Some(query) if self.repair.checks() => Pipeline::parse(&query.pipeline)
                .raw_stages()
                .iter()
                .flat_map(|r| r.unsupported())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .map(|u| u.to_string())
                .collect(),
            _ => Vec::new(),
        };
        Ok(Answer {
            reply: response.text,
            usage: response.usage,
            latency,
            query,
            rows,
            repairs,
            check_problems,
            unchecked,
        })
    }

    fn parse(&self, text: &str, truncated: bool) -> Result<Query, AnswerError> {
        if truncated {
            return Err(AnswerError::Truncated(self.max_tokens()));
        }
        let parsed = parse_reply(text)?;
        Ok(Query {
            collection: parsed.collection,
            pipeline: parsed.pipeline,
        })
    }

    /// The static check of `query` against the official description of the
    /// sample's database.
    fn check(&self, sample: &Sample, query: &Query) -> Result<Vec<Problem>, AskError> {
        let schema = {
            let mut cache = self.schemas.lock().unwrap();
            match cache.get(&sample.db_id) {
                Some(schema) => schema.clone(),
                None => {
                    let fields = self
                        .dataset
                        .official_field_types(&sample.split, &sample.db_id)?;
                    let schema = Arc::new(Schema::from_official(&fields));
                    cache.insert(sample.db_id.clone(), schema.clone());
                    schema
                }
            }
        };
        Ok(Pipeline::parse(&query.pipeline)
            .check(&query.collection, &schema)
            .into_iter()
            .map(|e| Problem {
                kind: e.kind.name().to_string(),
                code: None,
                message: e.message,
            })
            .collect())
    }

    /// The literals of `query` that no document has, each with the values
    /// closest to it.
    async fn missing_values(&self, db_id: &str, query: &Query) -> Result<Vec<Problem>, AskError> {
        let literals = values::literals(&query.collection, &Pipeline::parse(&query.pipeline));
        let missing = values::missing(&self.mongo.database(db_id), literals)
            .await
            .map_err(AskError::Database)?;
        Ok(missing
            .into_iter()
            .map(|m| {
                let quoted: Vec<String> = m.closest.iter().map(|v| quote(v)).collect();
                Problem {
                    kind: "missing_value".to_string(),
                    code: None,
                    message: format!(
                        "no {} document has {} in {}; the closest values there are {}",
                        m.literal.collection,
                        quote(&m.literal.value),
                        m.literal.path,
                        quoted.join(", ")
                    ),
                }
            })
            .collect())
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

/// The user turn that asks for a repair.
fn feedback(kind: RepairKind, problems: &[Problem]) -> String {
    let correct = "Correct it and reply with the whole answer again, in the same format.";
    let (what, ask) = match kind {
        RepairKind::StaticCheck => (
            "The pipeline in your reply does not fit the MongoDB schema description",
            correct,
        ),
        RepairKind::Execution => ("Running the pipeline in your reply failed", correct),
        RepairKind::ValueCorrection => (
            "Values in the pipeline in your reply do not occur in the database",
            "If the question means one of these values, use it and reply with the whole answer \
             again, in the same format. If it means a value that is not in the data, reply with \
             the same answer again.",
        ),
    };
    let list: Vec<String> = problems
        .iter()
        .map(|p| format!("- {}", p.message))
        .collect();
    format!("{what}:\n{}\n\n{ask}", list.join("\n"))
}

/// `value` as a JSON string, the way the pipeline writes it.
fn quote(value: &str) -> String {
    serde_json::Value::from(value).to_string()
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// Tokens of prompt text, at 3 bytes a token. That overestimates (English
/// runs near 4), so a prompt near the context limit is skipped rather than
/// silently truncated.
pub fn estimate_tokens(text: &str) -> u64 {
    text.len().div_ceil(3) as u64
}

/// Errors that say nothing about the query: the server is unreachable or
/// rejects our credentials.
pub(crate) fn is_unavailable(e: &mongodb::error::Error) -> bool {
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
