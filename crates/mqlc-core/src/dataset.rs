//! Read-only access to an AptMQL-Bench checkout: questions, the official
//! text-to-MQL prompt and the per-collection schema descriptions.

use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const DATA_FILE: &str = "aptmqlbench_data.jsonl";
const PROMPT_FILE: &str = "prompts/s51_text2mql_1time_completion.txt";

#[derive(Debug, thiserror::Error)]
pub enum DatasetError {
    #[error("cannot read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} line {line}: {source}")]
    BadSample {
        path: PathBuf,
        line: usize,
        source: serde_json::Error,
    },
    #[error("{0} is not in the official prompt layout")]
    BadPrompt(PathBuf),
    #[error("no schema description for {split}/{db_id}")]
    NoSchema { split: String, db_id: String },
    #[error("{path} line {line}: {source}")]
    BadDescription {
        path: PathBuf,
        line: usize,
        source: serde_json::Error,
    },
}

/// A collection's name, and the `field_name` and `data_type` of each field
/// its official description lists.
pub type CollectionFields = (String, Vec<(String, String)>);

/// One line of a `collections_description/<collection>.jsonl` file.
#[derive(Deserialize)]
struct FieldDescription {
    field_name: String,
    field_description: String,
    data_type: String,
    value_description: String,
}

/// One benchmark question with its gold answer.
#[derive(Debug, Clone, Deserialize)]
pub struct Sample {
    pub split: String,
    pub db_id: String,
    pub question: String,
    pub evidence: String,
    pub mongodb_evidence: String,
    /// A string in train and an integer in dev; normalised to a string.
    #[serde(deserialize_with = "string_or_int")]
    pub question_id: String,
    pub sample_id: String,
    #[serde(rename = "SQL")]
    pub sql: String,
    #[serde(rename = "MQL")]
    pub mql: GoldMql,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GoldMql {
    pub collection: String,
    /// Extended JSON, kept as-is.
    pub aggregation_pipeline: serde_json::Value,
}

/// The official prompt file: a system prompt followed by Python string
/// templates for the schema block, the question block and the evidence block.
#[derive(Debug, Clone)]
pub struct OfficialPrompt {
    pub system: String,
    schema: String,
    question: String,
    evidence: String,
    hash: String,
}

impl OfficialPrompt {
    fn parse(text: &str) -> Option<Self> {
        let (system, rest) = text.split_once("\n\"\"\"")?;
        let template = |name: &str| {
            let re = Regex::new(&format!(r#"(?s){name} = """(.*?)""""#)).unwrap();
            re.captures(rest).map(|c| c[1].to_string())
        };
        Some(Self {
            system: system.trim_end().to_string(),
            schema: template("SCHEMA_PROMPT")?,
            question: template("QUESTION_PROMPT")?,
            evidence: template("EVIDENCE_SECTION")?,
            hash: sha256(text.as_bytes()),
        })
    }

    /// The schema block: the part of the user turn shared by every question on
    /// one database, and so the part worth caching.
    pub fn schema_block(&self, schema: &str) -> String {
        self.schema.replace("{schema_description}", schema)
    }

    /// The question block, with the evidence block only when `evidence` is given.
    pub fn question_block(&self, question: &str, evidence: Option<&str>) -> String {
        let evidence = evidence
            .map(|e| self.evidence.replace("{mongodb_evidence}", e))
            .unwrap_or_default();
        self.question
            .replace("{nl_request}", question)
            .replace("{evidence_section}", &evidence)
    }

    /// Hash of the whole prompt file, recorded in run manifests.
    pub fn hash(&self) -> &str {
        &self.hash
    }
}

#[derive(Debug, Clone)]
pub struct Dataset {
    root: PathBuf,
    samples: Vec<Sample>,
    prompt: OfficialPrompt,
}

impl Dataset {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, DatasetError> {
        let root = root.as_ref().to_path_buf();
        let data_path = root.join(DATA_FILE);
        let samples = read(&data_path)?
            .lines()
            .enumerate()
            .filter(|(_, l)| !l.trim().is_empty())
            .map(|(i, l)| {
                serde_json::from_str(l).map_err(|source| DatasetError::BadSample {
                    path: data_path.clone(),
                    line: i + 1,
                    source,
                })
            })
            .collect::<Result<_, _>>()?;
        let prompt_path = root.join(PROMPT_FILE);
        let prompt = OfficialPrompt::parse(&read(&prompt_path)?)
            .ok_or(DatasetError::BadPrompt(prompt_path))?;
        Ok(Self {
            root,
            samples,
            prompt,
        })
    }

    pub fn samples(&self) -> &[Sample] {
        &self.samples
    }

    pub fn sample(&self, sample_id: &str) -> Option<&Sample> {
        self.samples.iter().find(|s| s.sample_id == sample_id)
    }

    /// The split whose questions are on `db_id` (the splits share no
    /// database).
    pub fn split_of(&self, db_id: &str) -> Option<&str> {
        self.samples
            .iter()
            .find(|s| s.db_id == db_id)
            .map(|s| s.split.as_str())
    }

    pub fn prompt(&self) -> &OfficialPrompt {
        &self.prompt
    }

    /// The official schema description of one database: every
    /// `collections_description/<collection>.jsonl` in name order, each under a
    /// `Collection: <name>` header. The paper does not publish how it joined the
    /// files, so this layout is our own.
    pub fn schema_description(&self, split: &str, db_id: &str) -> Result<String, DatasetError> {
        self.joined_descriptions(split, db_id, |name, path| {
            Ok(format!("Collection: {name}\n{}", read(path)?.trim()))
        })
    }

    /// The official schema description with each file wrapped in a
    /// `<collection name="...">` tag instead of under a `Collection:` header,
    /// another guess at the paper's layout (#19).
    pub fn tagged_schema_description(
        &self,
        split: &str,
        db_id: &str,
    ) -> Result<String, DatasetError> {
        self.joined_descriptions(split, db_id, |name, path| {
            Ok(format!(
                "<collection name=\"{name}\">\n{}\n</collection>",
                read(path)?.trim()
            ))
        })
    }

    /// The official schema description with each field line rewritten to the
    /// keys the official prompt names (`field_name`, `field_description`,
    /// `data_format`, `value_description`): the files call `data_format`
    /// `data_type` and add `required`, which the prompt does not mention.
    pub fn prompt_keys_schema_description(
        &self,
        split: &str,
        db_id: &str,
    ) -> Result<String, DatasetError> {
        self.joined_descriptions(split, db_id, |name, path| {
            // Written by hand to keep the files' `", "` and `": "`
            // separators, which serde_json's compact output drops.
            let quoted = |v: &str| serde_json::to_string(v).expect("strings serialize");
            let fields: Vec<String> = field_descriptions(path)?
                .iter()
                .map(|field| {
                    format!(
                        "{{\"field_name\": {}, \"field_description\": {}, \"data_format\": {}, \"value_description\": {}}}",
                        quoted(&field.field_name),
                        quoted(&field.field_description),
                        quoted(&field.data_type),
                        quoted(&field.value_description),
                    )
                })
                .collect();
            Ok(format!("Collection: {name}\n{}", fields.join("\n")))
        })
    }

    /// Each collection of a database, in name order, with the `field_name`
    /// and `data_type` of every field its official description lists.
    pub fn official_field_types(
        &self,
        split: &str,
        db_id: &str,
    ) -> Result<Vec<CollectionFields>, DatasetError> {
        self.description_files(split, db_id)?
            .into_iter()
            .map(|(name, path)| {
                let fields = field_descriptions(&path)?
                    .into_iter()
                    .map(|f| (f.field_name, f.data_type))
                    .collect();
                Ok((name, fields))
            })
            .collect()
    }

    /// The official schema description of one database as its field lines
    /// (blank lines dropped), per collection in name order.
    pub fn official_fields(
        &self,
        split: &str,
        db_id: &str,
    ) -> Result<Vec<(String, Vec<String>)>, DatasetError> {
        self.description_files(split, db_id)?
            .into_iter()
            .map(|(name, path)| {
                let lines = read(&path)?
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(str::to_string)
                    .collect();
                Ok((name, lines))
            })
            .collect()
    }

    /// Every `collections_description/<collection>.jsonl` of a database in
    /// name order, each rendered by `render` from its collection name and
    /// path, one blank line apart.
    fn joined_descriptions(
        &self,
        split: &str,
        db_id: &str,
        render: impl Fn(&str, &Path) -> Result<String, DatasetError>,
    ) -> Result<String, DatasetError> {
        let parts = self
            .description_files(split, db_id)?
            .iter()
            .map(|(name, path)| render(name, path))
            .collect::<Result<Vec<_>, DatasetError>>()?;
        Ok(parts.join("\n\n"))
    }

    /// Each `collections_description/<collection>.jsonl` of a database as its
    /// collection name and path, in name order.
    fn description_files(
        &self,
        split: &str,
        db_id: &str,
    ) -> Result<Vec<(String, PathBuf)>, DatasetError> {
        let dir = self
            .root
            .join("databases")
            .join(split)
            .join(db_id)
            .join("collections_description");
        let no_schema = || DatasetError::NoSchema {
            split: split.to_string(),
            db_id: db_id.to_string(),
        };
        let mut files: Vec<PathBuf> = fs::read_dir(&dir)
            .map_err(|_| no_schema())?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .collect();
        if files.is_empty() {
            return Err(no_schema());
        }
        files.sort();
        Ok(files
            .into_iter()
            .map(|p| (p.file_stem().unwrap().to_string_lossy().into_owned(), p))
            .collect())
    }
}

fn field_descriptions(path: &Path) -> Result<Vec<FieldDescription>, DatasetError> {
    read(path)?
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            serde_json::from_str(line).map_err(|source| DatasetError::BadDescription {
                path: path.to_path_buf(),
                line: index + 1,
                source,
            })
        })
        .collect()
}

/// `sha256:<hex>`, the form hashes take in run manifests.
pub(crate) fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold("sha256:".to_string(), |mut h, b| {
            h.push_str(&format!("{b:02x}"));
            h
        })
}

fn string_or_int<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Id {
        Str(String),
        Int(i64),
    }
    Ok(match Id::deserialize(d)? {
        Id::Str(s) => s,
        Id::Int(i) => i.to_string(),
    })
}

fn read(path: &Path) -> Result<String, DatasetError> {
    fs::read_to_string(path).map_err(|source| DatasetError::Io {
        path: path.to_path_buf(),
        source,
    })
}
