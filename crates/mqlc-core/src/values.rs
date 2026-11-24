//! Value correction: string literals of a pipeline that no document has, and
//! the closest values the data does have.

use std::time::Duration;

use futures_util::TryStreamExt;
use mongodb::Database;
use mongodb::bson::{Bson, Document, doc};

use crate::engine::is_unavailable;
use crate::ir::{
    Clause, Condition, ElemMatch, Filter, Pipeline, Predicate, QueryOp, Stage, Unwind,
};

/// Distinct values read per pattern, at most.
const MAX_VALUES: i64 = 1000;
/// Each query's time limit, the pipeline's own: a regex over a large
/// collection takes more than 10 seconds.
const QUERY_TIMEOUT: Duration = Duration::from_secs(60);
/// Closest values fed back per missing literal.
const CLOSEST: usize = 5;

/// A string a field must equal for a document to pass a `$match`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Literal {
    pub collection: String,
    pub path: String,
    pub value: String,
}

/// A literal no document has, with the values closest to it, best first.
#[derive(Debug)]
pub(crate) struct Missing {
    pub literal: Literal,
    pub closest: Vec<String>,
}

/// The literals of the `$match` stages that still see the start
/// collection's documents (or, past a `$lookup`, the joined collection's),
/// each one a field must equal: equality, `$eq`, `$in` and `$all`, outside
/// `$or` and `$expr`, where a missing value can be what the question means.
pub(crate) fn literals(collection: &str, pipeline: &Pipeline) -> Vec<Literal> {
    // Fields added by the stages so far: from a joined collection, or
    // unknown.
    let mut added: Vec<(String, Option<String>)> = Vec::new();
    let mut found = Vec::new();
    for stage in &pipeline.stages {
        match stage {
            Stage::Match(filter) => {
                let mut fields = Vec::new();
                filter_literals(filter, "", &mut fields);
                found.extend(
                    fields
                        .into_iter()
                        .filter_map(|(path, value)| resolve(collection, &added, path, value)),
                );
            }
            Stage::Sort(_) | Stage::Limit(_) | Stage::Skip(_) => {}
            Stage::Unwind(Unwind::Path(_)) => {}
            Stage::Unwind(Unwind::Options {
                include_array_index,
                ..
            }) => added.extend(include_array_index.iter().map(|f| (f.clone(), None))),
            Stage::Lookup(lookup) => {
                let from = (lookup.local_field.is_some() && lookup.pipeline.is_none())
                    .then(|| lookup.from.clone());
                added.push((lookup.as_field.clone(), from));
            }
            // Anything else changes what the documents hold.
            _ => break,
        }
    }
    found.sort();
    found.dedup();
    found
}

/// The literal of `path`, in the collection it reads.
fn resolve(
    collection: &str,
    added: &[(String, Option<String>)],
    path: String,
    value: String,
) -> Option<Literal> {
    let latest = added
        .iter()
        .rev()
        .find(|(field, _)| path == *field || path.starts_with(&format!("{field}.")));
    let (collection, path) = match latest {
        None => (collection.to_string(), path),
        Some((field, Some(from))) if path != *field => {
            (from.clone(), path[field.len() + 1..].to_string())
        }
        Some(_) => return None,
    };
    Some(Literal {
        collection,
        path,
        value,
    })
}

/// The (path, string) pairs `filter` requires, its paths under `prefix`.
fn filter_literals(filter: &Filter, prefix: &str, found: &mut Vec<(String, String)>) {
    for clause in &filter.0 {
        match clause {
            Clause::Field(field, condition) => {
                let path = format!("{prefix}{field}");
                match condition {
                    Condition::Equals(value) => push_string(value, &path, found),
                    Condition::Predicates(predicates) => {
                        predicate_literals(predicates, &path, found)
                    }
                }
            }
            Clause::And(filters) => filters
                .iter()
                .for_each(|f| filter_literals(f, prefix, found)),
            Clause::Or(_) | Clause::Expr(_) => {}
        }
    }
}

/// The strings `predicates` require `path` to equal.
fn predicate_literals(predicates: &[Predicate], path: &str, found: &mut Vec<(String, String)>) {
    for predicate in predicates {
        match predicate {
            Predicate::Compare(QueryOp::Eq, value) => push_string(value, path, found),
            Predicate::Compare(QueryOp::In | QueryOp::All, Bson::Array(values)) => values
                .iter()
                .for_each(|value| push_string(value, path, found)),
            Predicate::Compare(..) => {}
            Predicate::ElemMatch(ElemMatch::Filter(filter)) => {
                filter_literals(filter, &format!("{path}."), found)
            }
            Predicate::ElemMatch(ElemMatch::Predicates(predicates)) => {
                predicate_literals(predicates, path, found)
            }
        }
    }
}

/// Adds `value` as the literal of `path` if it is a string other than "".
fn push_string(value: &Bson, path: &str, found: &mut Vec<(String, String)>) {
    if let Bson::String(s) = value
        && !s.is_empty()
    {
        found.push((path.to_string(), s.clone()));
    }
}

/// The literals no document has and that have values like them. One whose
/// queries the server rejects or times out is left out: the pipeline using
/// it already ran. Only an unavailable database is an error.
pub(crate) async fn missing(
    db: &Database,
    literals: Vec<Literal>,
) -> Result<Vec<Missing>, mongodb::error::Error> {
    let mut found = Vec::new();
    for literal in literals {
        match closest_values(db, &literal).await {
            Ok(closest) if closest.is_empty() => {}
            Ok(closest) => found.push(Missing { literal, closest }),
            Err(e) if is_unavailable(&e) => return Err(e),
            Err(_) => {}
        }
    }
    Ok(found)
}

/// None when a document has the literal; otherwise the closest values from
/// the narrowest pattern that finds any (`patterns`).
async fn closest_values(
    db: &Database,
    literal: &Literal,
) -> Result<Vec<String>, mongodb::error::Error> {
    let collection = db.collection::<Document>(&literal.collection);
    let path = literal.path.as_str();
    let count = collection
        .count_documents(doc! { path: literal.value.as_str() })
        .limit(1)
        .max_time(QUERY_TIMEOUT)
        .await?;
    if count > 0 {
        return Ok(Vec::new());
    }
    for pattern in patterns(&literal.value) {
        let rows: Vec<Document> = collection
            .aggregate([
                doc! { "$match": { path: { "$regex": pattern, "$options": "i" } } },
                // By value, so values many documents share do not crowd out
                // the rest, and sorted, so the same values come back every
                // time.
                doc! { "$group": { "_id": format!("${path}") } },
                doc! { "$sort": { "_id": 1 } },
                doc! { "$limit": MAX_VALUES },
            ])
            .max_time(QUERY_TIMEOUT)
            .await?
            .try_collect()
            .await?;
        let mut values = Vec::new();
        rows.iter()
            .filter_map(|row| row.get("_id"))
            .for_each(|v| collect_strings(v, &mut values));
        if !values.is_empty() {
            return Ok(closest(&literal.value, values));
        }
    }
    Ok(Vec::new())
}

/// Case-insensitive patterns for values like `value`, narrowest first:
/// containing it, containing each of its words in any order (`Smith, Alice`
/// for `Alice Smith`), containing any of its words of two or more
/// characters (`6PM` for `6:00 PM`). Narrower patterns go first so that a word many values share
/// does not fill `MAX_VALUES` with them.
fn patterns(value: &str) -> Vec<String> {
    let words = words(value);
    let mut patterns = vec![regex::escape(value)];
    if words.len() > 1 {
        patterns.push(
            words
                .iter()
                .map(|w| format!("(?=.*{})", regex::escape(w)))
                .collect(),
        );
    }
    let long: Vec<String> = words
        .iter()
        .filter(|w| w.chars().count() >= 2)
        .map(|w| regex::escape(w))
        .collect();
    if long.len() > 1 || (long.len() == 1 && words.len() > 1) {
        patterns.push(long.join("|"));
    }
    patterns
}

/// The words of `value`, lowercased.
fn words(value: &str) -> Vec<String> {
    value
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// The strings in `value`, inside arrays too.
fn collect_strings(value: &Bson, found: &mut Vec<String>) {
    match value {
        Bson::String(s) => found.push(s.clone()),
        Bson::Array(items) => items.iter().for_each(|v| collect_strings(v, found)),
        _ => {}
    }
}

/// The values most like `literal` (`similarity`), best first.
fn closest(literal: &str, mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values.dedup();
    let mut scored: Vec<(f64, String)> = values
        .into_iter()
        .map(|v| (similarity(literal, &v), v))
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    scored.into_iter().take(CLOSEST).map(|(_, v)| v).collect()
}

/// Jaro-Winkler of the lowercased texts, or of their words sorted, whichever
/// is higher.
fn similarity(a: &str, b: &str) -> f64 {
    let sorted = |s: &str| {
        let mut w = words(s);
        w.sort();
        w.join(" ")
    };
    strsim::jaro_winkler(&a.to_lowercase(), &b.to_lowercase())
        .max(strsim::jaro_winkler(&sorted(a), &sorted(b)))
}
