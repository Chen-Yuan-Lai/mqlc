use std::fs;
use std::path::Path;

use mongodb::Client;
use mongodb::bson::Document;
use tempfile::TempDir;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage};

pub const DB_ID: &str = "pets";

/// A trimmed copy of the official prompt file's layout: system prompt, then the
/// three Python string templates.
const PROMPT_FILE: &str = r#"# ROLE
You are an expert MongoDB engineer.
"""

SCHEMA_PROMPT = """<MongoDB Schema Description>

{schema_description}

</MongoDB Schema Description>"""


QUESTION_PROMPT = """<Natural Language Request>
{nl_request}
</Natural Language Request>
{evidence_section}"""


EVIDENCE_SECTION = """
-----------------------------

<Evidence>
{mongodb_evidence}
</Evidence>""""#;

const SAMPLES: [&str; 2] = [
    r#"{"split": "dev", "db_id": "pets", "question": "How many cats are there?", "evidence": "", "mongodb_evidence": "cats refers to species = 'cat'", "question_id": 7, "sample_id": "dev_7", "SQL": "SELECT COUNT(*) FROM pets WHERE species = 'cat'", "MQL": {"collection": "animals", "aggregation_pipeline": [{"$match": {"species": "cat"}}, {"$count": "count"}]}}"#,
    r#"{"split": "dev", "db_id": "pets", "question": "How many dogs are there?", "evidence": "", "mongodb_evidence": "dogs refers to species = 'dog'", "question_id": 8, "sample_id": "dev_8", "SQL": "SELECT COUNT(*) FROM pets WHERE species = 'dog'", "MQL": {"collection": "animals", "aggregation_pipeline": [{"$match": {"species": "dog"}}, {"$count": "count"}]}}"#,
];

const DESCRIPTION: &str = r#"{"field_name": "name", "field_description": "Pet name", "data_type": "String", "required": true, "value_description": ""}
{"field_name": "species", "field_description": "Kind of animal", "data_type": "String", "required": true, "value_description": "cat, dog"}"#;

/// Writes a one-question AptMQL-Bench layout into a temp dir.
pub fn dataset_dir() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(
        root.join("aptmqlbench_data.jsonl"),
        SAMPLES.join("\n") + "\n",
    )
    .unwrap();
    write(
        root,
        "prompts/s51_text2mql_1time_completion.txt",
        PROMPT_FILE,
    );
    write(
        root,
        "databases/dev/pets/collections_description/animals.jsonl",
        DESCRIPTION,
    );
    dir
}

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

pub struct Mongo {
    pub client: Client,
    _container: ContainerAsync<GenericImage>,
}

pub async fn mongo_with(docs: Vec<Document>) -> Mongo {
    let container = GenericImage::new("mongo", "8.2")
        .with_exposed_port(27017.tcp())
        .with_wait_for(WaitFor::message_on_stdout("Waiting for connections"))
        .start()
        .await
        .unwrap();
    let port = container.get_host_port_ipv4(27017).await.unwrap();
    let client = Client::with_uri_str(format!("mongodb://127.0.0.1:{port}/?directConnection=true"))
        .await
        .unwrap();
    client
        .database(DB_ID)
        .collection::<Document>("animals")
        .insert_many(docs)
        .await
        .unwrap();
    Mongo {
        client,
        _container: container,
    }
}

/// An LLM reply in the shape the official prompt asks for.
pub fn reply(collection: &str, pipeline: &str) -> String {
    format!(
        "```python\nfrom pymongo import MongoClient\n\nCOLLECTION = \"{collection}\"\nAGGREGATION_PIPELINE = {pipeline}\n```"
    )
}
