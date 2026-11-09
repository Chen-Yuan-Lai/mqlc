//! `mqlc bench`: answer a batch of samples and append one record per answer
//! to `results.jsonl` in a run directory.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use mongodb::bson::Bson;
use serde::{Deserialize, Serialize};

use crate::dataset::Sample;
use crate::engine::{Engine, Settings};
use crate::llm::{LlmClient, Usage};
use crate::pricing;

const RESULTS: &str = "results.jsonl";
const MANIFEST: &str = "manifest.json";
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
    pub latency_ms: u64,
    pub cost_usd: Option<f64>,
    pub reply: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordError {
    /// `parse`, `write_stage` or `execution`.
    pub kind: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    /// Answered in this invocation.
    pub completed: usize,
    /// Already answered by an earlier invocation.
    pub skipped: usize,
    /// Why the run stopped early, if it did.
    pub stopped: Option<String>,
    /// Cost of every record in the run so far, earlier invocations included.
    pub cost_usd: f64,
    /// Records in the run whose model has no known price, so are not in
    /// `cost_usd`.
    pub unpriced: usize,
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
    #[serde(flatten)]
    pub settings: Settings,
    pub prompt_hash: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Invocation {
    pub started_at: String,
    pub mqlc_version: String,
    pub samples: usize,
}

/// `mqlc-core` version and the git commit it was built from.
pub fn version() -> String {
    format!(
        "{}+{}",
        env!("CARGO_PKG_VERSION"),
        option_env!("MQLC_GIT_SHA").unwrap_or("unknown")
    )
}

pub async fn run<L: LlmClient>(
    engine: &Engine<L>,
    run_dir: &Path,
    samples: &[Sample],
    mut on_record: impl FnMut(&Record),
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
            settings: engine.settings(),
            prompt_hash: engine.dataset().prompt().hash().to_string(),
        },
        samples.len(),
    )?;
    let results_path = run_dir.join(RESULTS);
    let earlier = load_records(&results_path)?;
    let mut cost_usd: f64 = earlier.iter().filter_map(|r| r.cost_usd).sum();
    let mut unpriced = earlier.iter().filter(|r| r.cost_usd.is_none()).count();
    let done: HashSet<Key> = earlier.iter().map(Key::of).collect();
    let mut results = open_append(&results_path).map_err(io)?;

    let (mut completed, mut skipped) = (0, 0);
    for sample in samples {
        let key = Key {
            arm: ARM.to_string(),
            model: model.clone(),
            sample_id: sample.sample_id.clone(),
            candidate: 0,
        };
        if done.contains(&key) {
            skipped += 1;
            continue;
        }
        let answer = match engine.ask(sample).await {
            Ok(answer) => answer,
            // No answer to record: the API or the database is unavailable, or
            // the dataset is broken. A rerun picks up from this sample.
            Err(e) => {
                return Ok(Summary {
                    completed,
                    skipped,
                    stopped: Some(e.to_string()),
                    cost_usd,
                    unpriced,
                });
            }
        };
        let record = Record {
            arm: ARM.to_string(),
            model: model.clone(),
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
            latency_ms: answer.latency.as_millis() as u64,
            cost_usd: pricing::cost_usd(&model, &answer.usage),
            reply: answer.reply,
        };
        // One write per line, flushed, so a kill loses at most the line in flight.
        let mut line = serde_json::to_string(&record).expect("records serialize");
        line.push('\n');
        results.write_all(line.as_bytes()).map_err(io)?;
        results.flush().map_err(io)?;
        match record.cost_usd {
            Some(c) => cost_usd += c,
            None => unpriced += 1,
        }
        on_record(&record);
        completed += 1;
    }
    Ok(Summary {
        completed,
        skipped,
        stopped: None,
        cost_usd,
        unpriced,
    })
}

/// Creates the manifest, or checks that `config` matches the one the run was
/// started with, and appends this invocation.
fn record_invocation(path: &Path, config: RunConfig, samples: usize) -> Result<(), BenchError> {
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
        started_at: mongodb::bson::DateTime::now()
            .try_to_rfc3339_string()
            .expect("now is representable"),
        mqlc_version: version(),
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
