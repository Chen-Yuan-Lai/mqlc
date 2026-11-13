//! Knowledge as a Wren MDL project: `mdl.json` (models and relationships, a
//! valid Wren MDL) and `instructions.json` beside it. MongoDB-only facts live
//! in properties under the `mqlc.` namespace (ADR 0004).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const MDL_FILE: &str = "mdl.json";
const INSTRUCTIONS_FILE: &str = "instructions.json";

#[derive(Debug, thiserror::Error)]
pub enum MdlError {
    #[error("cannot read or write {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} is not a valid MDL project file: {source}")]
    Invalid {
        path: PathBuf,
        source: serde_json::Error,
    },
}

/// One database's knowledge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MdlProject {
    pub mdl: Mdl,
    pub instructions: Vec<Instruction>,
}

/// The Wren MDL manifest. Wren has no MongoDB data source, so `dataSource`
/// is left out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mdl {
    pub catalog: String,
    /// The database name.
    pub schema: String,
    pub models: Vec<Model>,
    #[serde(default)]
    pub relationships: Vec<serde_json::Value>,
}

/// A collection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub name: String,
    pub table_reference: TableReference,
    pub columns: Vec<Column>,
    #[serde(default)]
    pub properties: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TableReference {
    pub table: String,
}

/// A field path. Wren allows only string values in column properties.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Column {
    pub name: String,
    /// The BSON types seen, most frequent first, joined by ` | `.
    pub r#type: String,
    #[serde(default)]
    pub not_null: bool,
    #[serde(default)]
    pub properties: BTreeMap<String, String>,
}

/// A note for the model, in WrenAI's instruction shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Instruction {
    pub instruction: String,
}

impl MdlProject {
    /// Writes `mdl.json` and `instructions.json` into `dir`, creating it.
    pub fn save(&self, dir: &Path) -> Result<(), MdlError> {
        let io = |path: &Path| {
            let path = path.to_path_buf();
            move |source| MdlError::Io { path, source }
        };
        fs::create_dir_all(dir).map_err(io(dir))?;
        for (file, text) in [
            (MDL_FILE, serde_json::to_string_pretty(&self.mdl)),
            (
                INSTRUCTIONS_FILE,
                serde_json::to_string_pretty(&self.instructions),
            ),
        ] {
            let path = dir.join(file);
            let text = text.expect("MDL serializes") + "\n";
            fs::write(&path, text).map_err(io(&path))?;
        }
        Ok(())
    }

    pub fn load(dir: &Path) -> Result<Self, MdlError> {
        Ok(Self {
            mdl: read_json(&dir.join(MDL_FILE))?,
            instructions: read_json(&dir.join(INSTRUCTIONS_FILE))?,
        })
    }

    /// The knowledge as prompt text, in the layout of the official schema
    /// description: models in name order, each under `Collection: <name>`
    /// with one JSON line per column, then the instructions.
    pub fn render(&self) -> String {
        let mut models: Vec<&Model> = self.mdl.models.iter().collect();
        models.sort_by(|a, b| a.name.cmp(&b.name));
        let mut parts: Vec<String> = models
            .into_iter()
            .map(|m| {
                let lines: Vec<String> = m.columns.iter().map(render_column).collect();
                format!("Collection: {}\n{}", m.name, lines.join("\n"))
            })
            .collect();
        if !self.instructions.is_empty() {
            let notes: Vec<String> = self
                .instructions
                .iter()
                .map(|i| format!("- {}", i.instruction))
                .collect();
            parts.push(format!("Instructions:\n{}", notes.join("\n")));
        }
        parts.join("\n\n")
    }
}

/// `{"field_name", "data_type", ...}` with arrays shown as `Array<element
/// types>`, and presence only when the field is sometimes missing.
fn render_column(c: &Column) -> String {
    let prop = |k: &str| c.properties.get(k).map(String::as_str);
    let path = prop("mqlc.path").unwrap_or(&c.name);
    let data_type = match prop("mqlc.elementTypes") {
        Some(elements) => c
            .r#type
            .split(" | ")
            .map(|t| match t {
                "Array" => format!("Array<{elements}>"),
                t => t.to_string(),
            })
            .collect::<Vec<_>>()
            .join(" | "),
        None => c.r#type.clone(),
    };
    let mut line = vec![("field_name", path.to_string()), ("data_type", data_type)];
    if let Some(array) = prop("mqlc.inArray") {
        line.push(("in_array", array.to_string()));
    }
    if let Some(present) = prop("mqlc.presence").and_then(render_presence) {
        line.push(("present", present));
    }
    // Spaced like the official description files.
    let json = |s: &str| serde_json::to_string(s).expect("strings serialize");
    let pairs: Vec<String> = line
        .iter()
        .map(|(k, v)| format!("{}: {}", json(k), json(v)))
        .collect();
    format!("{{{}}}", pairs.join(", "))
}

/// `"<present>/<enclosing>"` as a rounded-down percentage, or `None` when
/// the field was always present. A field seen at all is never shown as 0%.
fn render_presence(presence: &str) -> Option<String> {
    let (present, enclosing) = presence.split_once('/')?;
    let (present, enclosing): (u64, u64) = (present.parse().ok()?, enclosing.parse().ok()?);
    if present >= enclosing {
        return None;
    }
    Some(match present * 100 / enclosing {
        0 => "<1%".to_string(),
        pct => format!("{pct}%"),
    })
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, MdlError> {
    let text = fs::read_to_string(path).map_err(|source| MdlError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_str(&text).map_err(|source| MdlError::Invalid {
        path: path.to_path_buf(),
        source,
    })
}
