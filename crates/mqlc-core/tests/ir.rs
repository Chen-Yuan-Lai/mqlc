//! IR round-trip over every gold pipeline, and IR coverage per split. Needs
//! the AptMQL-Bench checkout (`scripts/fetch-data.sh`, or `MQLC_DATA_DIR`) but
//! no MongoDB.

use std::path::PathBuf;

use mqlc_core::dataset::GoldMql;
use mqlc_core::ir::{
    Clause, Condition, Coverage, Expr, ExprOp, Filter, Pipeline, Projection, Stage, Unsupported,
};
use mqlc_core::mongodb::bson::{Bson, Document};
use mqlc_core::{Dataset, Sample};
use serde_json::json;

fn dataset() -> Dataset {
    let dir = std::env::var_os("MQLC_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/AptMQL-Bench")
        });
    Dataset::open(&dir).unwrap_or_else(|e| {
        panic!("{e}: these tests need the AptMQL-Bench checkout (scripts/fetch-data.sh)")
    })
}

fn bson_pipeline(json: &serde_json::Value) -> Vec<Bson> {
    match Bson::try_from(json.clone()).unwrap() {
        Bson::Array(stages) => stages,
        other => panic!("not a pipeline: {other}"),
    }
}

/// Stages whose argument is a fixed set of named options: MongoDB ignores the
/// order of those keys, and nowhere else.
const OPTION_STAGES: [&str; 5] = [
    "$lookup",
    "$unwind",
    "$unionWith",
    "$replaceRoot",
    "$setWindowFields",
];

/// Sorts the option keys of every option stage, including those in nested
/// `$lookup`, `$unionWith` and `$facet` pipelines, so that two pipelines are
/// semantically equal exactly when they are equal key order and all.
fn normalise(stages: &mut [Bson]) {
    for stage in stages {
        let Bson::Document(stage) = stage else {
            continue;
        };
        for (name, arg) in stage.iter_mut() {
            let Bson::Document(arg) = arg else { continue };
            for (key, value) in arg.iter_mut() {
                let nested = key == "pipeline" || name == "$facet";
                if let (true, Bson::Array(inner)) = (nested, value) {
                    normalise(inner);
                }
            }
            if OPTION_STAGES.contains(&name.as_str()) {
                let mut keys: Vec<(String, Bson)> = std::mem::take(arg).into_iter().collect();
                keys.sort_by(|a, b| a.0.cmp(&b.0));
                *arg = keys.into_iter().collect();
            }
        }
    }
}

/// Deep equality where document key order counts (bson's own `==` ignores it)
/// and so do numeric types.
fn same(a: &Bson, b: &Bson) -> bool {
    match (a, b) {
        (Bson::Document(a), Bson::Document(b)) => same_doc(a, b),
        (Bson::Array(a), Bson::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same(a, b))
        }
        _ => a == b,
    }
}

fn same_doc(a: &Document, b: &Document) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|((ka, va), (kb, vb))| ka == kb && same(va, vb))
}

fn round_trips(json: &serde_json::Value) -> bool {
    let pipeline = Pipeline::from_extended_json(json).unwrap();
    let mut before = bson_pipeline(json);
    let mut after = bson_pipeline(&pipeline.to_extended_json());
    normalise(&mut before);
    normalise(&mut after);
    same(&Bson::Array(before), &Bson::Array(after))
}

#[test]
fn every_gold_pipeline_round_trips_unchanged() {
    let dataset = dataset();
    assert_eq!(dataset.samples().len(), 3186);
    let broken: Vec<&str> = dataset
        .samples()
        .iter()
        .filter(|s| !round_trips(&s.mql.aggregation_pipeline))
        .map(|s| s.sample_id.as_str())
        .collect();
    assert!(
        broken.is_empty(),
        "{} do not round-trip: {broken:?}",
        broken.len()
    );
}

#[test]
fn at_least_95_percent_of_train_needs_no_raw_stage() {
    let dataset = dataset();
    let coverage = Coverage::of(dataset.samples()).unwrap();
    // `cargo test -- --nocapture` prints the report.
    println!("{coverage}");
    let train = &coverage.splits["train"];
    assert_eq!(train.samples, 1762);
    assert!(
        train.rate() >= 0.95,
        "train coverage {:.1}%",
        100.0 * train.rate()
    );
    assert_eq!(coverage.splits["dev"].samples, 1424);
}

#[test]
fn keeps_64_bit_integers_that_would_fit_in_32_bits() {
    let json = json!([{"$match": {"n": {"$numberLong": "5"}}}, {"$limit": {"$numberLong": "3"}}]);
    let back = bson_pipeline(
        &Pipeline::from_extended_json(&json)
            .unwrap()
            .to_extended_json(),
    );
    assert!(same(&Bson::Array(back), &Bson::Array(bson_pipeline(&json))));
}

#[test]
fn keeps_what_it_cannot_represent_as_raw_stages_and_says_why() {
    let json = json!([
        {"$lookup": {"from": "b", "as": "bs", "pipeline": [
            {"$project": {"n": {"$toUpper": "$name"}}},
            {"$limit": 1},
        ]}},
        {"$sample": {"size": 3}},
        {"$match": {"a": 1}},
    ]);
    let pipeline = Pipeline::from_extended_json(&json).unwrap();
    let reasons: Vec<&Unsupported> = pipeline.raw_stages().iter().map(|r| &r.reason).collect();
    assert_eq!(
        reasons,
        [
            &Unsupported::Operator("$toUpper".into()),
            &Unsupported::Stage("$sample".into()),
        ]
    );
    assert!(matches!(pipeline.stages[0], Stage::Lookup(_)));
    assert!(matches!(pipeline.stages[2], Stage::Match(_)));
    assert!(round_trips(&json));
}

#[test]
fn reads_each_value_in_the_context_mongodb_reads_it() {
    let json = json!([
        {"$match": {"name": "$literal-looking", "$expr": {"$gt": ["$a", "$$limit"]}}},
        {"$project": {"_id": 0, "kept": true, "also": 2, "s": "$a.b", "lit": {"$literal": "$x"}}},
    ]);
    let pipeline = Pipeline::from_extended_json(&json).unwrap();
    let [Stage::Match(Filter(clauses)), Stage::Project(fields)] = &pipeline.stages[..] else {
        panic!("{:?}", pipeline.stages);
    };
    assert_eq!(
        clauses,
        &[
            Clause::Field(
                "name".into(),
                Condition::Equals(Bson::from("$literal-looking"))
            ),
            Clause::Expr(Expr::Op(
                ExprOp::Gt,
                Box::new(Expr::Array(vec![
                    Expr::Path("a".into()),
                    Expr::Variable("limit".into()),
                ])),
            )),
        ]
    );
    assert_eq!(
        fields,
        &[
            ("_id".into(), Projection::Exclude(Bson::Int32(0))),
            ("kept".into(), Projection::Include(Bson::Boolean(true))),
            ("also".into(), Projection::Include(Bson::Int32(2))),
            ("s".into(), Projection::Computed(Expr::Path("a.b".into()))),
            (
                "lit".into(),
                Projection::Computed(Expr::Op(
                    ExprOp::Literal,
                    Box::new(Expr::Literal(Bson::from("$x"))),
                )),
            ),
        ]
    );
}

#[test]
fn elem_match_on_an_array_of_scalars_takes_operators() {
    let json = json!([{"$match": {"scores": {"$elemMatch": {"$gt": 5, "$lt": 9}}}}]);
    let pipeline = Pipeline::from_extended_json(&json).unwrap();
    assert!(
        pipeline.raw_stages().is_empty(),
        "{:?}",
        pipeline.raw_stages()
    );
    assert!(round_trips(&json));
}

#[test]
fn sort_orders_keep_the_type_they_were_written_in() {
    let json = json!([{"$sort": {"a": 1.0, "b": {"$numberLong": "-1"}}}]);
    assert!(
        Pipeline::from_extended_json(&json)
            .unwrap()
            .raw_stages()
            .is_empty()
    );
    assert!(round_trips(&json));
}

#[test]
fn coverage_counts_every_unsupported_operator_of_a_raw_stage() {
    let sample = |id: &str, pipeline| Sample {
        split: "dev".into(),
        db_id: "db".into(),
        question: String::new(),
        evidence: String::new(),
        mongodb_evidence: String::new(),
        question_id: id.into(),
        sample_id: id.into(),
        sql: String::new(),
        mql: GoldMql {
            collection: "c".into(),
            aggregation_pipeline: pipeline,
        },
    };
    let samples = [
        sample(
            "dev_1",
            json!([{"$project": {"n": {"$strLenCP": {"$substr": ["$a", 0, 2]}}}}]),
        ),
        sample(
            "dev_2",
            json!([{"$project": {"n": {"$substr": ["$a", 0, 2]}}}]),
        ),
        sample("dev_3", json!([{"$count": "n"}])),
    ];
    let dev = &Coverage::of(&samples).unwrap().splits["dev"];
    assert_eq!((dev.samples, dev.covered), (3, 1));
    let counts: Vec<(String, usize)> = dev
        .unsupported
        .iter()
        .map(|(u, n)| (u.to_string(), *n))
        .collect();
    assert_eq!(counts, [("$strLenCP".into(), 1), ("$substr".into(), 2)]);
}
