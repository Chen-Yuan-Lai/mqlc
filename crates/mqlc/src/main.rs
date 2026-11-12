use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use mqlc_core::mongodb::bson::{Bson, Document};
use mqlc_core::{
    AnswerError, AnthropicClient, Dataset, Engine, LlmClient, OpenAiClient, Sample, bench,
};

#[derive(Parser)]
#[command(version, about = "Schema-aware NL → MongoDB query compiler")]
struct Cli {
    /// AptMQL-Bench checkout (see scripts/fetch-data.sh).
    #[arg(
        long,
        env = "MQLC_DATA_DIR",
        default_value = "data/AptMQL-Bench",
        global = true
    )]
    data_dir: PathBuf,

    /// Read-only connection string (compose creates the `mqlc` user).
    #[arg(
        long,
        env = "MQLC_MONGO_URI",
        default_value = "mongodb://mqlc:mqlc@localhost:27017/?authSource=admin",
        global = true
    )]
    mongo_uri: String,

    /// Model id; defaults to claude-haiku-4-5 on the anthropic backend and
    /// is required with any other.
    #[arg(long, env = "MQLC_MODEL", global = true)]
    model: Option<String>,

    /// `openai` is any OpenAI-compatible endpoint (Ollama, vLLM,
    /// OpenRouter); it reads an optional OPENAI_API_KEY.
    #[arg(
        long,
        env = "MQLC_BACKEND",
        value_enum,
        default_value_t = BackendArg::Anthropic,
        global = true
    )]
    backend: BackendArg,

    /// Endpoint of the `openai` backend, up to `/chat/completions`.
    #[arg(
        long,
        env = "MQLC_BASE_URL",
        default_value = "http://localhost:11434/v1",
        global = true
    )]
    base_url: String,

    /// Tokens the `openai` backend's model accepts per prompt (required with
    /// it). Must match the server: Ollama truncates past its
    /// OLLAMA_CONTEXT_LENGTH without saying so.
    #[arg(long, env = "MQLC_CONTEXT_WINDOW", global = true)]
    context_window: Option<u32>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum BackendArg {
    Anthropic,
    Openai,
}

#[derive(Subcommand)]
enum Command {
    /// Answer one benchmark question and print the pipeline and its rows.
    Ask {
        /// e.g. dev_1471
        sample_id: String,

        /// How many result rows to print.
        #[arg(long, default_value_t = 20)]
        max_rows: usize,
    },
    /// Answer a batch of questions into a run directory (resumable).
    Bench {
        /// Run directory: results.jsonl and manifest.json go here. Rerunning
        /// with the same directory skips questions already answered.
        #[arg(long)]
        out: PathBuf,

        #[arg(long, value_enum, default_value_t = SplitArg::Dev)]
        split: SplitArg,

        /// Only questions on this database, e.g. restaurant.
        #[arg(long)]
        db: Option<String>,

        /// Only the first N questions of the split (in dataset order).
        #[arg(long)]
        limit: Option<usize>,

        /// `batch` submits every unanswered question as one Message Batch at
        /// half price and waits for it; interrupt at any time and rerun to
        /// collect it.
        #[arg(long, value_enum, default_value_t = ModeArg::Sync)]
        mode: ModeArg,

        /// Stop once every run next to this one (all directories under the
        /// parent of --out) has together cost this many USD.
        #[arg(long, default_value_t = 80.0)]
        spend_limit: f64,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum ModeArg {
    Sync,
    Batch,
}

/// How often a pending batch is checked.
const BATCH_POLL: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum SplitArg {
    Train,
    Dev,
    All,
}

impl SplitArg {
    fn includes(self, split: &str) -> bool {
        match self {
            Self::Train => split == "train",
            Self::Dev => split == "dev",
            Self::All => true,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // `.env` (gitignored) may hold ANTHROPIC_API_KEY and MQLC_* settings;
    // variables already set in the environment win.
    dotenvy::dotenv().ok();
    let cli = Cli::parse();
    let dataset = Dataset::open(&cli.data_dir)
        .with_context(|| format!("opening dataset at {}", cli.data_dir.display()))?;
    let mongo = mqlc_core::mongodb::Client::with_uri_str(&cli.mongo_uri).await?;
    match cli.backend {
        BackendArg::Anthropic => {
            let key = std::env::var("ANTHROPIC_API_KEY").context("ANTHROPIC_API_KEY is not set")?;
            let model = cli.model.unwrap_or_else(|| "claude-haiku-4-5".to_string());
            let llm = AnthropicClient::new(key, model);
            run(cli.command, Engine::new(llm, mongo, dataset)).await
        }
        BackendArg::Openai => {
            let window = cli
                .context_window
                .context("--context-window is required with --backend openai")?;
            let model = cli
                .model
                .context("--model is required with --backend openai")?;
            let key = std::env::var("OPENAI_API_KEY").ok();
            let llm = OpenAiClient::new(cli.base_url, key, model, window);
            run(cli.command, Engine::new(llm, mongo, dataset)).await
        }
    }
}

async fn run<L: LlmClient>(command: Command, engine: Engine<L>) -> Result<()> {
    let dataset = engine.dataset();
    match command {
        Command::Ask {
            sample_id,
            max_rows,
        } => {
            let sample = dataset
                .sample(&sample_id)
                .with_context(|| format!("no sample {sample_id}"))?
                .clone();
            let answer = engine.ask(&sample).await?;

            println!("sample:     {} ({})", sample.sample_id, sample.db_id);
            println!("question:   {}", sample.question);
            match &answer.query {
                Some(query) => {
                    println!("collection: {}", query.collection);
                    println!("pipeline:\n{}", pretty(&query.pipeline));
                }
                None => println!("reply:\n{}", answer.reply),
            }
            match &answer.rows {
                Ok(rows) => {
                    println!(
                        "rows: {} total, showing {}",
                        rows.len(),
                        rows.len().min(max_rows)
                    );
                    for row in rows.iter().take(max_rows) {
                        println!("{}", Bson::Document(row.clone()).into_relaxed_extjson());
                    }
                }
                Err(e) => println!("error: {e}"),
            }
            let u = answer.usage;
            println!(
                "tokens: in={} out={} cache_read={} cache_write={}  latency: {} ms",
                u.input_tokens,
                u.output_tokens,
                u.cache_read_input_tokens,
                u.cache_creation_input_tokens,
                answer.latency.unwrap_or_default().as_millis()
            );
            match &answer.rows {
                // Not sent, so neither right nor wrong.
                Err(AnswerError::ContextOverflow { .. }) => std::process::exit(3),
                Err(_) => std::process::exit(1),
                Ok(_) => {}
            }
        }
        Command::Bench {
            out,
            split,
            db,
            limit,
            mode,
            spend_limit,
        } => {
            let mut samples: Vec<Sample> = dataset
                .samples()
                .iter()
                .filter(|s| split.includes(&s.split))
                .filter(|s| db.as_ref().is_none_or(|db| &s.db_id == db))
                .take(limit.unwrap_or(usize::MAX))
                .cloned()
                .collect();
            // Questions on the same database share a cached prompt prefix.
            samples.sort_by(|a, b| a.db_id.cmp(&b.db_id));

            let options = bench::Options {
                mode: match mode {
                    ModeArg::Sync => bench::Mode::Sync,
                    ModeArg::Batch => bench::Mode::Batch,
                },
                spend_limit_usd: Some(spend_limit),
                batch_poll: Some(BATCH_POLL),
            };

            let total = samples.len();
            let mut seen = 0;
            let summary = bench::run(&engine, &out, &samples, &options, |progress| match progress {
                bench::Progress::Answered(r) => {
                    seen += 1;
                    let status = r.error.as_ref().map_or("ok", |e| e.kind.as_str());
                    let cost = r.cost_usd.map_or("?".to_string(), |c| format!("${c:.4}"));
                    let latency = match (r.latency_ms, r.batch) {
                        (Some(ms), _) => format!("{ms} ms"),
                        (None, true) => "batch".to_string(),
                        (None, false) => "not sent".to_string(),
                    };
                    eprintln!(
                        "[{seen}] {} {:<24} {:<11} {cost:>8}  {latency}",
                        r.sample_id, r.db_id, status
                    );
                }
                bench::Progress::BatchPending(id) => eprintln!(
                    "batch {id} is processing; checking again in {}s (safe to interrupt and rerun)",
                    BATCH_POLL.as_secs()
                ),
            })
            .await?;

            let unpriced = match summary.unpriced {
                0 => String::new(),
                n => format!(" plus {n} calls with no known price"),
            };
            eprintln!(
                "{} answered, {} already done, {} of {total} remaining; run cost so far ${:.4}{unpriced}",
                summary.completed, summary.skipped, summary.remaining, summary.cost_usd
            );
            if let Some(reason) = summary.stopped {
                eprintln!("stopped early: {reason}\nrerun the same command to continue");
                std::process::exit(2);
            }
        }
    }
    Ok(())
}

fn pretty(pipeline: &[Document]) -> String {
    let stages: Vec<_> = pipeline
        .iter()
        .map(|d| Bson::Document(d.clone()).into_relaxed_extjson())
        .collect();
    serde_json::to_string_pretty(&stages).unwrap()
}
