use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use mqlc_core::mongodb::bson::{Bson, Document};
use mqlc_core::{AnthropicClient, Dataset, Engine};

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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Ask {
            sample_id,
            model,
            max_rows,
        } => {
            let dataset = Dataset::open(&cli.data_dir)
                .with_context(|| format!("opening dataset at {}", cli.data_dir.display()))?;
            let sample = dataset
                .sample(&sample_id)
                .with_context(|| format!("no sample {sample_id}"))?
                .clone();
            let api_key =
                std::env::var("ANTHROPIC_API_KEY").context("ANTHROPIC_API_KEY is not set")?;
            let mongo = mqlc_core::mongodb::Client::with_uri_str(&cli.mongo_uri).await?;
            let engine = Engine::new(AnthropicClient::new(api_key, model), mongo, dataset);

            let answer = engine.ask(&sample).await?;

            println!("sample:     {} ({})", sample.sample_id, sample.db_id);
            println!("question:   {}", sample.question);
            println!("collection: {}", answer.collection);
            println!("pipeline:\n{}", pretty(&answer.pipeline));
            println!(
                "rows: {} total, showing {}",
                answer.rows.len(),
                answer.rows.len().min(max_rows)
            );
            for row in answer.rows.iter().take(max_rows) {
                println!("{}", Bson::Document(row.clone()).into_relaxed_extjson());
            }
            let u = answer.usage;
            println!(
                "tokens: in={} out={} cache_read={} cache_write={}",
                u.input_tokens,
                u.output_tokens,
                u.cache_read_input_tokens,
                u.cache_creation_input_tokens
            );
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
