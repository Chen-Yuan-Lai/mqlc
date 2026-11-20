//! Knowledge as a Wren MDL project: `mdl.json` (models and relationships, a
//! valid Wren MDL) and `instructions.json` beside it. MongoDB-only facts live
//! in properties under the `mqlc.` namespace (ADR 0004).

use std::collections::{BTreeMap, HashSet};
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
/// is left out. Every type here refuses keys it does not know: a key mqlc
/// does not read (a typo in a human MDL, or a Wren feature such as views)
/// would otherwise be dropped from the prompt without a word.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mdl {
    pub catalog: String,
    /// The database name.
    pub schema: String,
    pub models: Vec<Model>,
    #[serde(default)]
    pub relationships: Vec<Relationship>,
}

/// A collection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Model {
    pub name: String,
    pub table_reference: TableReference,
    pub columns: Vec<Column>,
    #[serde(default)]
    pub properties: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableReference {
    pub table: String,
}

/// A field path, or (calculated) a formula over the model's fields. Wren
/// allows only string values in column properties; a person writes
/// `description` there.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Column {
    pub name: String,
    /// The BSON types seen, most frequent first, joined by ` | `.
    pub r#type: String,
    #[serde(default)]
    pub not_null: bool,
    /// Not stored in the documents: computed by `expression`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_calculated: bool,
    /// A calculated column's formula, written by a person (for MongoDB, an
    /// aggregation expression).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expression: Option<String>,
    #[serde(default)]
    pub properties: BTreeMap<String, String>,
}

/// A reference from a field of one model to another model's `_id`. Its
/// `mqlc.path` property is the referring field's path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Relationship {
    pub name: String,
    /// The referring model, then the referred one.
    pub models: Vec<String>,
    pub join_type: JoinType,
    pub condition: String,
    #[serde(default)]
    pub properties: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum JoinType {
    OneToOne,
    OneToMany,
    ManyToOne,
    ManyToMany,
}

/// A note for the model, in WrenAI's instruction shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
        let references = self.references();
        let mut parts: Vec<String> = self
            .sorted_models()
            .into_iter()
            .map(|m| {
                let lines: Vec<String> = m
                    .columns
                    .iter()
                    .map(|c| render_line(&column_line(c, references.get(&(&*m.name, path(c))))))
                    .collect();
                format!(
                    "{}\n{}",
                    collection_header(&m.name, Some(m)),
                    lines.join("\n")
                )
            })
            .collect();
        parts.extend(self.render_instructions(&[]));
        parts.join("\n\n")
    }

    /// The official schema description (`official`: field lines per
    /// collection) with this knowledge merged in, in the same layout: each
    /// official field line keeps its text and gains what the MDL says of the
    /// same path (with or without the collection's name in front), the MDL's
    /// `data_type` as `observed_type` when it adds to the official one; then
    /// each field only the MDL has, as `render` writes it. Collections of
    /// either side, in name order.
    pub fn render_with_official(&self, official: &[(String, Vec<String>)]) -> String {
        use serde_json::Value;
        let references = self.references();
        let models: BTreeMap<&str, &Model> = self
            .mdl
            .models
            .iter()
            .map(|m| (m.name.as_str(), m))
            .collect();
        let official: BTreeMap<&str, &[String]> = official
            .iter()
            .map(|(name, lines)| (name.as_str(), lines.as_slice()))
            .collect();
        let mut names: Vec<&str> = models.keys().chain(official.keys()).copied().collect();
        names.sort();
        names.dedup();
        let mut parts: Vec<String> = names
            .into_iter()
            .map(|name| {
                let columns = models.get(name).map_or(&[][..], |m| m.columns.as_slice());
                let line_of = |c: &Column| column_line(c, references.get(&(name, path(c))));
                let mut merged = HashSet::new();
                let mut lines: Vec<String> = official
                    .get(name)
                    .copied()
                    .unwrap_or_default()
                    .iter()
                    .map(|text| {
                        let field: Option<Value> = serde_json::from_str(text).ok();
                        let field_name = field.as_ref().and_then(|f| f["field_name"].as_str());
                        let official_type = field.as_ref().and_then(|f| f["data_type"].as_str());
                        // Some official files name each field after its
                        // collection too (`schools.cdsCode`).
                        let unprefixed =
                            field_name.and_then(|f| f.strip_prefix(name)?.strip_prefix('.'));
                        let column_at =
                            |f: Option<&str>| f.and_then(|f| columns.iter().find(|c| path(c) == f));
                        let Some(column) = column_at(field_name).or_else(|| column_at(unprefixed))
                        else {
                            return text.clone();
                        };
                        merged.insert(path(column));
                        let extra: Vec<(&str, Value)> = line_of(column)
                            .into_iter()
                            .filter_map(|(key, value)| match key {
                                "field_name" => None,
                                "data_type" => value
                                    .as_str()
                                    .is_some_and(|seen| {
                                        adds_to_official_type(seen, official_type.unwrap_or(""))
                                    })
                                    .then_some(("observed_type", value)),
                                _ => Some((key, value)),
                            })
                            .collect();
                        match text.strip_suffix('}') {
                            Some(open) if !extra.is_empty() => {
                                format!("{open}, {}}}", render_pairs(&extra))
                            }
                            _ => text.clone(),
                        }
                    })
                    .collect();
                lines.extend(
                    columns
                        .iter()
                        .filter(|c| !merged.contains(path(c)))
                        .map(|c| render_line(&line_of(c))),
                );
                let header = collection_header(name, models.get(name).copied());
                format!("{header}\n{}", lines.join("\n"))
            })
            .collect();
        parts.extend(self.render_instructions(&[OBSERVED_TYPE_NOTE]));
        parts.join("\n\n")
    }

    fn sorted_models(&self) -> Vec<&Model> {
        let mut models: Vec<&Model> = self.mdl.models.iter().collect();
        models.sort_by(|a, b| a.name.cmp(&b.name));
        models
    }

    /// (model, path) -> the `_id` it refers to.
    fn references(&self) -> BTreeMap<(&str, &str), Reference> {
        self.mdl
            .relationships
            .iter()
            .filter_map(|r| {
                let path = r.properties.get("mqlc.path")?.as_str()?;
                let [from, to] = r.models.as_slice() else {
                    return None;
                };
                // `<matched>/<checked>`, shown only when some did not match.
                let matched = r
                    .properties
                    .get("mqlc.matched")
                    .and_then(|m| m.as_str())
                    .filter(|m| m.split_once('/').is_some_and(|(a, b)| a != b))
                    .map(str::to_string);
                let reference = Reference {
                    to: format!("{to}._id"),
                    matched,
                };
                Some(((from.as_str(), path), reference))
            })
            .collect()
    }

    /// The instructions, then `notes`, as one block; none if there are none.
    fn render_instructions(&self, notes: &[&str]) -> Option<String> {
        let lines: Vec<String> = self
            .instructions
            .iter()
            .map(|i| i.instruction.as_str())
            .chain(notes.iter().copied())
            .map(|i| format!("- {i}"))
            .collect();
        (!lines.is_empty()).then(|| format!("Instructions:\n{}", lines.join("\n")))
    }
}

/// The `_id` a field refers to, and how many of its distinct values checked
/// are `_id`s there when not all are.
struct Reference {
    to: String,
    matched: Option<String>,
}

/// What `render_with_official` adds to the instructions: the profiler wrote
/// them for `data_type` as the types seen, which an official line's is not.
const OBSERVED_TYPE_NOTE: &str = "In a field with a field_description, data_type is the official description's type, not the types seen; the types seen, in the form described above for data_type, are its observed_type where they add to it: a type it does not name (Int32, Int64, Double and Float are all Number, Object is Document), null where it does not allow null, or an array's element types. A field without a field_description was seen only in the sampled documents.";

/// Whether the types seen (`String | Null`, `Array<Document>`) say more than
/// the official `data_type` (`Null, String`, `Array`): a type it does not
/// name, null where it does not allow it, or an array's element types. Read
/// in the official vocabulary, where every number is a `Number` and the order
/// does not count.
fn adds_to_official_type(seen: &str, official: &str) -> bool {
    let word = |t: &str| match t.trim() {
        "Int32" | "Int64" | "Double" | "Float" => "Number".to_string(),
        "Object" => "Document".to_string(),
        t => t.to_string(),
    };
    let official: HashSet<String> = official.split([',', '|']).map(word).collect();
    top_level_types(seen).any(|t| !official.contains(&word(t)))
}

/// `A | Array<B | C>` as `A`, `Array<B | C>`.
fn top_level_types(types: &str) -> impl Iterator<Item = &str> {
    let mut depth = 0;
    types
        .split(move |c| {
            match c {
                '<' => depth += 1,
                '>' => depth -= 1,
                _ => {}
            }
            c == '|' && depth == 0
        })
        .map(str::trim)
}

/// `Collection: <name>`, then the model's `description` (a person's) if it
/// has one.
fn collection_header(name: &str, model: Option<&Model>) -> String {
    match model.and_then(|m| m.properties.get("description")?.as_str()) {
        Some(description) => format!("Collection: {name}\nDescription: {description}"),
        None => format!("Collection: {name}"),
    }
}

fn path(c: &Column) -> &str {
    c.properties.get("mqlc.path").unwrap_or(&c.name)
}

/// `{"field_name", "data_type", ...}` as key-value pairs, with arrays shown
/// as `Array<element types>`, a person's `description` and formula
/// (`computed_as`) if any, presence only when the field is sometimes missing,
/// the `_id` it refers to (if any, instead of its values, with how many of
/// them matched when not all did), then its values: all of them, or their
/// format, range and most common ones, and how many strings have leading or
/// trailing whitespace.
fn column_line(
    c: &Column,
    reference: Option<&Reference>,
) -> Vec<(&'static str, serde_json::Value)> {
    use serde_json::Value;
    let prop = |k: &str| c.properties.get(k).map(String::as_str);
    let parsed = |k: &str| prop(k).and_then(|v| serde_json::from_str::<Value>(v).ok());
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
    let mut line: Vec<(&str, Value)> = vec![
        ("field_name", path(c).into()),
        ("data_type", data_type.into()),
    ];
    if let Some(description) = prop("description") {
        line.push(("description", description.into()));
    }
    if let (true, Some(expression)) = (c.is_calculated, &c.expression) {
        line.push(("computed_as", expression.as_str().into()));
    }
    if let Some(array) = prop("mqlc.inArray") {
        line.push(("in_array", array.into()));
    }
    if let Some(present) = prop("mqlc.presence").and_then(render_presence) {
        line.push(("present", present.into()));
    }
    // The ids a reference holds say nothing its target does not.
    if let Some(reference) = reference {
        line.push(("references", reference.to.as_str().into()));
        if let Some(matched) = &reference.matched {
            line.push(("references_matched", matched.as_str().into()));
        }
        return line;
    }
    if let Some(values) = parsed("mqlc.enum") {
        line.push(("values", values));
    }
    if let Some(format) = prop("mqlc.format") {
        line.push(("format", format.into()));
    }
    for (key, prop) in [("min", "mqlc.min"), ("max", "mqlc.max")] {
        if let Some(v) = parsed(prop) {
            line.push((key, v));
        }
    }
    // `[[value, count], ...]`: the values alone.
    if let Some(Value::Array(top)) = parsed("mqlc.topValues") {
        let values = top
            .into_iter()
            .filter_map(|pair| pair.as_array()?.first().cloned())
            .collect();
        line.push(("common_values", Value::Array(values)));
    }
    if let Some(untrimmed) = parsed("mqlc.untrimmed") {
        line.push(("untrimmed", untrimmed));
    }
    line
}

/// Spaced like the official description files.
fn render_line(line: &[(&str, serde_json::Value)]) -> String {
    format!("{{{}}}", render_pairs(line))
}

/// `"key": value, ...` without the braces.
fn render_pairs(line: &[(&str, serde_json::Value)]) -> String {
    use serde_json::Value;
    let json = |v: &Value| serde_json::to_string(v).expect("JSON serializes");
    let pairs: Vec<String> = line
        .iter()
        .map(|(k, v)| format!("{}: {}", json(&(*k).into()), json(v)))
        .collect();
    pairs.join(", ")
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
