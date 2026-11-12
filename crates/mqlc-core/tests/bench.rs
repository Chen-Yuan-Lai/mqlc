mod support;

use std::fs;
use std::path::Path;

use mongodb::bson::doc;
use mqlc_core::bench::{self, Mode, Options, Progress, Record};
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

    let summary = bench::run(
        &engine,
        run_dir.path(),
        &samples,
        &Options::default(),
        |_| {},
    )
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
    let summary = bench::run(
        &first,
        run_dir.path(),
        &samples,
        &Options::default(),
        |_| {},
    )
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
    let summary = bench::run(
        &second,
        run_dir.path(),
        &samples,
        &Options::default(),
        |_| {},
    )
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

    let summary = bench::run(
        &engine,
        run_dir.path(),
        &samples,
        &Options::default(),
        |_| {},
    )
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
    let err = bench::run(
        &other,
        run_dir.path(),
        &samples,
        &Options::default(),
        |_| {},
    )
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

    let summary = bench::run(
        &engine,
        run_dir.path(),
        &samples,
        &Options::default(),
        |_| {},
    )
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

    let summary = bench::run(
        &engine,
        run_dir.path(),
        &samples,
        &Options::default(),
        |_| {},
    )
    .await
    .unwrap();

    assert_eq!(summary.completed, 0);
    assert!(summary.stopped.unwrap().contains("database"));
    assert!(records(run_dir.path()).is_empty());
}

#[tokio::test]
async fn a_batch_submitted_before_an_interruption_is_collected_on_rerun_without_resubmitting() {
    let data = dataset_dir();
    let mongo = mongo_with(vec![
        doc! { "name": "Tom", "species": "cat" },
        doc! { "name": "Rex", "species": "dog" },
    ])
    .await;
    let runs = tempfile::tempdir().unwrap();
    let run_dir = runs.path().join("batch");
    let usage = Usage {
        input_tokens: 1000,
        output_tokens: 100,
        cache_read_input_tokens: 2000,
        cache_creation_input_tokens: 0,
    };
    // One fake API shared by every invocation, as the real batch outlives them.
    let api = ScriptedLlm::new([
        reply(
            "animals",
            r#"[{"$match": {"species": "cat"}}, {"$count": "n"}]"#,
        ),
        reply(
            "animals",
            r#"[{"$match": {"species": "dog"}}, {"$count": "n"}]"#,
        ),
    ])
    .with_model("claude-haiku-4-5")
    .with_usage(usage);
    let engine = || {
        Engine::new(
            api.clone(),
            mongo.client.clone(),
            Dataset::open(data.path()).unwrap(),
        )
    };
    let samples = engine().dataset().samples().to_vec();
    let batch = Options {
        mode: Mode::Batch,
        ..Options::default()
    };

    let submitted = bench::run(&engine(), &run_dir, &samples, &batch, |_| {})
        .await
        .unwrap();
    assert_eq!(submitted.completed, 0);
    assert!(submitted.pending_batch.is_some());

    // Interrupted while the batch is still processing: the rerun waits on it.
    let waiting = bench::run(&engine(), &run_dir, &samples, &batch, |_| {})
        .await
        .unwrap();
    assert_eq!(waiting.pending_batch, submitted.pending_batch);

    api.finish_batches();
    let collected = bench::run(&engine(), &run_dir, &samples, &batch, |_| {})
        .await
        .unwrap();

    assert_eq!(collected.completed, 2);
    assert_eq!(collected.pending_batch, None);
    assert_eq!(api.batches().len(), 1, "the batch was submitted once");
    assert_eq!(api.batches()[0].len(), 2);
    assert!(api.requests().is_empty(), "nothing was asked synchronously");
    // Haiku 4.5 at the 50% batch discount: $0.50/M input, $2.50/M output,
    // cache reads at $0.05/M.
    let per_call = 0.0005 + 0.00025 + 0.0001;
    let records = records(&run_dir);
    let rows: Vec<_> = records
        .iter()
        .map(|r| (r.sample_id.as_str(), r.row_count, r.batch))
        .collect();
    assert_eq!(rows, [("dev_7", Some(1), true), ("dev_8", Some(1), true)]);
    for record in &records {
        assert!((record.cost_usd.unwrap() - per_call).abs() < 1e-12);
        assert_eq!(record.latency_ms, None);
    }
}

#[tokio::test]
async fn batch_requests_that_got_no_answer_stop_the_run_unrecorded_and_are_resubmitted_on_rerun() {
    let data = dataset_dir();
    let mongo = mongo_with(vec![doc! { "name": "Tom", "species": "cat" }]).await;
    let runs = tempfile::tempdir().unwrap();
    let run_dir = runs.path().join("batch");
    // One reply for two requests: the second request expires.
    let api = ScriptedLlm::new([reply("animals", r#"[{"$count": "n"}]"#)]);
    let engine = || {
        Engine::new(
            api.clone(),
            mongo.client.clone(),
            Dataset::open(data.path()).unwrap(),
        )
    };
    let samples = engine().dataset().samples().to_vec();
    let batch = Options {
        mode: Mode::Batch,
        ..Options::default()
    };
    bench::run(&engine(), &run_dir, &samples, &batch, |_| {})
        .await
        .unwrap();
    api.finish_batches();

    let collected = bench::run(&engine(), &run_dir, &samples, &batch, |_| {})
        .await
        .unwrap();

    assert_eq!(collected.completed, 1);
    assert!(collected.stopped.unwrap().contains("expired"));
    assert_eq!(collected.pending_batch, None);
    let ids: Vec<_> = records(&run_dir).into_iter().map(|r| r.sample_id).collect();
    assert_eq!(ids, ["dev_7"]);

    let resubmitted = bench::run(&engine(), &run_dir, &samples, &batch, |_| {})
        .await
        .unwrap();

    assert!(resubmitted.pending_batch.is_some());
    let batches = api.batches();
    assert_eq!(batches.len(), 2);
    let asked: Vec<_> = batches[1].iter().map(|r| r.user.contains("dogs")).collect();
    assert_eq!(asked, [true]);
}

#[tokio::test]
async fn stops_asking_once_every_run_under_the_same_root_has_spent_the_limit() {
    let data = dataset_dir();
    let mongo = mongo_with(vec![doc! { "name": "Tom", "species": "cat" }]).await;
    let runs = tempfile::tempdir().unwrap();
    // $50 a call on Haiku 4.5 ($1/M input).
    let usage = Usage {
        input_tokens: 50_000_000,
        ..Usage::default()
    };
    let api = ScriptedLlm::new(std::iter::repeat_n(
        reply("animals", r#"[{"$count": "n"}]"#),
        3,
    ))
    .with_model("claude-haiku-4-5")
    .with_usage(usage);
    let engine = || {
        Engine::new(
            api.clone(),
            mongo.client.clone(),
            Dataset::open(data.path()).unwrap(),
        )
    };
    let samples = engine().dataset().samples().to_vec();
    let limited = |mode| Options {
        mode,
        spend_limit_usd: Some(80.0),
        ..Options::default()
    };
    // An earlier run spent $50.
    bench::run(
        &engine(),
        &runs.path().join("earlier"),
        &samples[..1],
        &Options::default(),
        |_| {},
    )
    .await
    .unwrap();

    // $50 spent is under the limit, so one more call; then $100 is over it.
    let sync = bench::run(
        &engine(),
        &runs.path().join("sync"),
        &samples,
        &limited(Mode::Sync),
        |_| {},
    )
    .await
    .unwrap();

    assert_eq!(sync.completed, 1);
    assert!(sync.stopped.unwrap().contains("spend limit"));
    assert_eq!(api.requests().len(), 2);

    let batch = bench::run(
        &engine(),
        &runs.path().join("batch"),
        &samples,
        &limited(Mode::Batch),
        |_| {},
    )
    .await
    .unwrap();

    assert!(batch.stopped.unwrap().contains("spend limit"));
    assert!(api.batches().is_empty());
}

#[tokio::test]
async fn retries_rate_limits_and_overloads_a_few_times_but_stops_at_once_when_credit_runs_out() {
    let data = dataset_dir();
    let mongo = mongo_with(vec![doc! { "name": "Tom", "species": "cat" }]).await;
    let runs = tempfile::tempdir().unwrap();
    let credit = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low to access the Anthropic API."}}"#;
    let api = ScriptedLlm::new([
        Scripted::api_error(429, "rate limited"),
        Scripted::api_error(529, "overloaded"),
        Scripted::Text(reply("animals", r#"[{"$count": "n"}]"#)),
        Scripted::api_error(400, credit),
    ]);
    let engine = Engine::new(
        api.clone(),
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    );
    let samples = engine.dataset().samples().to_vec();

    let summary = bench::run(
        &engine,
        &runs.path().join("run"),
        &samples,
        &Options::default(),
        |_| {},
    )
    .await
    .unwrap();

    assert_eq!(summary.completed, 1);
    assert!(
        summary
            .stopped
            .unwrap()
            .contains("credit balance is too low")
    );
    assert_eq!(api.requests().len(), 4, "the credit error was not retried");

    // Overloaded on every attempt: three retries, then the run stops.
    let overloaded = ScriptedLlm::new((0..5).map(|_| Scripted::api_error(529, "overloaded")));
    let engine = Engine::new(
        overloaded.clone(),
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    );
    let summary = bench::run(
        &engine,
        &runs.path().join("run"),
        &samples,
        &Options::default(),
        |_| {},
    )
    .await
    .unwrap();

    assert_eq!(summary.completed, 0);
    assert!(summary.stopped.unwrap().contains("529"));
    assert_eq!(overloaded.requests().len(), 4);
}

#[tokio::test]
async fn waits_for_a_pending_batch_in_one_invocation_and_refuses_a_sync_run_meanwhile() {
    let data = dataset_dir();
    let mongo = mongo_with(vec![doc! { "name": "Tom", "species": "cat" }]).await;
    let runs = tempfile::tempdir().unwrap();
    let run_dir = runs.path().join("batch");
    let api = ScriptedLlm::new(std::iter::repeat_n(
        reply("animals", r#"[{"$count": "n"}]"#),
        2,
    ));
    let engine = || {
        Engine::new(
            api.clone(),
            mongo.client.clone(),
            Dataset::open(data.path()).unwrap(),
        )
    };
    let samples = engine().dataset().samples().to_vec();
    bench::run(
        &engine(),
        &run_dir,
        &samples,
        &Options {
            mode: Mode::Batch,
            ..Options::default()
        },
        |_| {},
    )
    .await
    .unwrap();

    // A sync run would answer the samples the pending batch is paying for.
    let sync = bench::run(&engine(), &run_dir, &samples, &Options::default(), |_| {})
        .await
        .unwrap();
    assert!(sync.stopped.unwrap().contains("still pending"));
    assert!(api.requests().is_empty());

    let mut checks = 0;
    let waited = bench::run(
        &engine(),
        &run_dir,
        &samples,
        &Options {
            mode: Mode::Batch,
            batch_poll: Some(std::time::Duration::ZERO),
            ..Options::default()
        },
        |progress| {
            if let Progress::BatchPending(_) = progress {
                checks += 1;
                if checks == 2 {
                    api.finish_batches();
                }
            }
        },
    )
    .await
    .unwrap();

    assert_eq!((waited.completed, waited.remaining), (2, 0));
    assert_eq!((waited.pending_batch, waited.stopped), (None, None));
    let manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(run_dir.join("manifest.json")).unwrap()).unwrap();
    let modes: Vec<_> = manifest["invocations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["mode"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(modes, ["batch", "sync", "batch"]);
}

#[tokio::test]
async fn questions_whose_prompt_exceeds_the_context_window_are_recorded_as_skipped_without_asking()
{
    let data = dataset_dir();
    let mongo = mongo_with(vec![doc! { "name": "Tom", "species": "cat" }]).await;
    let runs = tempfile::tempdir().unwrap();
    // The fixture prompt is a few hundred tokens: it fits 1,000 tokens, but
    // not with room for a 4,096-token reply.
    let api = ScriptedLlm::new(Vec::<String>::new()).with_context_window(1000);
    let engine = || {
        Engine::new(
            api.clone(),
            mongo.client.clone(),
            Dataset::open(data.path()).unwrap(),
        )
    };
    let samples = engine().dataset().samples().to_vec();

    for (dir, mode) in [("sync", Mode::Sync), ("batch", Mode::Batch)] {
        let run_dir = runs.path().join(dir);
        let summary = bench::run(
            &engine(),
            &run_dir,
            &samples,
            &Options {
                mode,
                ..Options::default()
            },
            |_| {},
        )
        .await
        .unwrap();

        assert_eq!((summary.completed, summary.remaining), (2, 0), "{dir}");
        assert_eq!((summary.stopped, summary.pending_batch), (None, None));
        for record in records(&run_dir) {
            assert_eq!(record.error.unwrap().kind, "skipped_context_overflow");
            assert_eq!(record.usage, Usage::default());
        }
        let manifest: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(run_dir.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["config"]["backend"], "scripted");
        assert_eq!(manifest["config"]["context_window"], 1000);
        assert_eq!(manifest["invocations"][0]["endpoint"], "scripted");
    }
    assert!(api.requests().is_empty() && api.batches().is_empty());
}
