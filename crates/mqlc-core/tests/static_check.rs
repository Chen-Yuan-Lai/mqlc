//! `ir::GoldCheck`: the static check on gold pipelines, which are right by
//! definition, so everything it reports is a false positive. Train only.

mod support;

use std::fs;
use std::path::Path;

use mqlc_core::Dataset;
use mqlc_core::ir::{CheckKind, GoldCheck};
use support::{dataset_dir, field};

fn add_train_sample(data: &Path, sample_id: &str, db_id: &str, pipeline: &str) {
    let line = format!(
        r#"{{"split": "train", "db_id": "{db_id}", "question": "?", "evidence": "", "mongodb_evidence": "", "question_id": "1", "sample_id": "{sample_id}", "SQL": "", "MQL": {{"collection": "animals", "aggregation_pipeline": {pipeline}}}}}"#
    );
    let path = data.join("aptmqlbench_data.jsonl");
    let mut text = fs::read_to_string(&path).unwrap();
    text.push_str(&line);
    text.push('\n');
    fs::write(path, text).unwrap();
}

fn describe_train(data: &Path, db_id: &str, fields: &[String]) {
    let dir = data.join(format!("databases/train/{db_id}/collections_description"));
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("animals.jsonl"), fields.join("\n")).unwrap();
}

/// Two train databases with three gold pipelines, two of them flagged, and
/// the dev pets database of `dataset_dir`.
fn data() -> tempfile::TempDir {
    let data = dataset_dir();
    let fields = [field("name", "String"), field("born", "Date")];
    describe_train(data.path(), "zoo", &fields);
    describe_train(data.path(), "farm", &fields);
    add_train_sample(
        data.path(),
        "train_1",
        "zoo",
        r#"[{"$match": {"name": "Tom"}}]"#,
    );
    add_train_sample(
        data.path(),
        "train_2",
        "zoo",
        r#"[{"$match": {"colour": "red"}}]"#,
    );
    add_train_sample(
        data.path(),
        "train_3",
        "farm",
        r#"[{"$match": {"born": "2020"}}]"#,
    );
    data
}

#[test]
fn counts_the_train_gold_pipelines_the_check_flags_by_kind_and_database() {
    let data = data();
    let dataset = Dataset::open(data.path()).unwrap();

    let report = GoldCheck::of(&dataset, None).unwrap();

    // The dev questions are left out: tuning the check on dev would leak.
    assert_eq!(report.samples, 3);
    assert_eq!(report.flagged, 2);
    assert_eq!(
        report.by_kind.into_iter().collect::<Vec<_>>(),
        [(CheckKind::UnknownField, 1), (CheckKind::TypeMismatch, 1)]
    );
    assert_eq!(report.by_db["zoo"].samples, 2);
    assert_eq!(report.by_db["zoo"].flagged, 1);
    assert_eq!(report.by_db["farm"].flagged, 1);
    let flagged: Vec<&str> = report.errors.iter().map(|e| e.sample_id.as_str()).collect();
    assert_eq!(flagged, ["train_2", "train_3"]);
    assert!(report.errors[0].message.contains("colour"));
}

#[test]
fn can_be_limited_to_one_database() {
    let data = data();
    let dataset = Dataset::open(data.path()).unwrap();

    let report = GoldCheck::of(&dataset, Some("farm")).unwrap();

    assert_eq!(report.samples, 1);
    assert_eq!(report.flagged, 1);
    assert_eq!(report.by_db.keys().collect::<Vec<_>>(), ["farm"]);
}
