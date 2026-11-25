//! Smoke test of the MCP server (ADR 0007): the real binary over stdio,
//! against a real MongoDB.

use mqlc_core::ir::{Pipeline, Schema};
use mqlc_core::mongodb::Client;
use mqlc_core::mongodb::bson::{Document, doc};
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use rmcp::{RoleClient, ServiceExt};
use serde_json::json;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage};

const DB_ID: &str = "pets";

struct Server {
    client: RunningService<RoleClient, ()>,
    _mongo: ContainerAsync<GenericImage>,
}

/// The server, connected to a MongoDB holding database `DB_ID`.
async fn server() -> Server {
    let container = GenericImage::new("mongo", "8.2")
        .with_exposed_port(27017.tcp())
        .with_wait_for(WaitFor::message_on_stdout("Waiting for connections"))
        .start()
        .await
        .unwrap();
    let port = container.get_host_port_ipv4(27017).await.unwrap();
    let uri = format!("mongodb://127.0.0.1:{port}/?directConnection=true");
    let mongo = Client::with_uri_str(&uri).await.unwrap();
    let animals: Vec<Document> = vec![
        doc! {
            "name": "Tom", "species": "cat", "owner": { "name": "Ann" },
            "age": 3, "nickname": null, "visits": [{ "vet": "Kim" }],
        },
        doc! {
            "name": "Rex", "species": "dog", "owner": { "name": "Bob" },
            "age": "unknown", "nickname": null, "visits": [],
        },
    ];
    mongo
        .database(DB_ID)
        .collection::<Document>("animals")
        .insert_many(animals)
        .await
        .unwrap();
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_mqlc-mcp"));
    command.arg(&uri);
    let client = ().serve(TokioChildProcess::new(command).unwrap()).await.unwrap();
    Server {
        client,
        _mongo: container,
    }
}

#[tokio::test]
async fn lists_its_two_tools() {
    let server = server().await;
    let mut names: Vec<String> = server
        .client
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    names.sort();
    assert_eq!(names, ["profile_schema", "validate_pipeline"]);
}

/// The text of a successful call.
async fn call(server: &Server, tool: &'static str, arguments: serde_json::Value) -> String {
    let result = server
        .client
        .call_tool(
            CallToolRequestParams::new(tool).with_arguments(arguments.as_object().unwrap().clone()),
        )
        .await
        .unwrap();
    let text = result.content[0].as_text().unwrap().text.clone();
    assert_eq!(result.is_error, Some(false), "{text}");
    text
}

#[tokio::test]
async fn profile_schema_describes_the_collections() {
    let server = server().await;
    let text = call(&server, "profile_schema", json!({ "database": DB_ID })).await;
    assert!(text.starts_with("Collection: animals\n"), "{text}");
    assert!(
        text.contains(r#"{"field_name": "owner.name", "data_type": "String""#),
        "{text}"
    );
}

/// The errors match those of the CLI's static check (`--repair
/// static-check`) given an official description of the same fields.
#[tokio::test]
async fn validate_pipeline_reports_what_the_cli_check_reports() {
    let server = server().await;
    let pipeline = json!([
        { "$match": { "specie": "cat" } },
        { "$match": { "owner.name": 3 } },
        { "$match": { "visits.vtt": "Kim" } },
        { "$match": { "age": true } },
        // Only ever null: its type is not known, so nothing to report.
        { "$match": { "nickname": 3 } },
        { "$match": { "visits.vet": 3 } },
        { "$unwind": "$name" },
    ]);
    let text = call(
        &server,
        "validate_pipeline",
        json!({ "database": DB_ID, "collection": "animals", "pipeline": pipeline }),
    )
    .await;
    let reply: serde_json::Value = serde_json::from_str(&text).unwrap();

    let official = Schema::from_official(&[(
        "animals".to_string(),
        [
            ("name", "String"),
            ("species", "String"),
            ("owner", "Document"),
            ("owner.name", "String"),
            ("age", "Number, String"),
            ("nickname", "Null"),
            ("visits", "Array"),
            ("visits.vet", "String"),
        ]
        .map(|(f, t)| (f.to_string(), t.to_string()))
        .to_vec(),
    )]);
    let cli: Vec<_> = Pipeline::from_extended_json(&pipeline)
        .unwrap()
        .check("animals", &official)
        .into_iter()
        .map(|e| json!({ "kind": e.kind.name(), "message": e.message }))
        .collect();
    let kinds: Vec<_> = cli.iter().map(|e| e["kind"].clone()).collect();
    assert_eq!(
        kinds,
        [
            "unknown_field",
            "type_mismatch",
            "unknown_field",
            "type_mismatch",
            "type_mismatch",
            "unwind_non_array"
        ]
    );
    assert_eq!(reply, json!({ "errors": cli, "unchecked": [] }));
}

#[tokio::test]
async fn profile_schema_refuses_a_database_with_no_collections() {
    let server = server().await;
    let result = server
        .client
        .call_tool(
            CallToolRequestParams::new("profile_schema")
                .with_arguments(json!({ "database": "petz" }).as_object().unwrap().clone()),
        )
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert_eq!(
        result.content[0].as_text().unwrap().text,
        "database `petz` has no collections"
    );
}
