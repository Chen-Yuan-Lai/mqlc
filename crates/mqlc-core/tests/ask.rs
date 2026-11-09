mod support;

use mongodb::bson::{DateTime, doc};
use mqlc_core::testing::ScriptedLlm;
use mqlc_core::{AskError, Dataset, Engine};
use support::{dataset_dir, mongo_with, reply};

#[tokio::test]
async fn answers_a_question_with_collection_pipeline_and_rows() {
    let data = dataset_dir();
    let mongo = mongo_with(vec![
        doc! { "name": "Tom", "species": "cat" },
        doc! { "name": "Rex", "species": "dog" },
        doc! { "name": "Kit", "species": "cat" },
    ])
    .await;
    let llm = ScriptedLlm::new([reply(
        "animals",
        r#"[{"$match": {"species": "cat"}}, {"$count": "count"}]"#,
    )]);
    let engine = Engine::new(
        llm,
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    );

    let sample = engine.dataset().sample("dev_7").unwrap().clone();
    let answer = engine.ask(&sample).await.unwrap();

    assert_eq!(answer.collection, "animals");
    assert_eq!(
        answer.pipeline,
        vec![
            doc! { "$match": { "species": "cat" } },
            doc! { "$count": "count" }
        ]
    );
    assert_eq!(answer.rows, vec![doc! { "count": 2 }]);
}

#[tokio::test]
async fn sends_the_official_prompt_with_schema_and_question_but_no_evidence() {
    let data = dataset_dir();
    let mongo = mongo_with(vec![doc! { "name": "Tom", "species": "cat" }]).await;
    let llm = ScriptedLlm::new([reply("animals", r#"[{"$count": "n"}]"#)]);
    let engine = Engine::new(
        llm,
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    );

    let sample = engine.dataset().sample("dev_7").unwrap().clone();
    engine.ask(&sample).await.unwrap();

    let requests = engine.llm().requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(
        request.system,
        "# ROLE\nYou are an expert MongoDB engineer."
    );
    assert_eq!(request.temperature, 0.0);
    assert!(request.user.starts_with(
        "<MongoDB Schema Description>\n\nCollection: animals\n{\"field_name\": \"name\""
    ));
    assert!(request.user.contains(
        "<Natural Language Request>\nHow many cats are there?\n</Natural Language Request>"
    ));
    assert!(!request.user.contains("Evidence"));
}

#[tokio::test]
async fn accepts_python_literals_and_extended_json_dates() {
    let data = dataset_dir();
    let mongo = mongo_with(vec![
        doc! { "name": "Tom", "species": "cat", "vaccinated": true, "born": DateTime::parse_rfc3339_str("2020-05-01T00:00:00Z").unwrap() },
        doc! { "name": "Kit", "species": "cat", "vaccinated": true, "born": DateTime::parse_rfc3339_str("2018-01-01T00:00:00Z").unwrap() },
        doc! { "name": "Rex", "species": "dog", "vaccinated": false, "born": DateTime::parse_rfc3339_str("2021-01-01T00:00:00Z").unwrap() },
    ])
    .await;
    let llm = ScriptedLlm::new([reply(
        "animals",
        r#"[
        # the pet's vaccination [status] and birth date
        {"$match": {"vaccinated": True, "chip": None, "born": {"$gte": {"$date": "2019-01-01T00:00:00Z"}}}},
        {"$match": {"name": {"$regex": "^T\w+$"}, "species": {"$regex": r"^c\w+"}}},
        {"$project": {"_id": 0, "name": 1},},
    ]"#,
    )]);
    let engine = Engine::new(
        llm,
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    );

    let sample = engine.dataset().sample("dev_7").unwrap().clone();
    let answer = engine.ask(&sample).await.unwrap();

    assert_eq!(answer.rows, vec![doc! { "name": "Tom" }]);
}

#[tokio::test]
async fn refuses_pipelines_that_write() {
    let data = dataset_dir();
    let mongo = mongo_with(vec![doc! { "name": "Tom", "species": "cat" }]).await;
    let llm = ScriptedLlm::new([
        reply("animals", r#"[{"$match": {}}, {"$out": "copied"}]"#),
        reply("animals", r#"[{"$merge": {"into": "merged"}}]"#),
    ]);
    let engine = Engine::new(
        llm,
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    );
    let sample = engine.dataset().sample("dev_7").unwrap().clone();

    for stage in ["$out", "$merge"] {
        let err = engine.ask(&sample).await.unwrap_err();
        assert!(
            matches!(&err, AskError::WriteStage(s) if s == stage),
            "{err}"
        );
    }
    let collections = mongo
        .client
        .database(support::DB_ID)
        .list_collection_names()
        .await
        .unwrap();
    assert_eq!(collections, ["animals"]);
}
