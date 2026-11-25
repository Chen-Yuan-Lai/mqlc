//! `bench::replay`: feeding a run's recorded replies to an engine with other
//! settings, to see which samples it would repair, without calling an LLM.

mod support;

use std::fs;

use mongodb::bson::doc;
use mqlc_core::bench::{self, Options, ReplayLlm};
use mqlc_core::testing::ScriptedLlm;
use mqlc_core::{Dataset, Engine, RepairKind};
use support::{dataset_dir, mongo_with, reply};

#[tokio::test]
async fn a_replay_reports_what_the_new_settings_would_repair_and_how_it_scored() {
    let data = dataset_dir();
    let mongo = mongo_with(vec![
        doc! { "name": "Tom", "species": "cat" },
        doc! { "name": "Rex", "species": "dog" },
    ])
    .await;
    let run_dir = tempfile::tempdir().unwrap();
    // A run without value correction: dev_7 asks for a value no document has.
    let recorded = Engine::new(
        ScriptedLlm::new([
            reply(
                "animals",
                r#"[{"$match": {"species": "Cat"}}, {"$count": "n"}]"#,
            ),
            reply(
                "animals",
                r#"[{"$match": {"species": "dog"}}, {"$count": "n"}]"#,
            ),
        ]),
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    );
    let samples = recorded.dataset().samples().to_vec();
    bench::run(
        &recorded,
        run_dir.path(),
        &samples,
        &Options::default(),
        |_| {},
    )
    .await
    .unwrap();
    fs::write(
        run_dir.path().join("scores.jsonl"),
        r#"{"sample_id": "dev_7", "candidate": 0, "soft_ex": false}
{"sample_id": "dev_8", "candidate": 0, "soft_ex": true}
"#,
    )
    .unwrap();
    // What a kill mid-write leaves: replay reads past it and leaves it there.
    let mut results = fs::read_to_string(run_dir.path().join("results.jsonl")).unwrap();
    results.push_str(r#"{"arm": "off"#);
    fs::write(run_dir.path().join("results.jsonl"), &results).unwrap();

    let replaying = Engine::new(
        ReplayLlm::default(),
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    )
    .with_value_correction(true);
    let replayed = bench::replay(&replaying, run_dir.path(), &samples)
        .await
        .unwrap();

    assert_eq!(replayed.len(), 2);
    let cat = replayed.iter().find(|r| r.sample_id == "dev_7").unwrap();
    let (kind, problems) = cat.first_repair.as_ref().unwrap();
    assert_eq!(*kind, RepairKind::ValueCorrection);
    assert_eq!(problems[0].kind, "missing_value");
    assert!(
        problems[0].message.contains("\"cat\""),
        "{}",
        problems[0].message
    );
    assert_eq!(cat.soft_ex, Some(false));
    let dog = replayed.iter().find(|r| r.sample_id == "dev_8").unwrap();
    assert!(dog.first_repair.is_none());
    assert_eq!(dog.soft_ex, Some(true));
    assert_eq!(
        fs::read_to_string(run_dir.path().join("results.jsonl")).unwrap(),
        results
    );
}
