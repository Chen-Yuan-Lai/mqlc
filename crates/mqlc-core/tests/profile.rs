mod support;

use std::collections::BTreeMap;

use mongodb::bson::{Bson, DateTime, doc, oid::ObjectId};
use mqlc_core::mdl::{Column, MdlProject};
use mqlc_core::profile::{self, ProfileOptions};
use support::{DB_ID, mongo_with_collections};

/// The WrenAI MDL JSON schema (Canner/wren-engine, mcp-server/mdl.schema.json).
const WREN_MDL_SCHEMA: &str = include_str!("fixtures/wren-mdl.schema.json");

fn date(s: &str) -> DateTime {
    DateTime::parse_rfc3339_str(s).unwrap()
}

/// Three pets with nested owners, arrays of scalars and of documents, and
/// fields whose type or presence varies.
fn animals() -> Vec<mongodb::bson::Document> {
    vec![
        doc! {
            "_id": 1, "name": "Tom", "born": date("2020-05-01T00:00:00Z"), "weight": 4,
            "owner": { "name": "Ann", "address": { "city": "Oslo" } },
            "tags": ["indoor", "old"],
            "visits": [{ "vet": "V1", "cost": 10 }, { "vet": "V2", "cost": 12.5 }],
            "collar": { "color": "red" },
        },
        doc! {
            "_id": 2, "name": "Rex", "born": date("2021-01-01T00:00:00Z"), "weight": 30.5,
            "owner": { "name": "Bob" },
            "tags": [],
            "visits": [{ "vet": "V1" }],
            "chip": Bson::Null,
        },
        doc! {
            "_id": 3, "name": "Kit", "weight": "unknown",
            "owner": Bson::Null,
            "visits": [{ "vet": "V3", "cost": 8 }],
            "collar": [{ "color": "blue" }],
        },
    ]
}

fn column<'a>(project: &'a MdlProject, model: &str, name: &str) -> &'a Column {
    project
        .mdl
        .models
        .iter()
        .find(|m| m.name == model)
        .unwrap_or_else(|| panic!("no model {model}"))
        .columns
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("no column {model}.{name}"))
}

fn props(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[tokio::test]
async fn profiles_nested_paths_arrays_and_embedded_documents_into_mqlc_properties() {
    let mongo = mongo_with_collections([
        ("animals", animals()),
        (
            "shelters",
            vec![doc! { "_id": ObjectId::new(), "name": "North" }],
        ),
    ])
    .await;

    let project = profile::profile(&mongo.client, DB_ID, &ProfileOptions::default())
        .await
        .unwrap();

    assert_eq!(project.mdl.schema, DB_ID);
    let models: Vec<_> = project.mdl.models.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(models, ["animals", "shelters"]);
    let animals = &project.mdl.models[0];
    assert_eq!(animals.table_reference.table, "animals");
    assert_eq!(animals.properties["mqlc.sampled"], 3);
    assert_eq!(animals.properties["mqlc.documents"], 3);
    // Children follow their parent, in the order fields were first seen.
    let columns: Vec<_> = animals.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        columns,
        [
            "_id",
            "name",
            "born",
            "weight",
            "owner",
            "owner.name",
            "owner.address",
            "owner.address.city",
            "tags",
            "visits",
            "visits.vet",
            "visits.cost",
            "collar",
            "collar.color",
            "chip",
        ]
    );

    let name = column(&project, "animals", "name");
    assert_eq!((name.r#type.as_str(), name.not_null), ("String", true));
    assert_eq!(
        name.properties,
        props(&[
            ("mqlc.path", "name"),
            ("mqlc.presence", "3/3"),
            ("mqlc.types", "String:3"),
        ])
    );

    let born = column(&project, "animals", "born");
    assert_eq!((born.r#type.as_str(), born.not_null), ("Date", false));
    assert_eq!(born.properties["mqlc.presence"], "2/3");

    // Polymorphic: types by frequency, ties in the order first seen.
    let weight = column(&project, "animals", "weight");
    assert_eq!(weight.r#type, "Int32 | Double | String");
    assert_eq!(weight.properties["mqlc.types"], "Int32:1,Double:1,String:1");

    let owner = column(&project, "animals", "owner");
    assert_eq!(
        (owner.r#type.as_str(), owner.not_null),
        ("Document | Null", false)
    );
    assert_eq!(owner.properties["mqlc.embedded"], "true");
    assert!(!owner.properties.contains_key("mqlc.array"));

    // Presence is relative to the enclosing document: every owner has a name,
    // one owner in two has an address. A pet may have no owner, so the name
    // may be missing.
    let owner_name = column(&project, "animals", "owner.name");
    assert_eq!(
        owner_name.properties,
        props(&[
            ("mqlc.path", "owner.name"),
            ("mqlc.presence", "2/2"),
            ("mqlc.types", "String:2"),
        ])
    );
    assert!(!owner_name.not_null);
    assert_eq!(
        column(&project, "animals", "owner.address").properties["mqlc.presence"],
        "1/2"
    );
    assert_eq!(
        column(&project, "animals", "owner.address.city").properties["mqlc.path"],
        "owner.address.city"
    );

    let tags = column(&project, "animals", "tags");
    assert_eq!(tags.r#type, "Array");
    assert_eq!(
        tags.properties,
        props(&[
            ("mqlc.path", "tags"),
            ("mqlc.presence", "2/3"),
            ("mqlc.types", "Array:2"),
            ("mqlc.array", "true"),
            ("mqlc.elementTypes", "String"),
        ])
    );

    let visits = column(&project, "animals", "visits");
    assert_eq!(
        visits.properties,
        props(&[
            ("mqlc.path", "visits"),
            ("mqlc.presence", "3/3"),
            ("mqlc.types", "Array:3"),
            ("mqlc.array", "true"),
            ("mqlc.elementTypes", "Document"),
            ("mqlc.embedded", "true"),
        ])
    );
    // Fields of array elements: presence over the 4 elements. Every visit
    // has a vet, but an array may be empty, so the vet may be missing.
    let vet = column(&project, "animals", "visits.vet");
    assert_eq!(vet.properties["mqlc.presence"], "4/4");
    assert!(!vet.not_null);
    let cost = column(&project, "animals", "visits.cost");
    assert_eq!(cost.r#type, "Int32 | Double");
    assert_eq!(
        cost.properties,
        props(&[
            ("mqlc.path", "visits.cost"),
            ("mqlc.presence", "3/4"),
            ("mqlc.types", "Int32:2,Double:1"),
            ("mqlc.inArray", "visits"),
        ])
    );

    // Sometimes an embedded document, sometimes an array of them: its fields
    // are in an array whichever shape came first.
    assert_eq!(
        column(&project, "animals", "collar").r#type,
        "Document | Array"
    );
    assert_eq!(
        column(&project, "animals", "collar.color").properties["mqlc.inArray"],
        "collar"
    );

    let chip = column(&project, "animals", "chip");
    assert_eq!((chip.r#type.as_str(), chip.not_null), ("Null", false));

    assert_eq!(column(&project, "shelters", "_id").r#type, "ObjectId");
    assert!(!project.instructions.is_empty());
}

#[tokio::test]
async fn samples_the_first_documents_by_id_so_profiles_are_reproducible() {
    let mongo = mongo_with_collections([("animals", animals())]).await;

    let project = profile::profile(&mongo.client, DB_ID, &ProfileOptions { sample_size: 2 })
        .await
        .unwrap();

    let animals = &project.mdl.models[0];
    assert_eq!(animals.properties["mqlc.sampled"], 2);
    assert_eq!(animals.properties["mqlc.documents"], 3);
    assert_eq!(
        column(&project, "animals", "born").properties["mqlc.presence"],
        "2/2"
    );
    assert_eq!(
        column(&project, "animals", "weight").r#type,
        "Int32 | Double"
    );
}

#[tokio::test]
async fn the_mdl_is_valid_wren_mdl_and_reloads_unchanged() {
    let mongo = mongo_with_collections([("animals", animals())]).await;
    let project = profile::profile(&mongo.client, DB_ID, &ProfileOptions::default())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();

    project.save(dir.path()).unwrap();

    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("mdl.json")).unwrap())
            .unwrap();
    let schema: serde_json::Value = serde_json::from_str(WREN_MDL_SCHEMA).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let errors: Vec<_> = validator
        .iter_errors(&written)
        .map(|e| e.to_string())
        .collect();
    assert!(errors.is_empty(), "{errors:#?}");
    assert_eq!(MdlProject::load(dir.path()).unwrap(), project);
}
