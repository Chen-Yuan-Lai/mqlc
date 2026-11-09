mod support;

use std::fs;
use std::path::Path;

use mongodb::bson::doc;
use mqlc_core::bench::{self, Record};
use mqlc_core::testing::{Scripted, ScriptedLlm};
use mqlc_core::{Dataset, Engine, Usage};
use support::{dataset_dir, mongo_with, reply};

fn records(run_dir: &Path) -> Vec<Record> {
    fs::read_to_string(run_dir.join("results.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[tokio::test]
async fn records_every_sample_including_ones_whose_reply_cannot_be_parsed() {
    let data = dataset_dir();
    let mongo = mongo_with(vec![
        doc! { "name": "Tom", "species": "cat" },
        doc! { "name": "Rex", "species": "dog" },
    ])
    .await;
    let llm = ScriptedLlm::new([
        reply(
            "animals",
            r#"[{"$match": {"species": "cat"}}, {"$count": "n"}]"#,
        ),
        "I am not sure.".to_string(),
    ]);
    let engine = Engine::new(
        llm,
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    );
    let run_dir = tempfile::tempdir().unwrap();
    let samples = engine.dataset().samples().to_vec();

    let summary = bench::run(&engine, run_dir.path(), &samples, |_| {})
        .await
        .unwrap();

    assert_eq!(summary.completed, 2);
    assert!(summary.stopped.is_none());
    let records = records(run_dir.path());
    let ids: Vec<_> = records.iter().map(|r| r.sample_id.as_str()).collect();
    assert_eq!(ids, ["dev_7", "dev_8"]);
    let cats = &records[0];
    assert_eq!(
        (cats.arm.as_str(), cats.model.as_str(), cats.candidate),
        ("official", "scripted", 0)
    );
    assert_eq!(cats.collection.as_deref(), Some("animals"));
    assert_eq!(
        cats.pipeline,
        Some(serde_json::json!([{"$match": {"species": "cat"}}, {"$count": "n"}]))
    );
    assert_eq!(cats.row_count, Some(1));
    assert!(cats.error.is_none());
    let unparsed = &records[1];
    assert_eq!(unparsed.error.as_ref().unwrap().kind, "parse");
    assert_eq!(unparsed.reply, "I am not sure.");
    assert!(unparsed.pipeline.is_none());
}

#[tokio::test]
async fn a_rerun_after_an_interruption_only_answers_the_missing_samples() {
    let data = dataset_dir();
    let mongo = mongo_with(vec![doc! { "name": "Tom", "species": "cat" }]).await;
    let run_dir = tempfile::tempdir().unwrap();
    let cats = reply("animals", r#"[{"$match": {"species": "cat"}}]"#);
    let dogs = reply("animals", r#"[{"$match": {"species": "dog"}}]"#);

    // The LLM fails on the second sample, as when a usage limit is hit.
    let first = Engine::new(
        ScriptedLlm::new([cats]),
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    );
    let samples = first.dataset().samples().to_vec();
    let summary = bench::run(&first, run_dir.path(), &samples, |_| {})
        .await
        .unwrap();
    assert_eq!(summary.completed, 1);
    assert!(summary.stopped.is_some());

    // A kill in the middle of a write leaves half a line behind.
    let results = run_dir.path().join("results.jsonl");
    let mut text = fs::read_to_string(&results).unwrap();
    text.push_str(r#"{"arm":"official","model":"scr"#);
    fs::write(&results, text).unwrap();

    let second = Engine::new(
        ScriptedLlm::new([dogs]),
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    );
    let summary = bench::run(&second, run_dir.path(), &samples, |_| {})
        .await
        .unwrap();

    assert_eq!((summary.completed, summary.skipped), (1, 1));
    assert!(summary.stopped.is_none());
    let asked: Vec<_> = second
        .llm()
        .requests()
        .iter()
        .map(|r| r.user.contains("dogs"))
        .collect();
    assert_eq!(asked, [true]);
    let ids: Vec<_> = records(run_dir.path())
        .into_iter()
        .map(|r| r.sample_id)
        .collect();
    assert_eq!(ids, ["dev_7", "dev_8"]);
}

#[tokio::test]
async fn writes_a_manifest_and_tracks_cost_and_refuses_to_mix_configurations() {
    let data = dataset_dir();
    let mongo = mongo_with(vec![doc! { "name": "Tom", "species": "cat" }]).await;
    let run_dir = tempfile::tempdir().unwrap();
    let usage = Usage {
        input_tokens: 1000,
        output_tokens: 100,
        cache_read_input_tokens: 2000,
        cache_creation_input_tokens: 0,
    };
    let llm = ScriptedLlm::new([
        reply("animals", r#"[{"$count": "n"}]"#),
        reply("animals", r#"[{"$count": "n"}]"#),
    ])
    .with_model("claude-haiku-4-5")
    .with_usage(usage);
    let engine = Engine::new(
        llm,
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    );
    let samples = engine.dataset().samples().to_vec();

    let summary = bench::run(&engine, run_dir.path(), &samples, |_| {})
        .await
        .unwrap();

    // Haiku 4.5: $1/M input, $5/M output, cache reads at $0.10/M.
    let per_call = 0.001 + 0.0005 + 0.0002;
    assert!(
        (summary.cost_usd - 2.0 * per_call).abs() < 1e-12,
        "{}",
        summary.cost_usd
    );
    for record in records(run_dir.path()) {
        assert_eq!(record.usage, usage);
        assert!((record.cost_usd.unwrap() - per_call).abs() < 1e-12);
    }
    let manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(run_dir.path().join("manifest.json")).unwrap())
            .unwrap();
    let config = &manifest["config"];
    assert_eq!(config["arm"], "official");
    assert_eq!(config["model"], "claude-haiku-4-5");
    assert_eq!(config["temperature"], 0.0);
    assert_eq!(config["thinking"], false);
    assert_eq!(config["knowledge_mode"], "full");
    assert_eq!(config["ir"], false);
    assert!(
        config["prompt_hash"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    assert!(manifest["invocations"][0]["mqlc_version"].is_string());

    let other = Engine::new(
        ScriptedLlm::new(Vec::<String>::new()).with_model("claude-sonnet-4-5"),
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    );
    let err = bench::run(&other, run_dir.path(), &samples, |_| {})
        .await
        .unwrap_err();
    assert!(
        matches!(err, bench::BenchError::ConfigMismatch { .. }),
        "{err}"
    );
}

#[tokio::test]
async fn a_reply_cut_off_at_max_tokens_is_recorded_and_the_run_goes_on() {
    let data = dataset_dir();
    let mongo = mongo_with(vec![doc! { "name": "Tom", "species": "cat" }]).await;
    let llm = ScriptedLlm::new([
        Scripted::Truncated("COLLECTION = \"animals\"\nAGGREGATION_PIPELINE = [{".to_string()),
        Scripted::Text(reply("animals", r#"[{"$count": "n"}]"#)),
    ]);
    let engine = Engine::new(
        llm,
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    );
    let run_dir = tempfile::tempdir().unwrap();
    let samples = engine.dataset().samples().to_vec();

    let summary = bench::run(&engine, run_dir.path(), &samples, |_| {})
        .await
        .unwrap();

    assert_eq!((summary.completed, summary.stopped), (2, None));
    let kinds: Vec<_> = records(run_dir.path())
        .into_iter()
        .map(|r| r.error.map(|e| e.kind))
        .collect();
    assert_eq!(kinds, [Some("truncated".to_string()), None]);
}

#[tokio::test]
async fn an_unreachable_database_stops_the_run_instead_of_failing_every_answer() {
    let data = dataset_dir();
    // Nothing listens on port 9; give up quickly.
    let client = mongodb::Client::with_uri_str(
        "mongodb://127.0.0.1:9/?serverSelectionTimeoutMS=300&connectTimeoutMS=300",
    )
    .await
    .unwrap();
    let llm = ScriptedLlm::new([reply("animals", r#"[{"$count": "n"}]"#)]);
    let engine = Engine::new(llm, client, Dataset::open(data.path()).unwrap());
    let run_dir = tempfile::tempdir().unwrap();
    let samples = engine.dataset().samples().to_vec();

    let summary = bench::run(&engine, run_dir.path(), &samples, |_| {})
        .await
        .unwrap();

    assert_eq!(summary.completed, 0);
    assert!(summary.stopped.unwrap().contains("database"));
    assert!(records(run_dir.path()).is_empty());
}
