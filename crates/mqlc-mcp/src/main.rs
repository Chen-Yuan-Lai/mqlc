//! MCP server over stdio: `profile_schema` and `validate_pipeline` on one
//! MongoDB connection. No natural-language query tool: the host writes the
//! MQL (ADR 0008).

use std::collections::HashMap;
use std::sync::Arc;

use clap::Parser;
use mqlc_core::ir::{Pipeline, Schema};
use mqlc_core::mdl::MdlProject;
use mqlc_core::mongodb::Client;
use mqlc_core::profile::{self, ProfileOptions};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{Json, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Connection string of the MongoDB to profile; a read-only user is enough.
    #[arg(env = "MQLC_MONGO_URI")]
    mongo_uri: String,
}

#[derive(Deserialize, JsonSchema)]
struct ProfileSchemaRequest {
    /// The database name.
    database: String,
}

#[derive(Deserialize, JsonSchema)]
struct ValidatePipelineRequest {
    /// The database name.
    database: String,
    /// The collection the pipeline runs on.
    collection: String,
    /// The aggregation pipeline, as an Extended JSON array of stages.
    pipeline: serde_json::Value,
}

#[derive(Serialize, JsonSchema)]
struct Validation {
    /// Everything the schema rules out, in pipeline order.
    errors: Vec<CheckError>,
    /// What the check cannot read: nothing after it is checked.
    unchecked: Vec<String>,
}

#[derive(Serialize, JsonSchema)]
struct CheckError {
    /// The kind's name, as `--repair static-check` records it
    /// (`unknown_field`, ...).
    kind: String,
    message: String,
}

#[derive(Clone)]
struct Server {
    mongo: Client,
    /// The schema of each database profiled so far.
    schemas: Arc<Mutex<HashMap<String, Arc<Schema>>>>,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl Server {
    fn new(mongo: Client) -> Self {
        Self {
            mongo,
            schemas: Arc::default(),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = PROFILE_SCHEMA)]
    async fn profile_schema(
        &self,
        Parameters(request): Parameters<ProfileSchemaRequest>,
    ) -> Result<String, String> {
        Ok(self.profile(&request.database).await?.render())
    }

    #[tool(description = VALIDATE_PIPELINE)]
    async fn validate_pipeline(
        &self,
        Parameters(request): Parameters<ValidatePipelineRequest>,
    ) -> Result<Json<Validation>, String> {
        let pipeline =
            Pipeline::from_extended_json(&request.pipeline).map_err(|e| e.to_string())?;
        let schema = self.schema(&request.database).await?;
        let errors = pipeline
            .check(&request.collection, &schema)
            .into_iter()
            .map(|e| CheckError {
                kind: e.kind.name().to_string(),
                message: e.message,
            })
            .collect();
        Ok(Json(Validation {
            errors,
            unchecked: pipeline.unchecked(),
        }))
    }
}

impl Server {
    /// The database's schema, profiling it the first time.
    async fn schema(&self, database: &str) -> Result<Arc<Schema>, String> {
        if let Some(schema) = self.schemas.lock().await.get(database) {
            return Ok(schema.clone());
        }
        self.profile(database).await?;
        Ok(self.schemas.lock().await[database].clone())
    }

    /// Profiles the database afresh, replacing the schema `validate_pipeline`
    /// checks against.
    async fn profile(&self, database: &str) -> Result<MdlProject, String> {
        let project = profile::profile(&self.mongo, database, &ProfileOptions::default())
            .await
            .map_err(|e| e.to_string())?;
        // MongoDB lists no collections for a database that does not exist.
        if project.mdl.models.is_empty() {
            return Err(format!("database `{database}` has no collections"));
        }
        self.schemas.lock().await.insert(
            database.to_string(),
            Arc::new(Schema::from_mdl(&project.mdl)),
        );
        Ok(project)
    }
}

// Text the host model reads: quoted in ADR 0008.
const PROFILE_SCHEMA: &str = "Describe a MongoDB database for writing aggregation pipelines: \
    each collection with its field paths (nested and array fields included), the BSON types \
    seen, how often a field is missing, the `_id`s it refers to, and its values or their \
    format, range and most common ones. Read from a sample of up to 100,000 documents per \
    collection. Call it again after the data changes: validate_pipeline checks against the \
    latest profile.";
const VALIDATE_PIPELINE: &str = "Check an aggregation pipeline against the database's \
    profile before running it. Reports collections and field paths that do not exist at that \
    stage, values compared with fields that never hold their type, and $unwind on fields that \
    are never arrays. It reports only what the profile rules out: no errors does not make the \
    pipeline right. `unchecked` lists the operators it cannot read; it checks nothing after \
    them. A field missing from every sampled document is reported as missing.";

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Server {}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let mongo = Client::with_uri_str(&cli.mongo_uri).await?;
    let service = Server::new(mongo).serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
