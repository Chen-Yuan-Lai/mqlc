//! Parsing the model's reply: Python code that assigns `COLLECTION` and
//! `AGGREGATION_PIPELINE`, as the official prompt asks for.

use mongodb::bson::{Bson, Document};
use regex::Regex;
use std::sync::LazyLock;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ParseError {
    #[error("reply has no COLLECTION assignment")]
    NoCollection,
    #[error("reply has no AGGREGATION_PIPELINE list")]
    NoPipeline,
    #[error("AGGREGATION_PIPELINE is not valid JSON: {0}")]
    BadJson(String),
    #[error("AGGREGATION_PIPELINE is not valid Extended JSON: {0}")]
    BadExtendedJson(String),
    #[error("pipeline stage {0} is not a document")]
    StageNotDocument(usize),
}

#[derive(Debug)]
pub struct ParsedReply {
    pub collection: String,
    pub pipeline: Vec<Document>,
}

// Anchored at line start so commented-out or prefixed names don't match.
static COLLECTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?m)^\s*COLLECTION\s*=\s*["']([^"']+)["']"#).unwrap());
static PIPELINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*AGGREGATION_PIPELINE\s*=\s*\[").unwrap());

pub fn parse_reply(text: &str) -> Result<ParsedReply, ParseError> {
    let collection = COLLECTION.captures(text).ok_or(ParseError::NoCollection)?[1].to_string();

    let start = PIPELINE.find(text).ok_or(ParseError::NoPipeline)?.end() - 1;
    // Rewrite first, so the bracket scan only ever sees JSON.
    let rewritten = python_to_json(&text[start..]);
    let list = balanced_list(&rewritten).ok_or(ParseError::NoPipeline)?;
    let json: serde_json::Value =
        serde_json::from_str(list).map_err(|e| ParseError::BadJson(e.to_string()))?;
    let Bson::Array(stages) =
        Bson::try_from(json).map_err(|e| ParseError::BadExtendedJson(e.to_string()))?
    else {
        return Err(ParseError::NoPipeline);
    };
    let pipeline = stages
        .into_iter()
        .enumerate()
        .map(|(i, s)| match s {
            Bson::Document(d) => Ok(d),
            _ => Err(ParseError::StageNotDocument(i)),
        })
        .collect::<Result<_, _>>()?;
    Ok(ParsedReply {
        collection,
        pipeline,
    })
}

/// Rewrites the Python literal syntax models use despite the prompt (`True`,
/// `False`, `None`, single quotes, raw strings, Python-only escapes such as
/// `\d`, `#` comments, trailing commas) into JSON.
fn python_to_json(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' | '\'' => push_string(&mut out, &mut chars, c, false),
            '#' => while chars.next_if(|&n| n != '\n').is_some() {},
            ']' | '}' => {
                let kept = out.trim_end().len();
                out.truncate(kept);
                if out.ends_with(',') {
                    out.pop();
                }
                out.push(c);
            }
            'r' | 'R' if matches!(chars.peek(), Some('"' | '\'')) => {
                let quote = chars.next().unwrap();
                push_string(&mut out, &mut chars, quote, true);
            }
            _ if c.is_ascii_alphabetic() => {
                let mut word = String::from(c);
                while let Some(n) = chars.next_if(|n| n.is_ascii_alphanumeric() || *n == '_') {
                    word.push(n);
                }
                out.push_str(match word.as_str() {
                    "True" => "true",
                    "False" => "false",
                    "None" => "null",
                    _ => &word,
                });
            }
            _ => out.push(c),
        }
    }
    out
}

/// Copies one Python string literal (opening `quote` already consumed) as a
/// JSON string with the same value.
fn push_string(
    out: &mut String,
    chars: &mut std::iter::Peekable<std::str::Chars>,
    quote: char,
    raw: bool,
) {
    out.push('"');
    while let Some(c) = chars.next() {
        match c {
            _ if c == quote => break,
            '"' => out.push_str("\\\""),
            '\\' if raw => out.push_str("\\\\"),
            '\\' => match chars.next() {
                // Escapes that mean the same in Python and JSON.
                Some(e @ ('\\' | '"' | 'n' | 'r' | 't' | 'b' | 'f' | 'u')) => {
                    out.push('\\');
                    out.push(e);
                }
                Some('\'') => out.push('\''),
                // Python keeps the backslash of unknown escapes (`\d`, `\.`).
                Some(e) => {
                    out.push_str("\\\\");
                    out.push(e);
                }
                None => break,
            },
            _ => out.push(c),
        }
    }
    out.push('"');
}

/// The text of the `[...]` that the JSON `text` starts with, skipping brackets
/// inside strings.
fn balanced_list(text: &str) -> Option<&str> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in text.char_indices() {
        if in_string {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '[' | '{' => depth += 1,
            ']' | '}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(&text[..=i]);
                }
            }
            _ => {}
        }
    }
    None
}
