mod support;

use mongodb::bson::{DateTime, doc};
use mqlc_core::profile::{self, ProfileOptions};
use mqlc_core::testing::ScriptedLlm;
use mqlc_core::{AnswerError, Arm, AskError, Dataset, Engine, Query};
use support::{DB_ID, dataset_dir, mongo_with, reply};

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

    assert_eq!(
        answer.query,
        Some(Query {
            collection: "animals".to_string(),
            pipeline: vec![
                doc! { "$match": { "species": "cat" } },
                doc! { "$count": "count" }
            ],
        })
    );
    assert_eq!(answer.rows.unwrap(), vec![doc! { "count": 2 }]);
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
    assert!(request.context.starts_with(
        "<MongoDB Schema Description>\n\nCollection: animals\n{\"field_name\": \"name\""
    ));
    assert!(request.user.starts_with(
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

    assert_eq!(answer.rows.unwrap(), vec![doc! { "name": "Tom" }]);
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
        let err = engine.ask(&sample).await.unwrap().rows.unwrap_err();
        assert!(
            matches!(&err, AnswerError::WriteStage(s) if s == stage),
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

#[tokio::test]
async fn the_auto_mdl_arm_sends_the_profiled_mdl_in_place_of_the_official_description() {
    let data = dataset_dir();
    let mongo = mongo_with(vec![
        doc! { "name": "Tom", "species": "cat", "visits": [{ "vet": "V1" }] },
        doc! { "name": "Rex", "species": "dog" },
    ])
    .await;
    let knowledge = tempfile::tempdir().unwrap();
    profile::profile(&mongo.client, DB_ID, &ProfileOptions::default())
        .await
        .unwrap()
        .save(&knowledge.path().join(DB_ID))
        .unwrap();
    let llm = ScriptedLlm::new([reply("animals", r#"[{"$count": "n"}]"#)]);
    let engine = Engine::new(
        llm,
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    )
    .with_arm(Arm::AutoMdl {
        dir: knowledge.path().to_path_buf(),
    });

    let sample = engine.dataset().sample("dev_7").unwrap().clone();
    engine.ask(&sample).await.unwrap();

    let request = &engine.llm().requests()[0];
    let expected_start = concat!(
        "<MongoDB Schema Description>\n\n",
        "Collection: animals\n",
        r#"{"field_name": "_id", "data_type": "ObjectId"}"#,
        "\n",
        r#"{"field_name": "name", "data_type": "String"}"#,
        "\n",
        r#"{"field_name": "species", "data_type": "String"}"#,
        "\n",
        r#"{"field_name": "visits", "data_type": "Array<Document>", "present": "50%"}"#,
        "\n",
        r#"{"field_name": "visits.vet", "data_type": "String", "in_array": "visits"}"#,
        "\n\nInstructions:\n- field_name is",
    );
    assert!(
        request.context.starts_with(expected_start),
        "{}",
        request.context
    );
    assert!(request.context.ends_with("</MongoDB Schema Description>"));
    assert!(!request.context.contains("Pet name"));
    assert_eq!(engine.arm().name(), "auto-mdl");
}

#[tokio::test]
async fn a_database_with_no_auto_mdl_is_an_error_not_an_answer() {
    let data = dataset_dir();
    let mongo = mongo_with(vec![doc! { "name": "Tom", "species": "cat" }]).await;
    let empty = tempfile::tempdir().unwrap();
    let engine = Engine::new(
        ScriptedLlm::new([reply("animals", r#"[{"$count": "n"}]"#)]),
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    )
    .with_arm(Arm::AutoMdl {
        dir: empty.path().to_path_buf(),
    });

    let sample = engine.dataset().sample("dev_7").unwrap().clone();
    let err = engine.ask(&sample).await.unwrap_err();

    assert!(matches!(err, AskError::Knowledge(_)), "{err}");
    assert!(engine.llm().requests().is_empty());
}
