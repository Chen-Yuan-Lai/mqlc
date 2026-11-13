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

/// The structural properties: the column's `mqlc.*` properties other than
/// those describing its values.
fn structure(c: &Column) -> BTreeMap<String, String> {
    const VALUES: [&str; 6] = [
        "mqlc.distinct",
        "mqlc.enum",
        "mqlc.topValues",
        "mqlc.min",
        "mqlc.max",
        "mqlc.format",
    ];
    c.properties
        .iter()
        .filter(|(k, _)| !VALUES.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
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
        structure(name),
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
        structure(owner_name),
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
        structure(tags),
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
        structure(visits),
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
        structure(cost),
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

    let project = profile::profile(
        &mongo.client,
        DB_ID,
        &ProfileOptions {
            sample_size: 2,
            ..ProfileOptions::default()
        },
    )
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
    let mongo = mongo_with_collections([
        ("animals", animals()),
        ("owners", vec![doc! { "_id": "Ann" }]),
        ("litters", vec![doc! { "_id": 1, "ownerId": "Ann" }]),
    ])
    .await;
    let project = profile::profile(&mongo.client, DB_ID, &ProfileOptions::default())
        .await
        .unwrap();
    assert!(!project.mdl.relationships.is_empty());
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

#[tokio::test]
async fn low_cardinality_fields_list_their_whole_value_domain() {
    let mongo = mongo_with_collections([("animals", animals())]).await;

    let project = profile::profile(&mongo.client, DB_ID, &ProfileOptions::default())
        .await
        .unwrap();

    // Most frequent first, ties in the order first seen; values keep their
    // JSON type, and array elements are the field's values.
    assert_eq!(
        column(&project, "animals", "name").properties["mqlc.enum"],
        r#"["Tom","Rex","Kit"]"#
    );
    assert_eq!(
        column(&project, "animals", "weight").properties["mqlc.enum"],
        r#"[4,30.5,"unknown"]"#
    );
    assert_eq!(
        column(&project, "animals", "tags").properties["mqlc.enum"],
        r#"["indoor","old"]"#
    );
    assert_eq!(
        column(&project, "animals", "visits.vet").properties["mqlc.enum"],
        r#"["V1","V2","V3"]"#
    );
    assert_eq!(
        column(&project, "animals", "visits.vet").properties["mqlc.distinct"],
        "3"
    );
    // Nulls are not values; ObjectIds and dates are never enumerated.
    assert!(
        !column(&project, "animals", "chip")
            .properties
            .contains_key("mqlc.enum")
    );
    assert!(
        !column(&project, "animals", "born")
            .properties
            .contains_key("mqlc.enum")
    );
}

/// Thirty parts: more distinct values than an enum lists.
fn parts() -> Vec<mongodb::bson::Document> {
    (1..=30)
        .map(|i| {
            let city = match i {
                1..=8 => "Oslo".to_string(),
                9..=11 => "Bergen".to_string(),
                _ => format!("Town {i}"),
            };
            doc! {
                "_id": i,
                "code": format!("P-{i:03}"),
                "serial": i.to_string(),
                "city": city,
                "price": i * 10,
                "made": date(&format!("2020-01-{i:02}T00:00:00Z")),
                "note": format!("a note far too long to be worth listing as a value, number {i}"),
                "lot": if i == 1 { Bson::String("L-1".to_string()) } else { Bson::Int32(i) },
            }
        })
        .collect()
}

#[tokio::test]
async fn high_cardinality_fields_give_statistics_common_values_and_format() {
    let mongo = mongo_with_collections([("parts", parts())]).await;

    let project = profile::profile(&mongo.client, DB_ID, &ProfileOptions::default())
        .await
        .unwrap();

    let city = column(&project, "parts", "city");
    assert!(!city.properties.contains_key("mqlc.enum"));
    assert_eq!(city.properties["mqlc.distinct"], "21");
    // Up to the five most frequent, with counts: only values seen more than
    // once are common.
    assert_eq!(
        city.properties["mqlc.topValues"],
        r#"[["Oslo",8],["Bergen",3]]"#
    );
    assert!(
        !column(&project, "parts", "code")
            .properties
            .contains_key("mqlc.topValues")
    );
    // No one shape covers nine values in ten.
    assert!(!city.properties.contains_key("mqlc.format"));

    // The exact shape when one covers nine values in ten, else the shape
    // with runs of any length.
    assert_eq!(
        column(&project, "parts", "code").properties["mqlc.format"],
        r"[A-Z]-\d{3}"
    );
    assert_eq!(
        column(&project, "parts", "serial").properties["mqlc.format"],
        r"\d+"
    );

    // A format describes most of the values, not most of the few strings.
    assert!(
        !column(&project, "parts", "lot")
            .properties
            .contains_key("mqlc.format")
    );

    let price = column(&project, "parts", "price");
    assert_eq!(
        (
            price.properties["mqlc.min"].as_str(),
            price.properties["mqlc.max"].as_str()
        ),
        ("10", "300")
    );
    assert!(!price.properties.contains_key("mqlc.format"));

    let made = column(&project, "parts", "made");
    assert_eq!(made.properties["mqlc.min"], r#""2020-01-01T00:00:00Z""#);
    assert_eq!(made.properties["mqlc.max"], r#""2020-01-30T00:00:00Z""#);
    assert!(!made.properties.contains_key("mqlc.topValues"));

    // Values too long to list are summarized but never shown.
    let note = column(&project, "parts", "note");
    assert_eq!(note.properties["mqlc.distinct"], "30");
    assert!(!note.properties.contains_key("mqlc.topValues"));
}

#[tokio::test]
async fn references_by_object_id_or_a_key_named_for_a_collection_become_relationships() {
    let s1 = ObjectId::new();
    let s2 = ObjectId::new();
    let mongo = mongo_with_collections([
        (
            "pets",
            vec![
                doc! {
                    "_id": "p1", "ownerId": "o1", "home": s1, "licence": { "id": "L1" },
                    "visits": [{ "vetId": "V1" }], "toyIds": ["t1", "t2"], "breederId": "b1",
                    "chipId": "c1", "keeperId": s2,
                },
                doc! {
                    "_id": "p2", "ownerId": "o1", "home": s1, "licence": { "id": "L2" },
                    "visits": [{ "vetId": "V2" }, { "vetId": "V1" }], "breederId": "b9",
                },
                // A copy of the pet's own key is no reference; its mother is.
                doc! {
                    "_id": "p3", "ownerId": "o2", "licence": { "id": "L3" }, "petId": "p3",
                    "motherPetId": "p1",
                },
            ],
        ),
        ("owners", vec![doc! { "_id": "o1" }, doc! { "_id": "o2" }]),
        ("shelters", vec![doc! { "_id": s1 }, doc! { "_id": s2 }]),
        (
            "licences",
            vec![
                doc! { "_id": "L1" },
                doc! { "_id": "L2" },
                doc! { "_id": "L3" },
            ],
        ),
        ("vets", vec![doc! { "_id": "V1" }, doc! { "_id": "V2" }]),
        ("toys", vec![doc! { "_id": "t1" }, doc! { "_id": "t2" }]),
        // Named for keepers, but its ObjectIds are shelters'.
        ("keepers", vec![doc! { "_id": "k1" }]),
        // Half the breeders referred to do not exist: not a reference.
        ("breeders", vec![doc! { "_id": "b1" }]),
    ])
    .await;

    let project = profile::profile(&mongo.client, DB_ID, &ProfileOptions::default())
        .await
        .unwrap();

    let relationships: Vec<_> = project
        .mdl
        .relationships
        .iter()
        .map(|r| {
            (
                r.name.as_str(),
                [r.models[0].as_str(), r.models[1].as_str()],
                serde_json::to_value(r.join_type).unwrap(),
                r.condition.as_str(),
                r.properties["mqlc.path"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        relationships,
        [
            // Values repeat: many pets to one owner.
            (
                "pets.ownerId",
                ["pets", "owners"],
                serde_json::json!("MANY_TO_ONE"),
                "pets.ownerId = owners._id",
                "ownerId",
            ),
            // An ObjectId needs no name to find what it refers to.
            (
                "pets.home",
                ["pets", "shelters"],
                serde_json::json!("MANY_TO_ONE"),
                "pets.home = shelters._id",
                "home",
            ),
            // An `id` is named for the field holding it; every value differs.
            (
                "pets.licence.id",
                ["pets", "licences"],
                serde_json::json!("ONE_TO_ONE"),
                "pets.licence.id = licences._id",
                "licence.id",
            ),
            // In array elements or arrays: a pet has many.
            (
                "pets.visits.vetId",
                ["pets", "vets"],
                serde_json::json!("MANY_TO_MANY"),
                "pets.visits.vetId = vets._id",
                "visits.vetId",
            ),
            (
                "pets.toyIds",
                ["pets", "toys"],
                serde_json::json!("MANY_TO_MANY"),
                "pets.toyIds = toys._id",
                "toyIds",
            ),
            (
                "pets.keeperId",
                ["pets", "shelters"],
                serde_json::json!("ONE_TO_ONE"),
                "pets.keeperId = shelters._id",
                "keeperId",
            ),
            (
                "pets.motherPetId",
                ["pets", "pets"],
                serde_json::json!("ONE_TO_ONE"),
                "pets.motherPetId = pets._id",
                "motherPetId",
            ),
        ]
    );
    assert_eq!(
        project.mdl.relationships[0].properties["mqlc.matched"],
        "2/2"
    );
}

#[tokio::test]
async fn excluded_fields_are_never_read_and_leave_no_value_in_the_mdl() {
    let mongo = mongo_with_collections([("animals", animals())]).await;
    let options = ProfileOptions {
        exclude: vec![
            "animals.owner".to_string(),
            "animals.visits.cost".to_string(),
        ],
        ..ProfileOptions::default()
    };

    let project = profile::profile(&mongo.client, DB_ID, &options)
        .await
        .unwrap();

    let animals = &project.mdl.models[0];
    let columns: Vec<_> = animals.columns.iter().map(|c| c.name.as_str()).collect();
    assert!(columns.contains(&"visits.vet"));
    assert!(
        !columns
            .iter()
            .any(|c| c.starts_with("owner") || *c == "visits.cost"),
        "{columns:?}"
    );
    // Only the paths are kept, so that they still name fields.
    assert_eq!(
        animals.properties["mqlc.excluded"],
        serde_json::json!(["owner", "visits.cost"])
    );
    let dir = tempfile::tempdir().unwrap();
    project.save(dir.path()).unwrap();
    let written = std::fs::read_to_string(dir.path().join("mdl.json")).unwrap()
        + &std::fs::read_to_string(dir.path().join("instructions.json")).unwrap()
        + &project.render();
    for value in ["Ann", "Bob", "Oslo", "12.5"] {
        assert!(!written.contains(value), "{value} leaked");
    }
}

#[tokio::test]
async fn excluding_a_field_that_is_not_there_is_an_error_not_a_silent_leak() {
    let mongo = mongo_with_collections([("animals", animals())]).await;

    for exclude in ["animals.ownr", "pets.owner", "owner"] {
        let options = ProfileOptions {
            exclude: vec![exclude.to_string()],
            ..ProfileOptions::default()
        };
        let err = profile::profile(&mongo.client, DB_ID, &options)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, profile::ProfileError::UnknownExclusion(e) if e == exclude),
            "{err}"
        );
    }
}
