mod support;

use std::fs;
use std::path::Path;

use mongodb::bson::{DateTime, doc};
use mqlc_core::profile::{self, ProfileOptions};
use mqlc_core::testing::ScriptedLlm;
use mqlc_core::{AnswerError, Arm, AskError, Dataset, Engine, Query};
use support::{DB_ID, dataset_dir, mongo_with, mongo_with_collections, reply};

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
async fn the_prompt_keys_arm_rewrites_each_field_to_the_keys_the_official_prompt_names() {
    let data = dataset_dir();
    let mongo = mongo_with(vec![doc! { "name": "Tom", "species": "cat" }]).await;
    let llm = ScriptedLlm::new([reply("animals", r#"[{"$count": "n"}]"#)]);
    let engine = Engine::new(
        llm,
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    )
    .with_arm(Arm::OfficialPromptKeys);

    let sample = engine.dataset().sample("dev_7").unwrap().clone();
    engine.ask(&sample).await.unwrap();

    assert_eq!(
        engine.llm().requests()[0].context,
        "<MongoDB Schema Description>\n\n\
         Collection: animals\n\
         {\"field_name\": \"name\", \"field_description\": \"Pet name\", \"data_format\": \"String\", \"value_description\": \"\"}\n\
         {\"field_name\": \"species\", \"field_description\": \"Kind of animal\", \"data_format\": \"String\", \"value_description\": \"cat, dog\"}\
         \n\n</MongoDB Schema Description>"
    );
}

#[tokio::test]
async fn the_tagged_arm_wraps_each_collection_in_a_collection_tag() {
    let data = dataset_dir();
    let mongo = mongo_with(vec![doc! { "name": "Tom", "species": "cat" }]).await;
    let llm = ScriptedLlm::new([reply("animals", r#"[{"$count": "n"}]"#)]);
    let engine = Engine::new(
        llm,
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    )
    .with_arm(Arm::OfficialTagged);

    let sample = engine.dataset().sample("dev_7").unwrap().clone();
    engine.ask(&sample).await.unwrap();

    assert_eq!(
        engine.llm().requests()[0].context,
        "<MongoDB Schema Description>\n\n\
         <collection name=\"animals\">\n\
         {\"field_name\": \"name\", \"field_description\": \"Pet name\", \"data_type\": \"String\", \"required\": true, \"value_description\": \"\"}\n\
         {\"field_name\": \"species\", \"field_description\": \"Kind of animal\", \"data_type\": \"String\", \"required\": true, \"value_description\": \"cat, dog\"}\n\
         </collection>\
         \n\n</MongoDB Schema Description>"
    );
}

#[tokio::test]
async fn a_single_message_puts_the_system_prompt_at_the_start_of_the_user_turn() {
    let data = dataset_dir();
    let mongo = mongo_with(vec![doc! { "name": "Tom", "species": "cat" }]).await;
    let llm = ScriptedLlm::new([reply("animals", r#"[{"$count": "n"}]"#)]);
    let engine = Engine::new(
        llm,
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    )
    .with_single_message(true);

    let sample = engine.dataset().sample("dev_7").unwrap().clone();
    engine.ask(&sample).await.unwrap();

    let request = &engine.llm().requests()[0];
    assert_eq!(request.system, "");
    assert!(request.context.starts_with(
        "# ROLE\nYou are an expert MongoDB engineer.\n\n<MongoDB Schema Description>\n\nCollection: animals\n"
    ));
    assert!(request.user.starts_with("<Natural Language Request>\n"));
    assert!(engine.settings().single_message);
}

#[tokio::test]
async fn thinking_gets_its_budget_on_top_of_the_reply_tokens() {
    let data = dataset_dir();
    let mongo = mongo_with(vec![doc! { "name": "Tom", "species": "cat" }]).await;
    let llm = ScriptedLlm::new([reply("animals", r#"[{"$count": "n"}]"#)]);
    let engine = Engine::new(
        llm,
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    )
    .with_thinking_budget(Some(2048));

    let sample = engine.dataset().sample("dev_7").unwrap().clone();
    engine.ask(&sample).await.unwrap();

    let request = &engine.llm().requests()[0];
    assert_eq!(request.thinking_budget, Some(2048));
    assert_eq!(request.max_tokens, 4096 + 2048);
    let settings = engine.settings();
    assert!(settings.thinking);
    assert_eq!(settings.thinking_budget, Some(2048));
    assert_eq!(settings.max_tokens, 4096 + 2048);
}

#[tokio::test]
async fn sends_the_temperature_it_was_given() {
    let data = dataset_dir();
    let mongo = mongo_with(vec![doc! { "name": "Tom", "species": "cat" }]).await;
    let llm = ScriptedLlm::new([reply("animals", r#"[{"$count": "n"}]"#)]);
    let engine = Engine::new(
        llm,
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    )
    .with_temperature(1.0);

    let sample = engine.dataset().sample("dev_7").unwrap().clone();
    engine.ask(&sample).await.unwrap();

    assert_eq!(engine.llm().requests()[0].temperature, 1.0);
    assert_eq!(engine.settings().temperature, 1.0);
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
    let mongo = mongo_with_collections([
        (
            "animals",
            vec![
                doc! { "name": "Tom", "species": "cat", "ownerId": "o1", "visits": [{ "vet": "V1" }] },
                doc! { "name": "Rex", "species": "dog", "ownerId": "o1" },
            ],
        ),
        ("owners", vec![doc! { "_id": "o1" }]),
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
        r#"{"field_name": "name", "data_type": "String", "values": ["Tom","Rex"]}"#,
        "\n",
        r#"{"field_name": "species", "data_type": "String", "values": ["cat","dog"]}"#,
        "\n",
        r#"{"field_name": "ownerId", "data_type": "String", "references": "owners._id"}"#,
        "\n",
        r#"{"field_name": "visits", "data_type": "Array<Document>", "present": "50%"}"#,
        "\n",
        r#"{"field_name": "visits.vet", "data_type": "String", "in_array": "visits", "values": ["V1"]}"#,
        "\n\nCollection: owners\n",
        r#"{"field_name": "_id", "data_type": "String", "values": ["o1"]}"#,
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
async fn the_auto_mdl_summarizes_fields_with_too_many_values_to_list() {
    let data = dataset_dir();
    let mongo = mongo_with(
        (1..=30)
            .map(|i| {
                let name = match i {
                    1..=5 => "Tom".to_string(),
                    6..=8 => "Kit".to_string(),
                    _ => format!("Pet {i}"),
                };
                doc! {
                    "_id": i, "name": name, "chip": format!("CH-{i:04}"), "weight": i * 2,
                    "born": DateTime::from_millis(i64::from(i) * 86_400_000),
                }
            })
            .collect(),
    )
    .await;
    let knowledge = tempfile::tempdir().unwrap();
    profile::profile(&mongo.client, DB_ID, &ProfileOptions::default())
        .await
        .unwrap()
        .save(&knowledge.path().join(DB_ID))
        .unwrap();
    let engine = Engine::new(
        ScriptedLlm::new([reply("animals", r#"[{"$count": "n"}]"#)]),
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    )
    .with_arm(Arm::AutoMdl {
        dir: knowledge.path().to_path_buf(),
    });

    let sample = engine.dataset().sample("dev_7").unwrap().clone();
    engine.ask(&sample).await.unwrap();

    let context = &engine.llm().requests()[0].context;
    for line in [
        r#"{"field_name": "name", "data_type": "String", "common_values": ["Tom","Kit"]}"#,
        r#"{"field_name": "chip", "data_type": "String", "format": "[A-Z]{2}-\\d{4}"}"#,
        r#"{"field_name": "weight", "data_type": "Int32", "min": 2, "max": 60}"#,
        r#"{"field_name": "born", "data_type": "Date", "min": "1970-01-02T00:00:00Z", "max": "1970-01-31T00:00:00Z"}"#,
    ] {
        assert!(context.contains(line), "{line} not in {context}");
    }
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

#[tokio::test]
async fn the_evidence_arm_adds_the_samples_mongodb_evidence_to_the_official_prompt() {
    let data = dataset_dir();
    let mongo = mongo_with(vec![doc! { "name": "Tom", "species": "cat" }]).await;
    let llm = ScriptedLlm::new([reply("animals", r#"[{"$count": "n"}]"#)]);
    let engine = Engine::new(
        llm,
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    )
    .with_arm(Arm::OfficialEvidence);

    let sample = engine.dataset().sample("dev_7").unwrap().clone();
    engine.ask(&sample).await.unwrap();

    let request = &engine.llm().requests()[0];
    assert!(
        request
            .context
            .contains("Collection: animals\n{\"field_name\": \"name\"")
    );
    assert!(request.user.ends_with(
        "</Natural Language Request>\n\n-----------------------------\n\n\
         <Evidence>\ncats refers to species = 'cat'\n</Evidence>"
    ));
    assert_eq!(engine.arm().name(), "official-evidence");
}

#[tokio::test]
async fn the_evidence_arm_sends_a_sample_without_evidence_the_official_prompt_unchanged() {
    let data = dataset_dir();
    let path = data.path().join("aptmqlbench_data.jsonl");
    let text = fs::read_to_string(&path).unwrap();
    fs::write(&path, text.replace("cats refers to species = 'cat'", "")).unwrap();
    let mongo = mongo_with(vec![doc! { "name": "Tom", "species": "cat" }]).await;
    let ask = |arm: Arm| {
        let mongo = mongo.client.clone();
        let data = data.path().to_path_buf();
        async move {
            let engine = Engine::new(
                ScriptedLlm::new([reply("animals", r#"[{"$count": "n"}]"#)]),
                mongo,
                Dataset::open(data).unwrap(),
            )
            .with_arm(arm);
            let sample = engine.dataset().sample("dev_7").unwrap().clone();
            engine.ask(&sample).await.unwrap();
            engine.llm().requests()[0].clone()
        }
    };

    let with_evidence_arm = ask(Arm::OfficialEvidence).await;
    let official = ask(Arm::Official).await;

    assert_eq!(with_evidence_arm, official);
    assert!(!with_evidence_arm.user.contains("Evidence"));
}

#[tokio::test]
async fn the_official_and_auto_mdl_arm_merges_the_auto_mdl_into_each_official_field_line() {
    let data = dataset_dir();
    let description = data
        .path()
        .join("databases/dev/pets/collections_description/animals.jsonl");
    let official = fs::read_to_string(&description).unwrap();
    let visits = r#"{"field_name": "visits", "field_description": "Vet visits", "data_type": "Array", "required": false, "value_description": ""}"#;
    fs::write(&description, format!("{official}\n{visits}\n")).unwrap();
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
    let engine = Engine::new(
        ScriptedLlm::new([reply("animals", r#"[{"$count": "n"}]"#)]),
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    )
    .with_arm(Arm::OfficialAutoMdl {
        dir: knowledge.path().to_path_buf(),
    });

    let sample = engine.dataset().sample("dev_7").unwrap().clone();
    engine.ask(&sample).await.unwrap();

    let context = &engine.llm().requests()[0].context;
    // Official fields first, in the official order, each with what the
    // profile adds; then the fields only the profile saw.
    let expected_start = concat!(
        "<MongoDB Schema Description>\n\n",
        "Collection: animals\n",
        r#"{"field_name": "name", "field_description": "Pet name", "data_type": "String", "required": true, "value_description": "", "values": ["Tom","Rex"]}"#,
        "\n",
        r#"{"field_name": "species", "field_description": "Kind of animal", "data_type": "String", "required": true, "value_description": "cat, dog", "values": ["cat","dog"]}"#,
        "\n",
        r#"{"field_name": "visits", "field_description": "Vet visits", "data_type": "Array", "required": false, "value_description": "", "observed_type": "Array<Document>", "present": "50%"}"#,
        "\n",
        r#"{"field_name": "_id", "data_type": "ObjectId"}"#,
        "\n",
        r#"{"field_name": "visits.vet", "data_type": "String", "in_array": "visits", "values": ["V1"]}"#,
        "\n\nInstructions:\n- field_name is",
    );
    assert!(context.starts_with(expected_start), "{context}");
    assert!(
        context.contains("\n- In a field with a field_description, data_type is the official"),
        "{context}"
    );
    assert_eq!(context.matches("Collection: animals").count(), 1);
    assert_eq!(engine.arm().name(), "official-auto-mdl");
}

/// Moves the fixture's database to the train split, where a human MDL may be
/// written (ADR 0006); its samples become `train_7` and `train_8`.
fn move_to_train(data: &Path) {
    let path = data.join("aptmqlbench_data.jsonl");
    let text = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        text.replace(r#""split": "dev""#, r#""split": "train""#)
            .replace("dev_", "train_"),
    )
    .unwrap();
    fs::create_dir_all(data.join("databases/train")).unwrap();
    fs::rename(
        data.join("databases/dev/pets"),
        data.join("databases/train/pets"),
    )
    .unwrap();
}

/// Profiles the fixture database into `knowledge/<DB_ID>/` and applies
/// `edit` to its `mdl.json` and `instructions.json`, as a person would.
async fn profile_and_edit(
    mongo: &support::Mongo,
    knowledge: &Path,
    edit: impl FnOnce(&mut serde_json::Value, &mut serde_json::Value),
) {
    let dir = knowledge.join(DB_ID);
    profile::profile(&mongo.client, DB_ID, &ProfileOptions::default())
        .await
        .unwrap()
        .save(&dir)
        .unwrap();
    let read = |file: &str| -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(dir.join(file)).unwrap()).unwrap()
    };
    let (mut mdl, mut instructions) = (read("mdl.json"), read("instructions.json"));
    edit(&mut mdl, &mut instructions);
    fs::write(dir.join("mdl.json"), mdl.to_string()).unwrap();
    fs::write(dir.join("instructions.json"), instructions.to_string()).unwrap();
}

#[tokio::test]
async fn a_human_mdl_sends_its_descriptions_formulas_and_instructions() {
    let data = dataset_dir();
    move_to_train(data.path());
    let mongo = mongo_with(vec![
        doc! { "name": "Tom", "species": "cat", "born": 2019 },
        doc! { "name": "Rex", "species": "dog", "born": 2021 },
    ])
    .await;
    let knowledge = tempfile::tempdir().unwrap();
    profile_and_edit(&mongo, knowledge.path(), |mdl, instructions| {
        let animals = &mut mdl["models"][0];
        animals["properties"]["description"] = "Pets registered at the clinic".into();
        let columns = animals["columns"].as_array_mut().unwrap();
        let species = columns.iter_mut().find(|c| c["name"] == "species").unwrap();
        species["properties"]["description"] = "cat or dog, lower case".into();
        columns.push(serde_json::json!({
            "name": "age",
            "type": "Int32",
            "isCalculated": true,
            "expression": r#"{"$subtract": [2024, "$born"]}"#,
            "properties": { "description": "Age in years in 2024" },
        }));
        instructions
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({ "instruction": "A puppy is a dog under 1 year." }));
    })
    .await;
    let engine = Engine::new(
        ScriptedLlm::new([reply("animals", r#"[{"$count": "n"}]"#)]),
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    )
    .with_arm(Arm::HumanMdl {
        dir: knowledge.path().to_path_buf(),
    });

    let sample = engine.dataset().sample("train_7").unwrap().clone();
    engine.ask(&sample).await.unwrap();

    let context = &engine.llm().requests()[0].context;
    for expected in [
        "Collection: animals\nDescription: Pets registered at the clinic\n{\"field_name\": \"_id\"",
        r#"{"field_name": "species", "data_type": "String", "description": "cat or dog, lower case", "values": ["cat","dog"]}"#,
        r#"{"field_name": "age", "data_type": "Int32", "description": "Age in years in 2024", "computed_as": "{\"$subtract\": [2024, \"$born\"]}"}"#,
        "\n- A puppy is a dog under 1 year.\n",
    ] {
        assert!(context.contains(expected), "{expected} not in {context}");
    }
    assert_eq!(engine.arm().name(), "human-mdl");
}

#[tokio::test]
async fn a_human_mdl_of_a_dev_database_is_refused_before_anything_is_sent() {
    let data = dataset_dir();
    let mongo = mongo_with(vec![doc! { "name": "Tom", "species": "cat" }]).await;
    let knowledge = tempfile::tempdir().unwrap();
    profile_and_edit(&mongo, knowledge.path(), |_, _| {}).await;

    for arm in [
        Arm::HumanMdl {
            dir: knowledge.path().to_path_buf(),
        },
        Arm::OfficialHumanMdl {
            dir: knowledge.path().to_path_buf(),
        },
    ] {
        let engine = Engine::new(
            ScriptedLlm::new([reply("animals", r#"[{"$count": "n"}]"#)]),
            mongo.client.clone(),
            Dataset::open(data.path()).unwrap(),
        )
        .with_arm(arm);

        let sample = engine.dataset().sample("dev_7").unwrap().clone();
        let err = engine.ask(&sample).await.unwrap_err();

        assert!(matches!(err, AskError::Leakage { .. }), "{err}");
        assert!(engine.llm().requests().is_empty());
    }
}

#[tokio::test]
async fn an_mdl_key_that_mqlc_would_not_send_is_an_error_not_silently_dropped() {
    let data = dataset_dir();
    let mongo = mongo_with(vec![doc! { "name": "Tom", "species": "cat" }]).await;
    let knowledge = tempfile::tempdir().unwrap();
    profile_and_edit(&mongo, knowledge.path(), |mdl, _| {
        mdl["models"][0]["columns"][1]["expresion"] = "misspelt".into();
    })
    .await;
    let engine = Engine::new(
        ScriptedLlm::new([reply("animals", r#"[{"$count": "n"}]"#)]),
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    )
    .with_arm(Arm::AutoMdl {
        dir: knowledge.path().to_path_buf(),
    });

    let sample = engine.dataset().sample("dev_7").unwrap().clone();
    let err = engine.ask(&sample).await.unwrap_err();

    assert!(err.to_string().contains("expresion"), "{err}");
    assert!(engine.llm().requests().is_empty());
}
