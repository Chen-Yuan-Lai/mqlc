//! Repair: feeding an execution error or a static check error back to the
//! model, through the single-question API (ADR 0007).

mod support;

use mongodb::bson::{DateTime, doc};
use mqlc_core::testing::ScriptedLlm;
use mqlc_core::{Answer, AnswerError, Dataset, Engine, Problem, Repair, RepairKind};
use support::{dataset_dir, describe, field, mongo_with, mongo_with_collections, reply};

const COUNT_CATS: &str = r#"[{"$match": {"species": "cat"}}, {"$count": "count"}]"#;
/// Fails on the server: no such stage.
const BAD_STAGE: &str = r#"[{"$match": {"species": "cat"}}, {"$cnt": "count"}]"#;

fn cats() -> Vec<mongodb::bson::Document> {
    vec![
        doc! { "name": "Tom", "species": "cat" },
        doc! { "name": "Rex", "species": "dog" },
        doc! { "name": "Kit", "species": "cat" },
    ]
}

#[tokio::test]
async fn an_execution_error_is_fed_back_and_the_corrected_pipeline_answers() {
    let data = dataset_dir();
    let mongo = mongo_with(cats()).await;
    let first = reply("animals", BAD_STAGE);
    let llm = ScriptedLlm::new([first.clone(), reply("animals", COUNT_CATS)]);
    let engine = Engine::new(
        llm,
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    )
    .with_repair(Repair::Execution);

    let sample = engine.dataset().sample("dev_7").unwrap().clone();
    let answer = engine.ask(&sample).await.unwrap();

    assert_eq!(answer.rows.unwrap(), vec![doc! { "count": 2 }]);
    assert_eq!(answer.repairs.len(), 1);
    assert_eq!(answer.repairs[0].kind, RepairKind::Execution);
    // The server's name for the error, to tell execution errors apart.
    assert_eq!(
        answer.repairs[0].problems[0].code.as_deref(),
        Some("Location40324")
    );
    let requests = engine.llm().requests();
    assert_eq!(requests.len(), 2);
    // The same prompt, then the rejected reply and why it was rejected.
    assert_eq!(requests[1].context, requests[0].context);
    assert_eq!(requests[1].user, requests[0].user);
    assert_eq!(requests[1].turns.len(), 1);
    assert_eq!(requests[1].turns[0].reply, first);
    assert!(
        requests[1].turns[0].feedback.contains("$cnt"),
        "{}",
        requests[1].turns[0].feedback
    );
}

#[tokio::test]
async fn execution_errors_are_repaired_at_most_twice() {
    let data = dataset_dir();
    let mongo = mongo_with(cats()).await;
    let llm = ScriptedLlm::new([
        reply("animals", BAD_STAGE),
        reply("animals", BAD_STAGE),
        reply("animals", BAD_STAGE),
        reply("animals", COUNT_CATS),
    ]);
    let engine = Engine::new(
        llm,
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    )
    .with_repair(Repair::Execution);

    let sample = engine.dataset().sample("dev_7").unwrap().clone();
    let answer = engine.ask(&sample).await.unwrap();

    assert_eq!(engine.llm().requests().len(), 3);
    assert_eq!(answer.repairs.len(), 2);
    assert!(matches!(answer.rows, Err(AnswerError::Mongo(_))));
    assert_eq!(engine.llm().requests()[2].turns.len(), 2);
}

#[tokio::test]
async fn without_repair_an_execution_error_is_the_answer() {
    let data = dataset_dir();
    let mongo = mongo_with(cats()).await;
    let llm = ScriptedLlm::new([reply("animals", BAD_STAGE), reply("animals", COUNT_CATS)]);
    let engine = Engine::new(
        llm,
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    );

    let sample = engine.dataset().sample("dev_7").unwrap().clone();
    let answer = engine.ask(&sample).await.unwrap();

    assert_eq!(engine.llm().requests().len(), 1);
    assert!(answer.repairs.is_empty());
    assert!(matches!(answer.rows, Err(AnswerError::Mongo(_))));
}

/// Animals with a date, an embedded document and an array of documents,
/// described as such, and their owners.
async fn zoo() -> (tempfile::TempDir, support::Mongo) {
    let data = dataset_dir();
    let animals = [
        field("_id", "ObjectId"),
        field("name", "String"),
        field("species", "String"),
        field("born", "Date"),
        field("ownerId", "Number"),
        field("owner", "Document"),
        field("owner.name", "String"),
        field("visits", "Array"),
        field("visits.vet", "String"),
        field("visits.cost", "Null, Number"),
    ];
    describe(
        data.path(),
        "animals",
        &animals.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    describe(
        data.path(),
        "owners",
        &[&field("_id", "Number"), &field("city", "String")],
    );
    let born = |y| DateTime::builder().year(y).month(1).day(1).build().unwrap();
    let mongo = mongo_with_collections([
        (
            "animals",
            vec![
                doc! { "name": "Tom", "species": "cat", "born": born(2022), "ownerId": 1,
                "owner": { "name": "Ann" },
                "visits": [{ "vet": "Lee", "cost": 30 }, { "vet": "Kim", "cost": 50 }] },
                doc! { "name": "Rex", "species": "dog", "born": born(2018), "ownerId": 2,
                "owner": { "name": "Bob" }, "visits": [{ "vet": "Lee", "cost": 20 }] },
            ],
        ),
        (
            "owners",
            vec![
                doc! { "_id": 1, "city": "Oslo" },
                doc! { "_id": 2, "city": "Rome" },
            ],
        ),
    ])
    .await;
    (data, mongo)
}

/// Asks dev_7 with these replies under the IR arm.
async fn ask_checked(replies: &[String]) -> (Answer, Vec<mqlc_core::LlmRequest>) {
    let (data, mongo) = zoo().await;
    let llm = ScriptedLlm::new(replies.to_vec());
    let engine = Engine::new(
        llm,
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    )
    .with_repair(Repair::StaticCheck);
    let sample = engine.dataset().sample("dev_7").unwrap().clone();
    let answer = engine.ask(&sample).await.unwrap();
    (answer, engine.llm().requests())
}

/// The single problem the static check found in the first candidate.
fn only_problem(answer: &Answer) -> &Problem {
    assert_eq!(answer.repairs.len(), 1, "{:?}", answer.repairs);
    assert_eq!(answer.repairs[0].kind, RepairKind::StaticCheck);
    assert_eq!(
        answer.repairs[0].problems.len(),
        1,
        "{:?}",
        answer.repairs[0].problems
    );
    &answer.repairs[0].problems[0]
}

#[tokio::test]
async fn a_field_path_missing_from_the_schema_is_fed_back_before_the_pipeline_runs() {
    let (answer, requests) = ask_checked(&[
        reply(
            "animals",
            r#"[{"$match": {"owner.nme": "Ann"}}, {"$count": "count"}]"#,
        ),
        reply(
            "animals",
            r#"[{"$match": {"owner.name": "Ann"}}, {"$count": "count"}]"#,
        ),
    ])
    .await;

    let problem = only_problem(&answer);
    assert_eq!(problem.kind, "unknown_field");
    assert!(problem.message.contains("owner.nme"), "{}", problem.message);
    assert!(requests[1].turns[0].feedback.contains("owner.nme"));
    assert_eq!(answer.rows.unwrap(), vec![doc! { "count": 1 }]);
    assert!(answer.check_problems.is_empty());
}

#[tokio::test]
async fn comparing_a_date_field_with_a_string_is_a_type_mismatch() {
    let (answer, _) = ask_checked(&[
        reply("animals", r#"[{"$match": {"born": {"$gte": "2020-01-01"}}}, {"$count": "count"}]"#),
        reply(
            "animals",
            r#"[{"$match": {"born": {"$gte": {"$date": "2020-01-01T00:00:00Z"}}}}, {"$count": "count"}]"#,
        ),
    ])
    .await;

    let problem = only_problem(&answer);
    assert_eq!(problem.kind, "type_mismatch");
    assert!(problem.message.contains("born"), "{}", problem.message);
    assert_eq!(answer.rows.unwrap(), vec![doc! { "count": 1 }]);
}

#[tokio::test]
async fn a_date_compared_with_a_string_inside_an_expression_is_a_type_mismatch() {
    let (answer, _) = ask_checked(&[
        reply(
            "animals",
            r#"[{"$match": {"$expr": {"$gt": ["$born", "2020-01-01"]}}}, {"$count": "count"}]"#,
        ),
        reply("animals", COUNT_CATS),
    ])
    .await;

    assert_eq!(only_problem(&answer).kind, "type_mismatch");
}

#[tokio::test]
async fn unwinding_a_field_that_is_not_an_array_is_fed_back() {
    let (answer, _) = ask_checked(&[
        reply("animals", r#"[{"$unwind": "$owner"}, {"$count": "count"}]"#),
        reply(
            "animals",
            r#"[{"$unwind": "$visits"}, {"$count": "count"}]"#,
        ),
    ])
    .await;

    let problem = only_problem(&answer);
    assert_eq!(problem.kind, "unwind_non_array");
    assert!(problem.message.contains("owner"), "{}", problem.message);
    // MongoDB unwinds a non-array as one element: the feedback says so.
    assert!(problem.message.contains("unchanged"), "{}", problem.message);
    assert_eq!(answer.rows.unwrap(), vec![doc! { "count": 3 }]);
}

#[tokio::test]
async fn a_collection_missing_from_the_schema_is_fed_back() {
    let (answer, _) = ask_checked(&[
        reply(
            "animals",
            r#"[{"$lookup": {"from": "people", "localField": "ownerId", "foreignField": "_id", "as": "o"}}]"#,
        ),
        reply("animals", COUNT_CATS),
    ])
    .await;

    let problem = only_problem(&answer);
    assert_eq!(problem.kind, "unknown_collection");
    assert!(problem.message.contains("people"), "{}", problem.message);
}

#[tokio::test]
async fn fields_are_followed_through_lookups_unwinds_groups_and_projections() {
    // Every path here exists at its stage, though most are not in the
    // starting collection's description.
    let pipeline = r#"[
        {"$lookup": {"from": "owners", "localField": "ownerId", "foreignField": "_id", "as": "o"}},
        {"$unwind": "$o"},
        {"$unwind": {"path": "$visits", "includeArrayIndex": "i"}},
        {"$match": {"visits.cost": {"$gt": 10}, "o.city": "Oslo", "i": {"$gte": 0}}},
        {"$addFields": {"vet": "$visits.vet", "year": {"$year": "$born"}}},
        {"$group": {"_id": {"city": "$o.city", "vet": "$vet"}, "total": {"$sum": "$visits.cost"},
                    "first": {"$first": "$owner"}, "names": {"$push": {"n": "$name"}}}},
        {"$match": {"_id.city": "Oslo", "total": {"$gt": 0}, "first.name": "Ann", "names.n": "Tom"}},
        {"$project": {"_id": 0, "city": "$_id.city", "total": 1, "owner": "$first.name"}},
        {"$sort": {"total": -1, "city": 1}},
        {"$project": {"owner": 0}},
        {"$match": {"city": {"$in": ["Oslo", "Rome"]}}}
    ]"#;
    let (answer, requests) = ask_checked(&[reply("animals", pipeline)]).await;

    assert!(answer.repairs.is_empty(), "{:?}", answer.repairs);
    assert_eq!(requests.len(), 1);
    assert!(answer.unchecked.is_empty());
    assert_eq!(
        answer.rows.unwrap(),
        vec![
            doc! { "city": "Oslo", "total": 50 },
            doc! { "city": "Oslo", "total": 30 },
        ]
    );
}

#[tokio::test]
async fn fields_after_a_stage_the_ir_cannot_represent_are_not_checked() {
    let (answer, requests) = ask_checked(&[reply(
        "animals",
        r#"[{"$sample": {"size": 5}}, {"$match": {"anything": {"$exists": false}}}, {"$count": "count"}]"#,
    )])
    .await;

    assert!(answer.repairs.is_empty(), "{:?}", answer.repairs);
    assert_eq!(requests.len(), 1);
    // As `ir-coverage` counts it: the stage, and the operator unknown anywhere.
    assert_eq!(answer.unchecked, ["$sample stage", "$sample"]);
}

#[tokio::test]
async fn a_candidate_still_failing_the_check_after_two_repairs_runs_anyway() {
    let bad = reply(
        "animals",
        r#"[{"$match": {"colour": "red"}}, {"$count": "count"}]"#,
    );
    let (answer, requests) = ask_checked(&[bad.clone(), bad.clone(), bad.clone()]).await;

    assert_eq!(requests.len(), 3);
    assert_eq!(answer.repairs.len(), 2);
    assert!(
        answer
            .repairs
            .iter()
            .all(|r| r.kind == RepairKind::StaticCheck)
    );
    // Run as it is: an empty match counts nothing.
    assert_eq!(answer.rows.unwrap(), Vec::<mongodb::bson::Document>::new());
    assert_eq!(answer.check_problems.len(), 1);
    assert_eq!(answer.check_problems[0].kind, "unknown_field");
}

#[tokio::test]
async fn the_ir_arm_also_repairs_execution_errors_after_its_static_repairs() {
    let bad_field = reply(
        "animals",
        r#"[{"$match": {"colour": "red"}}, {"$count": "count"}]"#,
    );
    let (answer, requests) = ask_checked(&[
        bad_field.clone(),
        bad_field.clone(),
        reply("animals", BAD_STAGE),
        reply("animals", BAD_STAGE),
        reply("animals", COUNT_CATS),
    ])
    .await;

    assert_eq!(requests.len(), 5);
    let kinds: Vec<RepairKind> = answer.repairs.iter().map(|r| r.kind).collect();
    assert_eq!(
        kinds,
        [
            RepairKind::StaticCheck,
            RepairKind::StaticCheck,
            RepairKind::Execution,
            RepairKind::Execution
        ]
    );
    assert_eq!(answer.rows.unwrap(), vec![doc! { "count": 1 }]);
}

#[tokio::test]
async fn the_control_arm_runs_a_pipeline_the_static_check_would_reject() {
    let (data, mongo) = zoo().await;
    let llm = ScriptedLlm::new([reply(
        "animals",
        r#"[{"$match": {"colour": "red"}}, {"$count": "count"}]"#,
    )]);
    let engine = Engine::new(
        llm,
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    )
    .with_repair(Repair::Execution);
    let sample = engine.dataset().sample("dev_7").unwrap().clone();
    let answer = engine.ask(&sample).await.unwrap();

    assert!(answer.repairs.is_empty());
    assert!(answer.check_problems.is_empty());
    assert_eq!(engine.llm().requests().len(), 1);
}

#[tokio::test]
async fn excluding_a_field_the_documents_do_not_have_is_not_an_error() {
    // `$count` leaves no `_id`; MongoDB excludes a missing field silently.
    let (answer, requests) = ask_checked(&[reply(
        "animals",
        r#"[{"$count": "n"}, {"$project": {"_id": 0, "colour": 0}}]"#,
    )])
    .await;

    assert!(answer.repairs.is_empty(), "{:?}", answer.repairs);
    assert_eq!(requests.len(), 1);
}

#[tokio::test]
async fn a_field_the_description_types_only_as_null_is_not_type_checked() {
    let (data, mongo) = zoo().await;
    describe(data.path(), "animals", &[&field("species", "Null")]);
    let llm = ScriptedLlm::new([reply("animals", COUNT_CATS)]);
    let engine = Engine::new(
        llm,
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    )
    .with_repair(Repair::StaticCheck);
    let sample = engine.dataset().sample("dev_7").unwrap().clone();
    let answer = engine.ask(&sample).await.unwrap();

    assert!(answer.repairs.is_empty(), "{:?}", answer.repairs);
}

#[tokio::test]
async fn a_path_through_an_array_of_documents_is_an_array_in_an_expression() {
    // `$visits.vet` is every visit's vet: an array of strings to unwind.
    let (answer, requests) = ask_checked(&[reply(
        "animals",
        r#"[{"$project": {"vets": "$visits.vet"}}, {"$unwind": "$vets"}, {"$count": "count"}]"#,
    )])
    .await;

    assert!(answer.repairs.is_empty(), "{:?}", answer.repairs);
    assert_eq!(requests.len(), 1);
    assert_eq!(answer.rows.unwrap(), vec![doc! { "count": 3 }]);
}
