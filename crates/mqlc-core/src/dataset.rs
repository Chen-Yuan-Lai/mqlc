//! Read-only access to an AptMQL-Bench checkout: questions, the official
//! text-to-MQL prompt and the per-collection schema descriptions.

use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::Deserialize;

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
        })
    }

    /// The user turn: schema block, then the question, with the evidence block
    /// only when `evidence` is given.
    pub fn user_message(&self, schema: &str, question: &str, evidence: Option<&str>) -> String {
        let evidence = evidence
            .map(|e| self.evidence.replace("{mongodb_evidence}", e))
            .unwrap_or_default();
        let schema = self.schema.replace("{schema_description}", schema);
        let question = self
            .question
            .replace("{nl_request}", question)
            .replace("{evidence_section}", &evidence);
        format!("{schema}\n\n{question}")
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

    pub fn prompt(&self) -> &OfficialPrompt {
        &self.prompt
    }

    /// The official schema description of one database: every
    /// `collections_description/<collection>.jsonl` in name order, each under a
    /// `Collection: <name>` header. The paper does not publish how it joined the
    /// files, so this layout is our own.
    pub fn schema_description(&self, split: &str, db_id: &str) -> Result<String, DatasetError> {
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
        let parts = files
            .iter()
            .map(|p| {
                let name = p.file_stem().unwrap().to_string_lossy();
                Ok(format!("Collection: {name}\n{}", read(p)?.trim()))
            })
            .collect::<Result<Vec<_>, DatasetError>>()?;
        Ok(parts.join("\n\n"))
    }
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
