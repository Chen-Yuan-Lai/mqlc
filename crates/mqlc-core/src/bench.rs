//! `mqlc bench`: answer a set of samples and append one record per answer
//! to `results.jsonl` in a run directory, either one call at a time or
//! through the asynchronous batch API.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use mongodb::bson::Bson;
use serde::{Deserialize, Serialize};

use crate::dataset::Sample;
use crate::engine::{Answer, Engine, Settings};
use crate::llm::{ANTHROPIC_CONTEXT_WINDOW, BatchPoll, LlmClient, LlmRequest, Usage, with_retries};
use crate::pricing;

const RESULTS: &str = "results.jsonl";
const MANIFEST: &str = "manifest.json";
/// Batches submitted for the run, so a later invocation collects them.
const BATCHES: &str = "batches.json";
/// The only arm until knowledge arms land.
const ARM: &str = "official";

#[derive(Debug, thiserror::Error)]
pub enum BenchError {
    #[error("cannot use run directory {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(
        "{path} was started with a different configuration; use a new run directory\n  was: {was}\n  now: {now}"
    )]
    ConfigMismatch {
        path: PathBuf,
        was: String,
        now: String,
    },
    #[error("{path} is not a valid manifest: {source}")]
    BadManifest {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("{path} is not a valid batch list: {source}")]
    BadBatches {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("{path} line {line} is not a valid record: {source}")]
    Corrupt {
        path: PathBuf,
        line: usize,
        source: serde_json::Error,
    },
}

/// One line of `results.jsonl`: one candidate answer to one sample.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub arm: String,
    pub model: String,
    pub sample_id: String,
    pub candidate: u32,
    pub db_id: String,
    pub collection: Option<String>,
    /// Canonical Extended JSON, so scoring sees the exact BSON types.
    pub pipeline: Option<serde_json::Value>,
    pub row_count: Option<usize>,
    pub error: Option<RecordError>,
    pub usage: Usage,
    /// `None` for batch answers.
    pub latency_ms: Option<u64>,
    /// Answered through the batch API, at its discount.
    #[serde(default)]
    pub batch: bool,
    pub cost_usd: Option<f64>,
    pub reply: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordError {
    /// `truncated`, `parse`, `write_stage` or `execution`.
    pub kind: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    /// Answered in this invocation.
    pub completed: usize,
    /// Already answered by an earlier invocation.
    pub skipped: usize,
    /// Of the samples asked for, how many still have no answer.
    pub remaining: usize,
    /// Why the run stopped early, if it did.
    pub stopped: Option<String>,
    /// Cost of every record in the run so far, earlier invocations included.
    pub cost_usd: f64,
    /// Records in the run whose model has no known price, so are not in
    /// `cost_usd`.
    pub unpriced: usize,
    /// A submitted batch still processing; rerun to collect it.
    pub pending_batch: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// One call per sample, answered as it goes.
    #[default]
    Sync,
    /// One batch for every unanswered sample, at half price; answers arrive
    /// when the batch ends (usually within an hour, at most 24).
    Batch,
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub mode: Mode,
    /// Stop asking once the runs next to this one (every run directory under
    /// the same parent, this one included) have together cost this much.
    pub spend_limit_usd: Option<f64>,
    /// Batch mode: how often to check a pending batch until it ends. `None`
    /// returns after one check, with the batch in `Summary::pending_batch`.
    pub batch_poll: Option<Duration>,
}

/// What `run` reports as it goes.
#[derive(Debug, Clone, Copy)]
pub enum Progress<'a> {
    Answered(&'a Record),
    /// The batch is still processing; `run` waits for it if asked to.
    BatchPending(&'a str),
}

/// `manifest.json`: what a run measures, fixed at its first invocation, plus
/// one entry per invocation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub config: RunConfig,
    pub invocations: Vec<Invocation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunConfig {
    pub arm: String,
    pub model: String,
    /// Absent from manifests written before other backends existed.
    #[serde(default = "anthropic")]
    pub backend: String,
    #[serde(default = "anthropic_context_window")]
    pub context_window: u32,
    #[serde(flatten)]
    pub settings: Settings,
    pub prompt_hash: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Invocation {
    pub started_at: String,
    pub mqlc_version: String,
    /// The API's base URL, which may change between invocations (a local
    /// server on another port) without changing what the run measures.
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub mode: Mode,
    pub samples: usize,
}

/// An entry of `batches.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct SubmittedBatch {
    id: String,
    submitted_at: String,
    /// Each request's `custom_id` is its sample id.
    sample_ids: Vec<String>,
    collected_at: Option<String>,
}

/// `mqlc-core` version and the git commit it was built from.
pub fn version() -> String {
    format!(
        "{}+{}",
        env!("CARGO_PKG_VERSION"),
        option_env!("MQLC_GIT_SHA").unwrap_or("unknown")
    )
}

/// Answers every sample of `samples` not answered yet. In batch mode it
/// collects the run's submitted batch once it has ended, then submits the
/// samples still unanswered as a new batch and waits for that one too (or,
/// without `batch_poll`, returns it as pending).
pub async fn run<L: LlmClient>(
    engine: &Engine<L>,
    run_dir: &Path,
    samples: &[Sample],
    options: &Options,
    on_progress: impl FnMut(Progress),
) -> Result<Summary, BenchError> {
    let io = |source| BenchError::Io {
        path: run_dir.to_path_buf(),
        source,
    };
    fs::create_dir_all(run_dir).map_err(io)?;
    let model = engine.llm().model().to_string();
    record_invocation(
        &run_dir.join(MANIFEST),
        RunConfig {
            arm: ARM.to_string(),
            model: model.clone(),
            backend: engine.llm().backend().to_string(),
            context_window: engine.llm().context_window(),
            settings: engine.settings(),
            prompt_hash: engine.dataset().prompt().hash().to_string(),
        },
        engine.llm().endpoint(),
        options.mode,
        samples.len(),
    )?;
    let results_path = run_dir.join(RESULTS);
    let earlier = load_records(&results_path)?;
    let mut rec = Recorder {
        file: open_append(&results_path).map_err(io)?,
        path: run_dir.to_path_buf(),
        model,
        done: earlier.iter().map(Key::of).collect(),
        on_progress,
        summary: Summary {
            completed: 0,
            skipped: 0,
            remaining: 0,
            stopped: None,
            // `0.0 +`: an empty f64 sum is -0.0.
            cost_usd: 0.0 + earlier.iter().filter_map(|r| r.cost_usd).sum::<f64>(),
            unpriced: earlier.iter().filter(|r| r.cost_usd.is_none()).count(),
            pending_batch: None,
        },
    };
    rec.summary.skipped = samples.iter().filter(|s| rec.is_done(s)).count();
    let budget = Budget::new(run_dir, options.spend_limit_usd);
    let batches_path = run_dir.join(BATCHES);

    let stopped = match options.mode {
        Mode::Sync => run_sync(engine, samples, &batches_path, &budget, &mut rec).await?,
        Mode::Batch => {
            run_batch(engine, samples, &batches_path, options, &budget, &mut rec).await?
        }
    };
    rec.summary.stopped = stopped;
    rec.summary.remaining = samples.iter().filter(|s| !rec.is_done(s)).count();
    Ok(rec.summary)
}

/// Returns why the run stopped early, if it did.
async fn run_sync<L: LlmClient>(
    engine: &Engine<L>,
    samples: &[Sample],
    batches_path: &Path,
    budget: &Budget,
    rec: &mut Recorder<impl FnMut(Progress)>,
) -> Result<Option<String>, BenchError> {
    if let Some(batch) = load_batches(batches_path)?
        .into_iter()
        .find(|b| b.collected_at.is_none())
    {
        return Ok(Some(format!(
            "batch {} is still pending in this run; collect it in batch mode first, \
             or its samples would be paid for twice",
            batch.id
        )));
    }
    for sample in samples {
        if rec.is_done(sample) {
            continue;
        }
        if let Some(reason) = budget.exceeded(rec.summary.cost_usd) {
            return Ok(Some(reason));
        }
        match engine.ask(sample).await {
            Ok(answer) => rec.write(sample, answer, false)?,
            // No answer to record: the API or the database is unavailable, or
            // the dataset is broken. A rerun picks up from this sample.
            Err(e) => return Ok(Some(e.to_string())),
        }
    }
    Ok(None)
}

/// Returns why the run stopped early, if it did.
async fn run_batch<L: LlmClient>(
    engine: &Engine<L>,
    samples: &[Sample],
    batches_path: &Path,
    options: &Options,
    budget: &Budget,
    rec: &mut Recorder<impl FnMut(Progress)>,
) -> Result<Option<String>, BenchError> {
    let mut batches = load_batches(batches_path)?;
    'pass: loop {
        let mut unanswered = Vec::new();
        for i in 0..batches.len() {
            if batches[i].collected_at.is_some() {
                continue;
            }
            let id = batches[i].id.clone();
            let items = match with_retries(|| engine.llm().poll_batch(&id)).await {
                Ok(BatchPoll::Ended(items)) => items,
                Ok(BatchPoll::InProgress) => {
                    (rec.on_progress)(Progress::BatchPending(&id));
                    match options.batch_poll {
                        Some(interval) => {
                            tokio::time::sleep(interval).await;
                            continue 'pass;
                        }
                        None => {
                            rec.summary.pending_batch = Some(id);
                            return Ok(None);
                        }
                    }
                }
                Err(e) => return Ok(Some(e.to_string())),
            };
            let mut outcomes: HashMap<_, _> = items
                .into_iter()
                .map(|i| (i.custom_id, i.outcome))
                .collect();
            for sample_id in &batches[i].sample_ids {
                let Some(sample) = engine.dataset().sample(sample_id) else {
                    continue;
                };
                if rec.is_done(sample) {
                    continue;
                }
                match outcomes.remove(sample_id) {
                    // A database failure leaves the batch uncollected, so a
                    // rerun answers from its results again.
                    Some(Ok(response)) => match engine.answer(sample, response, None).await {
                        Ok(answer) => rec.write(sample, answer, true)?,
                        Err(e) => return Ok(Some(e.to_string())),
                    },
                    Some(Err(reason)) => unanswered.push(format!("{sample_id}: {reason}")),
                    None => unanswered.push(format!("{sample_id}: missing from the results")),
                }
            }
            batches[i].collected_at = Some(now());
            save_batches(batches_path, &batches)?;
        }
        // Like a failed call: nothing recorded, and a rerun asks again.
        if let Some(first) = unanswered.first() {
            return Ok(Some(format!(
                "{} batch requests got no answer (first: {first}); rerun to resubmit them",
                unanswered.len()
            )));
        }

        let unanswered: Vec<&Sample> = samples.iter().filter(|s| !rec.is_done(s)).collect();
        if unanswered.is_empty() {
            return Ok(None);
        }
        if let Some(reason) = budget.exceeded(rec.summary.cost_usd) {
            return Ok(Some(reason));
        }
        let mut requests: Vec<(String, LlmRequest)> = Vec::new();
        for sample in unanswered {
            let request = match engine.request(sample) {
                Ok(request) => request,
                Err(e) => return Ok(Some(e.to_string())),
            };
            match engine.overflow_answer(&request) {
                Some(skipped) => rec.write(sample, skipped, false)?,
                None => requests.push((sample.sample_id.clone(), request)),
            }
        }
        if requests.is_empty() {
            return Ok(None);
        }
        let id = match with_retries(|| engine.llm().submit_batch(&requests)).await {
            Ok(id) => id,
            Err(e) => return Ok(Some(e.to_string())),
        };
        batches.push(SubmittedBatch {
            id,
            submitted_at: now(),
            sample_ids: requests.into_iter().map(|(id, _)| id).collect(),
            collected_at: None,
        });
        save_batches(batches_path, &batches)?;
    }
}

/// Appends records to `results.jsonl` and keeps the run's totals.
struct Recorder<F> {
    file: File,
    path: PathBuf,
    model: String,
    done: HashSet<Key>,
    on_progress: F,
    summary: Summary,
}

impl<F: FnMut(Progress)> Recorder<F> {
    fn is_done(&self, sample: &Sample) -> bool {
        self.done.contains(&Key {
            arm: ARM.to_string(),
            model: self.model.clone(),
            sample_id: sample.sample_id.clone(),
            candidate: 0,
        })
    }

    fn write(&mut self, sample: &Sample, answer: Answer, batch: bool) -> Result<(), BenchError> {
        let record = Record {
            arm: ARM.to_string(),
            model: self.model.clone(),
            sample_id: sample.sample_id.clone(),
            candidate: 0,
            db_id: sample.db_id.clone(),
            collection: answer.query.as_ref().map(|q| q.collection.clone()),
            pipeline: answer.query.as_ref().map(|q| {
                Bson::Array(q.pipeline.iter().cloned().map(Bson::Document).collect())
                    .into_canonical_extjson()
            }),
            row_count: answer.rows.as_ref().ok().map(Vec::len),
            error: answer.rows.as_ref().err().map(|e| RecordError {
                kind: e.kind().to_string(),
                message: e.to_string(),
            }),
            usage: answer.usage,
            latency_ms: answer.latency.map(|l| l.as_millis() as u64),
            batch,
            // Nothing used costs nothing, priced model or not (a prompt that
            // was never sent).
            cost_usd: if answer.usage == Usage::default() {
                Some(0.0)
            } else {
                pricing::cost_usd(&self.model, &answer.usage, batch)
            },
            reply: answer.reply,
        };
        // One write per line, flushed, so a kill loses at most the line in flight.
        let mut line = serde_json::to_string(&record).expect("records serialize");
        line.push('\n');
        let io = |source| BenchError::Io {
            path: self.path.clone(),
            source,
        };
        self.file.write_all(line.as_bytes()).map_err(io)?;
        self.file.flush().map_err(io)?;
        match record.cost_usd {
            Some(c) => self.summary.cost_usd += c,
            None => self.summary.unpriced += 1,
        }
        self.done.insert(Key::of(&record));
        (self.on_progress)(Progress::Answered(&record));
        self.summary.completed += 1;
        Ok(())
    }
}

/// The spend limit over every run directory next to this one.
struct Budget {
    root: PathBuf,
    /// What the other runs had cost when this invocation started.
    others_usd: f64,
    limit_usd: Option<f64>,
}

impl Budget {
    fn new(run_dir: &Path, limit_usd: Option<f64>) -> Self {
        let root = match run_dir.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        Self {
            others_usd: spent_by_other_runs(&root, run_dir),
            root,
            limit_usd,
        }
    }

    /// Why the run must stop, if this run's cost brings the total to the limit.
    fn exceeded(&self, run_cost_usd: f64) -> Option<String> {
        let spent = self.others_usd + run_cost_usd;
        let limit = self.limit_usd.filter(|limit| spent >= *limit)?;
        Some(format!(
            "spend limit reached: runs under {} have cost ${spent:.2} (limit ${limit:.2}); \
             review the spend, then raise the limit to continue",
            self.root.display()
        ))
    }
}

fn anthropic() -> String {
    "anthropic".to_string()
}

fn anthropic_context_window() -> u32 {
    ANTHROPIC_CONTEXT_WINDOW
}

/// Total cost recorded by every run directory under `root` except
/// `run_dir`. Read-only, and lenient: another run may be writing its last line.
fn spent_by_other_runs(root: &Path, run_dir: &Path) -> f64 {
    #[derive(Deserialize)]
    struct Cost {
        cost_usd: Option<f64>,
    }
    let Ok(entries) = fs::read_dir(root) else {
        return 0.0;
    };
    entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.file_name() != run_dir.file_name())
        .filter_map(|p| fs::read_to_string(p.join(RESULTS)).ok())
        .flat_map(|text| {
            text.lines()
                .filter_map(|l| serde_json::from_str::<Cost>(l).ok()?.cost_usd)
                .collect::<Vec<_>>()
        })
        .sum()
}

fn now() -> String {
    mongodb::bson::DateTime::now()
        .try_to_rfc3339_string()
        .expect("now is representable")
}

fn load_batches(path: &Path) -> Result<Vec<SubmittedBatch>, BenchError> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|source| BenchError::BadBatches {
            path: path.to_path_buf(),
            source,
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(source) => Err(BenchError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn save_batches(path: &Path, batches: &[SubmittedBatch]) -> Result<(), BenchError> {
    let text = serde_json::to_string_pretty(batches).expect("batches serialize");
    fs::write(path, text + "\n").map_err(|source| BenchError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Creates the manifest, or checks that `config` matches the one the run was
/// started with, and appends this invocation.
fn record_invocation(
    path: &Path,
    config: RunConfig,
    endpoint: &str,
    mode: Mode,
    samples: usize,
) -> Result<(), BenchError> {
    let io = |source| BenchError::Io {
        path: path.to_path_buf(),
        source,
    };
    let mut manifest = match fs::read_to_string(path) {
        Ok(text) => {
            let manifest: Manifest =
                serde_json::from_str(&text).map_err(|source| BenchError::BadManifest {
                    path: path.to_path_buf(),
                    source,
                })?;
            if manifest.config != config {
                let show = |c: &RunConfig| serde_json::to_string(c).expect("config serializes");
                return Err(BenchError::ConfigMismatch {
                    path: path.to_path_buf(),
                    was: show(&manifest.config),
                    now: show(&config),
                });
            }
            manifest
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Manifest {
            config,
            invocations: Vec::new(),
        },
        Err(e) => return Err(io(e)),
    };
    manifest.invocations.push(Invocation {
        started_at: now(),
        mqlc_version: version(),
        endpoint: Some(endpoint.to_string()),
        mode,
        samples,
    });
    let text = serde_json::to_string_pretty(&manifest).expect("manifest serializes");
    fs::write(path, text + "\n").map_err(io)
}

/// The identity of a record: a rerun skips every key already in the file.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    arm: String,
    model: String,
    sample_id: String,
    candidate: u32,
}

impl Key {
    fn of(r: &Record) -> Self {
        Self {
            arm: r.arm.clone(),
            model: r.model.clone(),
            sample_id: r.sample_id.clone(),
            candidate: r.candidate,
        }
    }
}

/// Reads every record in `path` (none if it does not exist yet). A final line
/// without its newline is what a kill mid-write leaves behind: it is cut off
/// so the sample is answered again. Any other bad line is an error.
fn load_records(path: &Path) -> Result<Vec<Record>, BenchError> {
    let io = |source| BenchError::Io {
        path: path.to_path_buf(),
        source,
    };
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io(e)),
    };
    let complete = text.rfind('\n').map_or(0, |i| i + 1);
    if complete < text.len() {
        let file = OpenOptions::new().write(true).open(path).map_err(io)?;
        file.set_len(complete as u64).map_err(io)?;
    }
    text[..complete]
        .lines()
        .enumerate()
        .map(|(i, line)| {
            serde_json::from_str(line).map_err(|source| BenchError::Corrupt {
                path: path.to_path_buf(),
                line: i + 1,
                source,
            })
        })
        .collect()
}

fn open_append(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}
