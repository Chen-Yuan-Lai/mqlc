//! Experiment E: Claude Code answers through `claude -p`. A fake `claude`
//! executable replays a recorded stream-json transcript (ADR 0015); the rest
//! of the run is real, MongoDB included.

mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use mongodb::bson::doc;
use mqlc_core::bench::{self, Options, Record};
use mqlc_core::{Arm, ClaudeCodeClient, Dataset, Engine, McpServers, McpTools};
use support::{dataset_dir, mongo_with, reply};
use tempfile::TempDir;

/// A `claude` that saves its arguments, one per NUL-terminated entry, to
/// `args` and prints the next unused transcript of `transcripts`.
struct FakeClaude {
    dir: TempDir,
}

impl FakeClaude {
    fn new(transcripts: &[String]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        for (i, t) in transcripts.iter().enumerate() {
            fs::write(dir.path().join(format!("transcript{i}")), t).unwrap();
        }
        let script = format!(
            r#"#!/bin/sh
dir='{}'
n=$(ls "$dir" | grep -c '^args')
for a in "$@"; do printf '%s\0' "$a"; done > "$dir/args$n"
env > "$dir/env$n"
[ -f "$dir/transcript$n" ] || exit 3
cat "$dir/transcript$n"
"#,
            dir.path().display()
        );
        let program = dir.path().join("claude");
        fs::write(&program, script).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir }
    }

    fn program(&self) -> PathBuf {
        self.dir.path().join("claude")
    }

    /// The environment of the `n`th invocation, one `NAME=value` per line.
    fn env(&self, n: usize) -> String {
        fs::read_to_string(self.dir.path().join(format!("env{n}"))).unwrap()
    }

    /// The arguments of the `n`th invocation.
    fn args(&self, n: usize) -> Vec<String> {
        let raw = fs::read_to_string(self.dir.path().join(format!("args{n}"))).unwrap();
        raw.split_terminator('\0').map(str::to_string).collect()
    }
}

fn servers() -> McpServers {
    McpServers {
        mongo_uri: "mongodb://127.0.0.1:1/".to_string(),
        official: PathBuf::from("/opt/mongodb-mcp-server/index.js"),
        mqlc_mcp: PathBuf::from("/opt/mqlc-mcp"),
    }
}

fn client(fake: &FakeClaude, tools: McpTools) -> ClaudeCodeClient {
    ClaudeCodeClient::new(fake.program(), "claude-haiku-4-5", tools, &servers())
}

const INIT: &str = r#"{"type":"system","subtype":"init","model":"claude-haiku-4-5","tools":["mcp__mongodb__find"],"mcp_servers":[{"name":"mongodb","status":"connected"}]}"#;
const INIT_BOTH: &str = r#"{"type":"system","subtype":"init","model":"claude-haiku-4-5","tools":["mcp__mongodb__find"],"mcp_servers":[{"name":"mongodb","status":"connected"},{"name":"mqlc","status":"connected"}]}"#;

fn tool_use(name: &str) -> String {
    serde_json::json!({
        "type": "assistant",
        "message": {"content": [{"type": "tool_use", "id": "t", "name": name, "input": {}}]}
    })
    .to_string()
}

fn tool_result() -> String {
    r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t","content":"[]"}]}}"#.to_string()
}

fn result(text: &str, cost: f64) -> String {
    serde_json::json!({
        "type": "result", "subtype": "success", "is_error": false, "num_turns": 3,
        "result": text, "total_cost_usd": cost, "api_error_status": null,
        "usage": {"input_tokens": 20, "output_tokens": 300,
                  "cache_read_input_tokens": 40000, "cache_creation_input_tokens": 9000}
    })
    .to_string()
}

fn transcript(lines: &[String]) -> String {
    lines.join("\n") + "\n"
}

fn records(run_dir: &Path) -> Vec<Record> {
    fs::read_to_string(run_dir.join("results.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[tokio::test]
async fn records_the_final_reply_its_tool_calls_and_what_claude_code_says_it_cost() {
    let data = dataset_dir();
    let mongo = mongo_with(vec![
        doc! { "name": "Tom", "species": "cat" },
        doc! { "name": "Rex", "species": "dog" },
    ])
    .await;
    let cats = reply(
        "animals",
        r#"[{"$match": {"species": "cat"}}, {"$count": "n"}]"#,
    );
    let fake = FakeClaude::new(&[transcript(&[
        INIT.to_string(),
        tool_use("mcp__mongodb__list-collections"),
        tool_result(),
        tool_use("mcp__mongodb__collection-schema"),
        tool_result(),
        tool_use("mcp__mongodb__collection-schema"),
        tool_result(),
        result(&cats, 0.0644),
    ])]);
    let engine = Engine::new(
        client(&fake, McpTools::Official),
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    )
    .with_arm(Arm::Mcp(McpTools::Official));
    let run_dir = tempfile::tempdir().unwrap();
    let samples = vec![engine.dataset().sample("dev_7").unwrap().clone()];

    let summary = bench::run(
        &engine,
        run_dir.path(),
        &samples,
        &Options::default(),
        |_| {},
    )
    .await
    .unwrap();

    assert!(summary.stopped.is_none(), "{:?}", summary.stopped);
    let records = records(run_dir.path());
    let r = &records[0];
    assert_eq!(
        (r.arm.as_str(), r.model.as_str()),
        ("mcp-official", "claude-haiku-4-5")
    );
    assert_eq!(r.collection.as_deref(), Some("animals"));
    assert_eq!(r.row_count, Some(1));
    assert!(r.error.is_none());
    assert_eq!(r.reply, cats);
    assert_eq!(
        r.tool_calls,
        [
            ("mcp__mongodb__collection-schema".to_string(), 2),
            ("mcp__mongodb__list-collections".to_string(), 1),
        ]
        .into()
    );
    assert_eq!(r.cost_usd, Some(0.0644));
    assert_eq!(r.usage.cache_read_input_tokens, 40000);
    assert!(r.latency_ms.is_some());
}

/// Runs `samples` of the test dataset through `fake` and returns the summary
/// and records.
async fn run_with(
    fake: &FakeClaude,
    tools: McpTools,
    sample_ids: &[&str],
) -> (bench::Summary, Vec<Record>) {
    let data = dataset_dir();
    let mongo = mongo_with(vec![doc! { "name": "Tom", "species": "cat" }]).await;
    let engine = Engine::new(
        client(fake, tools),
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    )
    .with_arm(Arm::Mcp(tools));
    let samples: Vec<_> = sample_ids
        .iter()
        .map(|id| engine.dataset().sample(id).unwrap().clone())
        .collect();
    let run_dir = tempfile::tempdir().unwrap();
    let summary = bench::run(
        &engine,
        run_dir.path(),
        &samples,
        &Options::default(),
        |_| {},
    )
    .await
    .unwrap();
    let path = run_dir.path().join("results.jsonl");
    let records = if path.exists() {
        records(run_dir.path())
    } else {
        Vec::new()
    };
    (summary, records)
}

fn answered() -> String {
    transcript(&[
        INIT.to_string(),
        result(
            &reply("animals", r#"[{"$match": {"species": "cat"}}]"#),
            0.01,
        ),
    ])
}

/// The value after `flag` in `args`.
fn flag<'a>(args: &'a [String], flag: &str) -> &'a str {
    let i = args.iter().position(|a| a == flag).unwrap();
    &args[i + 1]
}

#[tokio::test]
async fn asks_with_the_official_system_prompt_and_only_the_arms_mcp_servers() {
    let both_answered = transcript(&[
        INIT_BOTH.to_string(),
        result(
            &reply("animals", r#"[{"$match": {"species": "cat"}}]"#),
            0.01,
        ),
    ]);
    let fake = FakeClaude::new(&[answered(), both_answered]);
    // Claude Code logs in on its own: an API key in the environment (mqlc
    // reads one from `.env`) would bill it instead.
    // SAFETY: no other test in this binary reads the variable.
    unsafe { std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-test") };

    run_with(&fake, McpTools::Official, &["dev_7"]).await;
    run_with(&fake, McpTools::OfficialMqlc, &["dev_7"]).await;

    let official = fake.args(0);
    assert_eq!(flag(&official, "--model"), "claude-haiku-4-5");
    assert!(flag(&official, "--append-system-prompt").starts_with("# ROLE"));
    assert_eq!(flag(&official, "--tools"), "");
    assert_eq!(flag(&official, "--setting-sources"), "");
    assert!(official.contains(&"--strict-mcp-config".to_string()));
    assert_eq!(flag(&official, "--allowedTools"), "mcp__mongodb");
    let config: serde_json::Value = serde_json::from_str(flag(&official, "--mcp-config")).unwrap();
    let servers = config["mcpServers"].as_object().unwrap();
    assert_eq!(servers.keys().collect::<Vec<_>>(), ["mongodb"]);
    assert_eq!(
        servers["mongodb"]["args"],
        serde_json::json!(["/opt/mongodb-mcp-server/index.js", "--readOnly"])
    );
    assert_eq!(
        servers["mongodb"]["env"]["MDB_MCP_CONNECTION_STRING"],
        "mongodb://127.0.0.1:1/"
    );

    assert!(!fake.env(0).contains("ANTHROPIC_API_KEY"));

    let both = fake.args(1);
    assert_eq!(flag(&both, "--allowedTools"), "mcp__mongodb,mcp__mqlc");
    let config: serde_json::Value = serde_json::from_str(flag(&both, "--mcp-config")).unwrap();
    assert_eq!(
        config["mcpServers"]["mqlc"],
        serde_json::json!({"command": "/opt/mqlc-mcp", "args": ["mongodb://127.0.0.1:1/"]})
    );
}

#[tokio::test]
async fn the_prompt_names_the_database_and_the_question_but_no_schema() {
    let fake = FakeClaude::new(&[answered()]);
    let data = dataset_dir();
    let mongo = mongo_with(vec![doc! { "name": "Tom" }]).await;
    let engine = Engine::new(
        client(&fake, McpTools::Official),
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    )
    .with_arm(Arm::Mcp(McpTools::Official));
    let sample = engine.dataset().sample("dev_7").unwrap().clone();

    let request = engine.request(&sample).unwrap();

    assert_eq!(
        format!("{}\n\n{}", request.context, request.user),
        "The MongoDB Schema Description is not given: look up the collections and fields of \
         the database `pets` with your tools.\n\n\
         <Natural Language Request>\nHow many cats are there?\n</Natural Language Request>\n"
    );
}

#[tokio::test]
async fn running_out_of_turns_is_recorded_as_a_wrong_answer() {
    let out_of_turns = transcript(&[
        INIT.to_string(),
        tool_use("mcp__mongodb__find"),
        tool_result(),
        serde_json::json!({
            "type": "result", "subtype": "error_max_turns", "is_error": true, "num_turns": 31,
            "total_cost_usd": 0.2, "errors": ["Reached maximum number of turns (30)"],
            "usage": {"input_tokens": 5, "output_tokens": 50}
        })
        .to_string(),
    ]);
    let fake = FakeClaude::new(&[out_of_turns]);

    let (summary, records) = run_with(&fake, McpTools::Official, &["dev_7"]).await;

    assert!(summary.stopped.is_none(), "{:?}", summary.stopped);
    let error = records[0].error.as_ref().unwrap();
    assert_eq!(error.kind, "max_turns");
    assert!(records[0].pipeline.is_none());
    assert_eq!(records[0].cost_usd, Some(0.2));
    assert_eq!(records[0].tool_calls["mcp__mongodb__find"], 1);
}

#[tokio::test]
async fn infrastructure_failures_stop_the_run_without_a_record() {
    let mcp_failed = transcript(&[
        r#"{"type":"system","subtype":"init","mcp_servers":[{"name":"mongodb","status":"failed"}]}"#
            .to_string(),
        result("I cannot reach the database.", 0.01),
    ]);
    let api_error = transcript(&[
        INIT.to_string(),
        serde_json::json!({
            "type": "result", "subtype": "success", "is_error": true, "num_turns": 1,
            "result": "API Error: 429 rate limited", "api_error_status": 429,
            "total_cost_usd": 0.0
        })
        .to_string(),
    ]);
    for (why, tools, transcripts, expected) in [
        (
            "MCP server down",
            McpTools::Official,
            vec![mcp_failed],
            "MCP server mongodb is failed",
        ),
        (
            "MCP server missing",
            McpTools::OfficialMqlc,
            vec![answered()],
            "MCP server mqlc is missing",
        ),
        ("API error", McpTools::Official, vec![api_error], "429"),
        // The fake exits 3 with no output, as claude does when it cannot start.
        ("no result", McpTools::Official, vec![], "no result"),
    ] {
        let fake = FakeClaude::new(&transcripts);

        let (summary, records) = run_with(&fake, tools, &["dev_7", "dev_8"]).await;

        let stopped = summary.stopped.unwrap_or_default();
        assert!(stopped.contains(expected), "{why}: {stopped}");
        assert!(records.is_empty(), "{why}");
        assert_eq!(summary.remaining, 2, "{why}");
    }
}

#[tokio::test]
async fn a_host_still_running_after_the_timeout_stops_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let program = dir.path().join("claude");
    fs::write(&program, "#!/bin/sh\nsleep 30\n").unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    let data = dataset_dir();
    let mongo = mongo_with(vec![doc! { "name": "Tom" }]).await;
    let llm = ClaudeCodeClient::new(&program, "claude-haiku-4-5", McpTools::Official, &servers())
        .with_timeout(std::time::Duration::from_millis(500));
    let engine = Engine::new(
        llm,
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    )
    .with_arm(Arm::Mcp(McpTools::Official));
    let samples = vec![engine.dataset().sample("dev_7").unwrap().clone()];
    let run_dir = tempfile::tempdir().unwrap();

    let summary = bench::run(
        &engine,
        run_dir.path(),
        &samples,
        &Options::default(),
        |_| {},
    )
    .await
    .unwrap();

    assert!(summary.stopped.unwrap().contains("still running"));
    assert_eq!(summary.completed, 0);
}
