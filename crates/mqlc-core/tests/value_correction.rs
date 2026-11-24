//! Value correction: a literal of the pipeline that no document has is fed
//! back with the closest values the data does have, through the
//! single-question API (ADR 0007).

mod support;

use mongodb::bson::{Document, doc};
use mqlc_core::testing::ScriptedLlm;
use mqlc_core::{Answer, Dataset, Engine, LlmRequest, RepairKind, Usage};
use support::{dataset_dir, mongo_with_collections, reply};

fn animals() -> Vec<Document> {
    vec![
        doc! { "name": "Tom", "species": "cat", "owner": { "name": "Smith, Alice" } },
        doc! { "name": "Rex", "species": "dog", "owner": { "name": "Jones, Bob" } },
        doc! { "name": "Kit", "species": "cat", "owner": { "name": "Smith, Alice" } },
    ]
}

/// Asks dev_7 with these replies, value correction on, of `animals()`.
async fn ask_corrected(replies: &[String]) -> (Answer, Vec<LlmRequest>) {
    ask_corrected_in([("animals", animals())], replies).await
}

/// Asks dev_7 with these replies, value correction on, of a database with
/// these collections.
async fn ask_corrected_in(
    collections: impl IntoIterator<Item = (&str, Vec<Document>)>,
    replies: &[String],
) -> (Answer, Vec<LlmRequest>) {
    let data = dataset_dir();
    let mongo = mongo_with_collections(collections).await;
    let llm = ScriptedLlm::new(replies.to_vec()).with_usage(Usage {
        input_tokens: 1000,
        output_tokens: 100,
        ..Usage::default()
    });
    let engine = Engine::new(
        llm,
        mongo.client.clone(),
        Dataset::open(data.path()).unwrap(),
    )
    .with_value_correction(true);
    let sample = engine.dataset().sample("dev_7").unwrap().clone();
    let answer = engine.ask(&sample).await.unwrap();
    (answer, engine.llm().requests())
}

fn count_owned_by(name: &str) -> String {
    format!(r#"[{{"$match": {{"owner.name": "{name}"}}}}, {{"$count": "count"}}]"#)
}

#[tokio::test]
async fn a_literal_no_document_has_is_fed_back_with_the_closest_values() {
    let first = reply("animals", &count_owned_by("Alice Smith"));
    let (answer, requests) = ask_corrected(&[
        first.clone(),
        reply("animals", &count_owned_by("Smith, Alice")),
    ])
    .await;

    assert_eq!(answer.rows.unwrap(), vec![doc! { "count": 2 }]);
    assert_eq!(answer.repairs.len(), 1);
    assert_eq!(answer.repairs[0].kind, RepairKind::ValueCorrection);
    let problem = &answer.repairs[0].problems[0];
    assert_eq!(problem.kind, "missing_value");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].turns[0].reply, first);
    let feedback = &requests[1].turns[0].feedback;
    assert!(feedback.contains("Alice Smith"), "{feedback}");
    assert!(feedback.contains("owner.name"), "{feedback}");
    assert!(feedback.contains("\"Smith, Alice\""), "{feedback}");
}

#[tokio::test]
async fn an_empty_result_whose_literals_all_exist_is_not_corrected() {
    // Both values are in the data, just not in one document.
    let pipeline =
        r#"[{"$match": {"species": "dog", "owner.name": "Smith, Alice"}}, {"$count": "count"}]"#;
    let (answer, requests) = ask_corrected(&[reply("animals", pipeline)]).await;

    assert_eq!(answer.rows.unwrap(), vec![]);
    assert!(answer.repairs.is_empty());
    assert_eq!(requests.len(), 1);
}

#[tokio::test]
async fn values_are_corrected_at_most_twice_and_every_call_counts() {
    let (answer, requests) = ask_corrected(&[
        reply("animals", &count_owned_by("Alice Smith")),
        reply("animals", &count_owned_by("alice smith")),
        reply("animals", &count_owned_by("Smith Alice")),
        reply("animals", &count_owned_by("Smith, Alice")),
    ])
    .await;

    assert_eq!(requests.len(), 3);
    assert_eq!(answer.repairs.len(), 2);
    let usage = answer.total_usage();
    assert_eq!((usage.input_tokens, usage.output_tokens), (3000, 300));
    // The last candidate stands, its value still missing.
    assert_eq!(answer.rows.unwrap(), vec![]);
}

#[tokio::test]
async fn a_missing_value_in_an_in_list_is_corrected_even_when_rows_come_back() {
    let pipeline = r#"[{"$match": {"owner.name": {"$in": ["Jones, Bob", "Alice Smith"]}}}, {"$count": "count"}]"#;
    let (answer, requests) = ask_corrected(&[
        reply("animals", pipeline),
        reply("animals", r#"[{"$count": "count"}]"#),
    ])
    .await;

    assert_eq!(answer.repairs.len(), 1);
    assert_eq!(answer.repairs[0].problems.len(), 1);
    let feedback = &requests[1].turns[0].feedback;
    assert!(feedback.contains("\"Alice Smith\""), "{feedback}");
    assert!(!feedback.contains("has \"Jones, Bob\""), "{feedback}");
    assert_eq!(answer.rows.unwrap(), vec![doc! { "count": 3 }]);
}

#[tokio::test]
async fn a_literal_past_a_lookup_is_looked_for_in_the_joined_collection() {
    let pipeline = |city: &str| {
        format!(
            r#"[{{"$lookup": {{"from": "owners", "localField": "ownerId", "foreignField": "_id", "as": "owner"}}}},
               {{"$unwind": "$owner"}}, {{"$match": {{"owner.city": "{city}"}}}}, {{"$count": "count"}}]"#
        )
    };
    let collections = [
        (
            "animals",
            vec![
                doc! { "name": "Tom", "ownerId": 1 },
                doc! { "name": "Rex", "ownerId": 2 },
            ],
        ),
        (
            "owners",
            vec![
                doc! { "_id": 1, "city": "Oslo" },
                doc! { "_id": 2, "city": "Rome" },
            ],
        ),
    ];
    let (answer, _) = ask_corrected_in(
        collections,
        &[
            reply("animals", &pipeline("oslo")),
            reply("animals", &pipeline("Oslo")),
        ],
    )
    .await;

    assert_eq!(answer.repairs.len(), 1);
    let message = &answer.repairs[0].problems[0].message;
    assert!(message.contains("owners"), "{message}");
    assert!(message.contains("\"Oslo\""), "{message}");
    assert_eq!(answer.rows.unwrap(), vec![doc! { "count": 1 }]);
}

#[tokio::test]
async fn literals_under_or_and_without_similar_values_are_left_alone() {
    let pipeline = r#"[{"$match": {"$or": [{"owner.name": "Alice Smith"}, {"species": "cat"}]}},
                      {"$match": {"name": {"$ne": "Zed"}, "species": {"$in": ["cat", "xyzzy"]}}},
                      {"$count": "count"}]"#;
    let (answer, requests) = ask_corrected(&[reply("animals", pipeline)]).await;

    assert!(answer.repairs.is_empty(), "{:?}", answer.repairs);
    assert_eq!(requests.len(), 1);
    assert_eq!(answer.rows.unwrap(), vec![doc! { "count": 2 }]);
}

#[tokio::test]
async fn many_values_sharing_a_word_do_not_crowd_out_the_closest_one() {
    let mut docs: Vec<_> = (0..1500)
        .map(|i| doc! { "name": "Rex", "owner": { "name": format!("Ann County {i}") } })
        .collect();
    docs.push(doc! { "name": "Tom", "owner": { "name": "Marin County" } });
    let (answer, _) = ask_corrected_in(
        [("animals", docs)],
        &[
            reply("animals", &count_owned_by("marin county")),
            reply("animals", &count_owned_by("Marin County")),
        ],
    )
    .await;

    assert_eq!(answer.repairs.len(), 1);
    let message = &answer.repairs[0].problems[0].message;
    assert!(message.contains("\"Marin County\""), "{message}");
}

#[tokio::test]
async fn a_reply_keeping_the_same_pipeline_ends_the_correction() {
    // The question may mean a value that is not in the data.
    let same = reply("animals", &count_owned_by("Alice Smith"));
    let (answer, requests) = ask_corrected(&[
        same.clone(),
        same.clone(),
        reply("animals", &count_owned_by("Smith, Alice")),
    ])
    .await;

    assert_eq!(requests.len(), 2);
    assert_eq!(answer.repairs.len(), 1);
    assert_eq!(answer.rows.unwrap(), vec![]);
}

#[tokio::test]
async fn values_are_found_by_words_of_two_characters() {
    let docs = vec![
        doc! { "name": "Tom", "owner": { "name": "6PM" } },
        doc! { "name": "Rex", "owner": { "name": "7AM" } },
    ];
    let (answer, _) = ask_corrected_in(
        [("animals", docs)],
        &[
            reply("animals", &count_owned_by("6:00 PM")),
            reply("animals", &count_owned_by("6PM")),
        ],
    )
    .await;

    assert_eq!(answer.repairs.len(), 1);
    let message = &answer.repairs[0].problems[0].message;
    assert!(
        message.contains("closest values there are \"6PM\""),
        "{message}"
    );
}
