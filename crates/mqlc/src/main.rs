use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use mqlc_core::engine;
use mqlc_core::mongodb::bson::{Bson, Document};
use mqlc_core::profile::{self, ProfileOptions};
use mqlc_core::{
    AnswerError, AnthropicClient, Arm, ClaudeCodeClient, Dataset, Engine, LlmClient, McpServers,
    McpTools, OpenAiClient, Repair, Sample, bench, ir,
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

    /// Model id; defaults to claude-haiku-4-5 on the anthropic and
    /// claude-code backends and is required with openai.
    #[arg(long, env = "MQLC_MODEL", global = true)]
    model: Option<String>,

    /// `openai` is any OpenAI-compatible endpoint (Ollama, vLLM,
    /// OpenRouter); it reads an optional OPENAI_API_KEY. `claude-code` runs
    /// `claude -p` with MCP tools for each question (experiment E, an
    /// `mcp-*` arm): it uses Claude Code's login, not ANTHROPIC_API_KEY.
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

    /// The Claude Code executable of the `claude-code` backend.
    #[arg(long, default_value = "claude", global = true)]
    claude: PathBuf,

    /// Entry point of the pinned official MongoDB MCP server
    /// (`scripts/install-mcp.sh` installs it).
    #[arg(
        long,
        default_value = "tools/mongodb-mcp/node_modules/mongodb-mcp-server/dist/esm/index.js",
        global = true
    )]
    official_mcp: PathBuf,

    /// The `mqlc-mcp` binary (`cargo build --release -p mqlc-mcp`).
    #[arg(long, default_value = "target/release/mqlc-mcp", global = true)]
    mqlc_mcp: PathBuf,

    /// Knowledge in the prompt. Experiment A: the official schema
    /// description, the auto MDL that `mqlc profile` wrote to --mdl-dir in
    /// its place (`auto-mdl`) or merged into it field by field
    /// (`official-auto-mdl`), or the official description with each
    /// question's mongodb_evidence (`official-evidence`). `human-mdl` and
    /// `official-human-mdl` do the same with the human MDL in
    /// --human-mdl-dir, on train databases only. Diagnostics: the official
    /// description with each field rewritten to the keys the official prompt
    /// names (`official-prompt-keys`) or with each collection in a tag
    /// (`official-tagged`).
    #[arg(long, value_enum, default_value_t = ArmArg::Official, global = true)]
    arm: ArmArg,

    /// Experiment B: feed errors back to the model for another reply, up to
    /// twice for each kind. `execution` repairs execution errors (the
    /// control arm); `static-check` first checks each pipeline against the
    /// official schema description before it runs and repairs what the check
    /// finds, then repairs execution errors too (the IR arm). Recorded in the
    /// run manifest.
    #[arg(long, value_enum, default_value_t = RepairArg::Off, global = true)]
    repair: RepairArg,

    /// Once a pipeline runs, check that each string its $match stages
    /// require a field to equal is in the data; when one is not, feed the
    /// closest values back for another reply, up to twice. Independent of
    /// --repair; recorded in the run manifest.
    #[arg(long, global = true)]
    value_correction: bool,

    /// Sampling temperature, recorded in the run manifest.
    #[arg(long, default_value_t = engine::DEFAULT_TEMPERATURE, global = true)]
    temperature: f32,

    /// Send the system prompt at the start of the user turn instead of as
    /// the system prompt.
    #[arg(long, global = true)]
    single_message: bool,

    /// Turn on extended thinking with this many tokens, at least 1024
    /// (Anthropic only; the API requires --temperature 1 with it).
    #[arg(long, global = true)]
    thinking_budget: Option<u32>,

    /// Auto MDL projects, one directory per database (`mqlc profile --out`).
    #[arg(long, env = "MQLC_MDL_DIR", default_value = "mdl/auto", global = true)]
    mdl_dir: PathBuf,

    /// Human MDL projects, one directory per train database: auto MDL a
    /// person added descriptions, formulas and instructions to.
    #[arg(
        long,
        env = "MQLC_HUMAN_MDL_DIR",
        default_value = "mdl/human",
        global = true
    )]
    human_mdl_dir: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum ArmArg {
    Official,
    OfficialPromptKeys,
    OfficialTagged,
    AutoMdl,
    OfficialAutoMdl,
    OfficialEvidence,
    HumanMdl,
    OfficialHumanMdl,
    McpOfficial,
    McpOfficialMqlc,
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum RepairArg {
    Off,
    Execution,
    StaticCheck,
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum BackendArg {
    Anthropic,
    Openai,
    ClaudeCode,
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

        /// Only N questions drawn at random (with --seed) from those --split
        /// and --db select, before --limit. Recorded in the run manifest.
        #[arg(long, value_name = "N")]
        draw: Option<usize>,

        /// Seed of --draw; the same seed draws the same questions.
        #[arg(long, default_value_t = 0)]
        seed: u64,

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
    /// Profile databases into auto MDL projects under --mdl-dir, one
    /// directory per database. Calls no LLM; rerunning overwrites them.
    Profile {
        #[arg(long, value_enum, default_value_t = SplitArg::All)]
        split: SplitArg,

        /// Only this database, e.g. restaurant.
        #[arg(long)]
        db: Option<String>,

        /// Documents read per collection, the first ones in `_id` order.
        #[arg(long, default_value_t = ProfileOptions::default().sample_size)]
        sample_size: usize,

        /// A sensitive field to leave out, as <db>.<collection>.<path>, with
        /// everything under it: its values are never read, and the MDL keeps
        /// only its path. Repeatable; naming a field that is not there is an
        /// error.
        #[arg(long, value_name = "DB.COLLECTION.PATH")]
        exclude: Vec<String>,
    },
    /// Report, per split, how many gold pipelines the IR represents without a
    /// raw stage, and what the rest need. Calls no LLM and no database.
    IrCoverage,
    /// Run the static check on gold pipelines against the official schema
    /// description: everything it reports is a false positive. Calls no LLM
    /// and no database.
    /// Train only: tuning the check on dev would leak (ADR 0006).
    StaticCheck {
        /// Only questions on this train database, e.g. restaurant.
        #[arg(long)]
        db: Option<String>,
    },
    /// Feed a run's recorded replies to the engine as --repair,
    /// --value-correction and --arm now set it, and list the candidates it
    /// would give feedback to, with how they scored (`score.py`). Calls no LLM.
    /// Takes runs without repairs, of train only (ADR 0006).
    Replay {
        /// A run directory written by `mqlc bench`; only read.
        run_dir: PathBuf,

        /// Only questions on this train database, e.g. restaurant.
        #[arg(long)]
        db: Option<String>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum ModeArg {
    Sync,
    Batch,
}

/// The model of the anthropic and claude-code backends unless --model says
/// otherwise.
const DEFAULT_MODEL: &str = "claude-haiku-4-5";

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
    if let Command::IrCoverage = cli.command {
        print!("{}", ir::Coverage::of(dataset.samples())?);
        return Ok(());
    }
    if let Command::StaticCheck { db } = &cli.command {
        print!("{}", ir::GoldCheck::of(&dataset, db.as_deref())?);
        return Ok(());
    }
    let mongo = mqlc_core::mongodb::Client::with_uri_str(&cli.mongo_uri).await?;
    if let Command::Profile {
        split,
        db,
        sample_size,
        exclude,
    } = &cli.command
    {
        let mut db_ids: Vec<&str> = dataset
            .samples()
            .iter()
            .filter(|s| split.includes(&s.split))
            .filter(|s| db.as_ref().is_none_or(|db| &s.db_id == db))
            .map(|s| s.db_id.as_str())
            .collect();
        db_ids.sort();
        db_ids.dedup();
        anyhow::ensure!(!db_ids.is_empty(), "no database matches");
        let mut excluded: HashMap<&str, Vec<String>> = HashMap::new();
        for e in exclude {
            let (db_id, field) = e
                .split_once('.')
                .with_context(|| format!("--exclude {e}: expected <db>.<collection>.<path>"))?;
            anyhow::ensure!(
                dataset.samples().iter().any(|s| s.db_id == db_id),
                "--exclude {e}: no database {db_id}"
            );
            excluded.entry(db_id).or_default().push(field.to_string());
        }
        for db_id in db_ids {
            let options = ProfileOptions {
                sample_size: *sample_size,
                exclude: excluded.get(db_id).cloned().unwrap_or_default(),
            };
            let project = profile::profile(&mongo, db_id, &options)
                .await
                .with_context(|| format!("profiling {db_id}"))?;
            let dir = cli.mdl_dir.join(db_id);
            project.save(&dir)?;
            let columns: usize = project.mdl.models.iter().map(|m| m.columns.len()).sum();
            eprintln!(
                "{db_id:<24} {:>3} collections {columns:>5} fields  ~{} prompt tokens  -> {}",
                project.mdl.models.len(),
                engine::estimate_tokens(&project.render()),
                dir.display()
            );
        }
        return Ok(());
    }
    let arm = match cli.arm {
        ArmArg::Official => Arm::Official,
        ArmArg::OfficialPromptKeys => Arm::OfficialPromptKeys,
        ArmArg::OfficialTagged => Arm::OfficialTagged,
        ArmArg::AutoMdl => Arm::AutoMdl {
            dir: cli.mdl_dir.clone(),
        },
        ArmArg::OfficialAutoMdl => Arm::OfficialAutoMdl {
            dir: cli.mdl_dir.clone(),
        },
        ArmArg::OfficialEvidence => Arm::OfficialEvidence,
        ArmArg::HumanMdl => Arm::HumanMdl {
            dir: cli.human_mdl_dir.clone(),
        },
        ArmArg::OfficialHumanMdl => Arm::OfficialHumanMdl {
            dir: cli.human_mdl_dir.clone(),
        },
        ArmArg::McpOfficial => Arm::Mcp(McpTools::Official),
        ArmArg::McpOfficialMqlc => Arm::Mcp(McpTools::OfficialMqlc),
    };
    let repair = match cli.repair {
        RepairArg::Off => Repair::Off,
        RepairArg::Execution => Repair::Execution,
        RepairArg::StaticCheck => Repair::StaticCheck,
    };
    if let Command::Replay { run_dir, db } = &cli.command {
        let samples: Vec<Sample> = dataset
            .samples()
            .iter()
            .filter(|s| s.split == "train")
            .filter(|s| db.as_ref().is_none_or(|db| &s.db_id == db))
            .cloned()
            .collect();
        anyhow::ensure!(!samples.is_empty(), "no train question matches");
        let engine = Engine::new(bench::ReplayLlm, mongo, dataset)
            .with_arm(arm)
            .with_repair(repair)
            .with_value_correction(cli.value_correction);
        let replay = bench::replay(&engine, run_dir, &samples).await?;
        anyhow::ensure!(
            !replay.candidates.is_empty(),
            "no train candidate in {}: replay takes train runs only",
            run_dir.display()
        );
        print!("{replay}");
        return Ok(());
    }
    if let Some(budget) = cli.thinking_budget {
        anyhow::ensure!(
            budget >= 1024,
            "--thinking-budget must be at least 1024 (the API's minimum)"
        );
        anyhow::ensure!(
            cli.backend == BackendArg::Anthropic,
            "--thinking-budget needs --backend anthropic"
        );
        anyhow::ensure!(
            cli.temperature == 1.0,
            "--thinking-budget needs --temperature 1 (the API requires it with thinking)"
        );
    }
    let mcp_tools = match arm {
        Arm::Mcp(tools) => Some(tools),
        _ => None,
    };
    anyhow::ensure!(
        mcp_tools.is_some() == (cli.backend == BackendArg::ClaudeCode),
        "the mcp-* arms go with --backend claude-code, and it takes only them"
    );
    if cli.backend == BackendArg::ClaudeCode {
        // Claude Code sets these itself (ADR 0015).
        anyhow::ensure!(
            repair == Repair::Off && !cli.value_correction,
            "--backend claude-code answers in one session: no --repair or --value-correction"
        );
        anyhow::ensure!(
            cli.thinking_budget.is_none()
                && cli.temperature == engine::DEFAULT_TEMPERATURE
                && !cli.single_message,
            "--backend claude-code takes no --temperature, --thinking-budget or --single-message"
        );
        if let Command::Bench { mode, .. } = &cli.command {
            anyhow::ensure!(
                *mode == ModeArg::Sync,
                "--backend claude-code has no batch mode"
            );
        }
    }
    match cli.backend {
        BackendArg::ClaudeCode => {
            let tools = mcp_tools.expect("checked above");
            anyhow::ensure!(
                cli.official_mcp.exists(),
                "{} not found: run scripts/install-mcp.sh",
                cli.official_mcp.display()
            );
            anyhow::ensure!(
                tools == McpTools::Official || cli.mqlc_mcp.exists(),
                "{} not found: run cargo build --release -p mqlc-mcp",
                cli.mqlc_mcp.display()
            );
            let servers = McpServers {
                mongo_uri: cli.mongo_uri.clone(),
                official: std::path::absolute(&cli.official_mcp)?,
                mqlc_mcp: std::path::absolute(&cli.mqlc_mcp)?,
            };
            let model = cli.model.unwrap_or_else(|| DEFAULT_MODEL.to_string());
            let llm = ClaudeCodeClient::new(cli.claude, model, tools, &servers);
            run(cli.command, Engine::new(llm, mongo, dataset).with_arm(arm)).await
        }
        BackendArg::Anthropic => {
            let key = std::env::var("ANTHROPIC_API_KEY").context("ANTHROPIC_API_KEY is not set")?;
            let model = cli.model.unwrap_or_else(|| DEFAULT_MODEL.to_string());
            let llm = AnthropicClient::new(key, model);
            run(
                cli.command,
                Engine::new(llm, mongo, dataset)
                    .with_arm(arm)
                    .with_temperature(cli.temperature)
                    .with_single_message(cli.single_message)
                    .with_thinking_budget(cli.thinking_budget)
                    .with_repair(repair)
                    .with_value_correction(cli.value_correction),
            )
            .await
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
            run(
                cli.command,
                Engine::new(llm, mongo, dataset)
                    .with_arm(arm)
                    .with_temperature(cli.temperature)
                    .with_single_message(cli.single_message)
                    .with_thinking_budget(cli.thinking_budget)
                    .with_repair(repair)
                    .with_value_correction(cli.value_correction),
            )
            .await
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
            for (i, r) in answer.repairs.iter().enumerate() {
                println!("repair {} ({}):", i + 1, r.kind.name());
                for p in &r.problems {
                    println!("  {}: {}", p.kind, p.message);
                }
            }
            for p in &answer.check_problems {
                println!("still failing the static check: {}: {}", p.kind, p.message);
            }
            let u = answer.total_usage();
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
            draw,
            seed,
            mode,
            spend_limit,
        } => {
            let mut samples: Vec<Sample> = dataset
                .samples()
                .iter()
                .filter(|s| split.includes(&s.split))
                .filter(|s| db.as_ref().is_none_or(|db| &s.db_id == db))
                .cloned()
                .collect();
            let draw = draw.map(|size| bench::Draw { size, seed });
            if let Some(draw) = draw {
                samples = draw.apply(samples);
            }
            samples.truncate(limit.unwrap_or(usize::MAX));
            // Questions on the same database share a cached prompt prefix.
            samples.sort_by(|a, b| a.db_id.cmp(&b.db_id));

            let options = bench::Options {
                mode: match mode {
                    ModeArg::Sync => bench::Mode::Sync,
                    ModeArg::Batch => bench::Mode::Batch,
                },
                spend_limit_usd: Some(spend_limit),
                batch_poll: Some(BATCH_POLL),
                draw,
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
        Command::IrCoverage
        | Command::StaticCheck { .. }
        | Command::Profile { .. }
        | Command::Replay { .. } => {
            unreachable!("handled before choosing a backend")
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
