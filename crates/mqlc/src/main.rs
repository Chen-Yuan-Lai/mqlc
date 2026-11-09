use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use mqlc_core::mongodb::bson::{Bson, Document};
use mqlc_core::{AnthropicClient, Dataset, Engine, Sample, bench};

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

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Answer one benchmark question and print the pipeline and its rows.
    Ask {
        /// e.g. dev_1471
        sample_id: String,

        #[arg(long, env = "MQLC_MODEL", default_value = "claude-haiku-4-5")]
        model: String,

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

        /// Only the first N questions of the split (in dataset order).
        #[arg(long)]
        limit: Option<usize>,

        #[arg(long, env = "MQLC_MODEL", default_value = "claude-haiku-4-5")]
        model: String,
    },
}

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
    let cli = Cli::parse();
    let dataset = Dataset::open(&cli.data_dir)
        .with_context(|| format!("opening dataset at {}", cli.data_dir.display()))?;
    let mongo = mqlc_core::mongodb::Client::with_uri_str(&cli.mongo_uri).await?;
    let api_key = || std::env::var("ANTHROPIC_API_KEY").context("ANTHROPIC_API_KEY is not set");
    match cli.command {
        Command::Ask {
            sample_id,
            model,
            max_rows,
        } => {
            let sample = dataset
                .sample(&sample_id)
                .with_context(|| format!("no sample {sample_id}"))?
                .clone();
            let engine = Engine::new(AnthropicClient::new(api_key()?, model), mongo, dataset);

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
                answer.latency.as_millis()
            );
            if answer.rows.is_err() {
                std::process::exit(1);
            }
        }
        Command::Bench {
            out,
            split,
            limit,
            model,
        } => {
            let mut samples: Vec<Sample> = dataset
                .samples()
                .iter()
                .filter(|s| split.includes(&s.split))
                .take(limit.unwrap_or(usize::MAX))
                .cloned()
                .collect();
            // Questions on the same database share a cached prompt prefix.
            samples.sort_by(|a, b| a.db_id.cmp(&b.db_id));
            let engine = Engine::new(AnthropicClient::new(api_key()?, model), mongo, dataset);

            let total = samples.len();
            let mut seen = 0;
            let summary = bench::run(&engine, &out, &samples, |r| {
                seen += 1;
                let status = r.error.as_ref().map_or("ok", |e| e.kind.as_str());
                let cost = r.cost_usd.map_or("?".to_string(), |c| format!("${c:.4}"));
                eprintln!(
                    "[{seen}] {} {:<24} {:<11} {cost:>8}  {} ms",
                    r.sample_id, r.db_id, status, r.latency_ms
                );
            })
            .await?;

            let unpriced = match summary.unpriced {
                0 => String::new(),
                n => format!(" plus {n} calls with no known price"),
            };
            eprintln!(
                "{} answered, {} already done, {} of {total} remaining; run cost so far ${:.4}{unpriced}",
                summary.completed,
                summary.skipped,
                total - summary.completed - summary.skipped,
                summary.cost_usd
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
