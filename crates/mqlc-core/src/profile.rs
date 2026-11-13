//! The schema profiler: samples each collection of a database and infers its
//! field paths, their BSON types and presence, arrays and embedded
//! documents, as an auto MDL project.

use std::collections::{BTreeMap, HashMap};

use futures_util::TryStreamExt;
use mongodb::Client;
use mongodb::bson::{Bson, Document, doc};

use crate::mdl::{Column, Instruction, Mdl, MdlProject, Model, TableReference};

const CATALOG: &str = "mqlc";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileOptions {
    /// Documents read per collection: the first ones in `_id` order, so the
    /// same data always gives the same profile.
    pub sample_size: usize,
}

impl Default for ProfileOptions {
    /// On AptMQL-Bench, 100,000 documents find every field a full scan finds
    /// (1,000 miss 42 sparse ones) and profile all 21 databases in about 35 s.
    fn default() -> Self {
        Self {
            sample_size: 100_000,
        }
    }
}

/// Profiles every collection of `db_id` (system collections aside).
pub async fn profile(
    client: &Client,
    db_id: &str,
    options: &ProfileOptions,
) -> mongodb::error::Result<MdlProject> {
    let db = client.database(db_id);
    let mut names: Vec<String> = db
        .list_collection_names()
        .await?
        .into_iter()
        .filter(|n| !n.starts_with("system."))
        .collect();
    names.sort();
    let mut models = Vec::new();
    for name in names {
        let collection = db.collection::<Document>(&name);
        let docs: Vec<Document> = collection
            .find(doc! {})
            .sort(doc! { "_id": 1 })
            .limit(options.sample_size as i64)
            .await?
            .try_collect()
            .await?;
        let documents = collection.estimated_document_count().await?;
        let mut fields = Fields::default();
        for d in &docs {
            fields.visit(d, "", None);
        }
        let mut columns = Vec::new();
        fields.columns(docs.len() as u64, true, &mut columns);
        models.push(Model {
            table_reference: TableReference {
                table: name.clone(),
            },
            name,
            columns,
            properties: BTreeMap::from([
                ("mqlc.sampled".to_string(), docs.len().into()),
                ("mqlc.documents".to_string(), documents.into()),
            ]),
        });
    }
    Ok(MdlProject {
        mdl: Mdl {
            catalog: CATALOG.to_string(),
            schema: db_id.to_string(),
            models,
            relationships: Vec::new(),
        },
        instructions: instructions(options.sample_size),
    })
}

fn instructions(sample_size: usize) -> Vec<Instruction> {
    [
        "field_name is the field's MongoDB dot-notation path. A field with in_array \
         lies inside the elements of that array: it matches element-wise, and \
         $unwind of the array turns each element into a row."
            .to_string(),
        "data_type lists the BSON types seen, most frequent first; Array<...> gives \
         the types of the array's elements. present is the share of enclosing \
         documents (or array elements) that have the field; a field without it was \
         always present."
            .to_string(),
        format!(
            "Types and presence come from up to the first {sample_size} documents of \
             each collection in _id order; other documents may differ."
        ),
    ]
    .into_iter()
    .map(|instruction| Instruction { instruction })
    .collect()
}

/// The fields seen in one kind of (sub)document, in the order first seen.
#[derive(Default)]
struct Fields {
    fields: Vec<Field>,
    index: HashMap<String, usize>,
}

struct Field {
    path: String,
    /// Enclosing documents that have the field.
    present: u64,
    types: Counts,
    element_types: Counts,
    /// The innermost array whose elements hold the field.
    in_array: Option<String>,
    /// Embedded documents found at this path (values or array elements):
    /// what `children` were looked for in.
    enclosed: u64,
    children: Fields,
}

impl Fields {
    fn visit(&mut self, doc: &Document, prefix: &str, in_array: Option<&str>) {
        for (key, value) in doc {
            let field = self.field(key, prefix, in_array);
            field.present += 1;
            field.types.add(type_name(value));
            match value {
                Bson::Document(d) => {
                    field.enclosed += 1;
                    field.children.visit(d, &field.path, in_array);
                }
                Bson::Array(elements) => {
                    for e in elements {
                        field.element_types.add(type_name(e));
                        // Arrays of arrays are not looked into.
                        if let Bson::Document(d) = e {
                            field.enclosed += 1;
                            field.children.visit(d, &field.path, Some(&field.path));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// The field `key`, created on first sight. A field seen both inside and
    /// outside array elements counts as inside.
    fn field(&mut self, key: &str, prefix: &str, in_array: Option<&str>) -> &mut Field {
        let i = *self.index.entry(key.to_string()).or_insert_with(|| {
            self.fields.push(Field {
                path: if prefix.is_empty() {
                    key.to_string()
                } else {
                    format!("{prefix}.{key}")
                },
                present: 0,
                types: Counts::default(),
                element_types: Counts::default(),
                in_array: None,
                enclosed: 0,
                children: Fields::default(),
            });
            self.fields.len() - 1
        });
        let field = &mut self.fields[i];
        if field.in_array.is_none() {
            field.in_array = in_array.map(str::to_string);
        }
        field
    }

    /// Appends a column per field, each followed by its children's.
    /// `enclosing` is how many documents these fields were looked for in;
    /// `always_there` is whether every sampled document has those documents.
    fn columns(&self, enclosing: u64, always_there: bool, out: &mut Vec<Column>) {
        for f in &self.fields {
            let mut properties = BTreeMap::from([
                ("mqlc.path".to_string(), f.path.clone()),
                (
                    "mqlc.presence".to_string(),
                    format!("{}/{enclosing}", f.present),
                ),
                ("mqlc.types".to_string(), f.types.summary()),
            ]);
            let mut set = |k: &str, v: String| properties.insert(k.to_string(), v);
            if f.types.has("Array") {
                set("mqlc.array", "true".to_string());
                if !f.element_types.is_empty() {
                    set("mqlc.elementTypes", f.element_types.joined());
                }
            }
            if f.types.has("Document") || f.element_types.has("Document") {
                set("mqlc.embedded", "true".to_string());
            }
            if let Some(array) = &f.in_array {
                set("mqlc.inArray", array.clone());
            }
            let not_null = always_there && f.present == enclosing && !f.types.has("Null");
            out.push(Column {
                name: f.path.clone(),
                r#type: f.types.joined(),
                not_null,
                properties,
            });
            // Elements of arrays, which may be empty, are never always there.
            let children_always_there = not_null && !f.types.has("Array");
            f.children.columns(f.enclosed, children_always_there, out);
        }
    }
}

/// Type counts in the order first seen.
#[derive(Default)]
struct Counts(Vec<(&'static str, u64)>);

impl Counts {
    fn add(&mut self, name: &'static str) {
        match self.0.iter_mut().find(|(n, _)| *n == name) {
            Some((_, count)) => *count += 1,
            None => self.0.push((name, 1)),
        }
    }

    fn has(&self, name: &str) -> bool {
        self.0.iter().any(|(n, _)| *n == name)
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Most frequent first; the sort is stable, so ties stay in seen order.
    fn sorted(&self) -> Vec<(&'static str, u64)> {
        let mut counts = self.0.clone();
        counts.sort_by_key(|c| std::cmp::Reverse(c.1));
        counts
    }

    fn joined(&self) -> String {
        let names: Vec<_> = self.sorted().into_iter().map(|(n, _)| n).collect();
        names.join(" | ")
    }

    fn summary(&self) -> String {
        let parts: Vec<_> = self
            .sorted()
            .into_iter()
            .map(|(n, c)| format!("{n}:{c}"))
            .collect();
        parts.join(",")
    }
}

fn type_name(value: &Bson) -> &'static str {
    match value {
        Bson::Double(_) => "Double",
        Bson::String(_) => "String",
        Bson::Array(_) => "Array",
        Bson::Document(_) => "Document",
        Bson::Boolean(_) => "Boolean",
        Bson::Null => "Null",
        Bson::RegularExpression(_) => "Regex",
        Bson::JavaScriptCode(_) | Bson::JavaScriptCodeWithScope(_) => "JavaScript",
        Bson::Int32(_) => "Int32",
        Bson::Int64(_) => "Int64",
        Bson::Timestamp(_) => "Timestamp",
        Bson::Binary(_) => "Binary",
        Bson::ObjectId(_) => "ObjectId",
        Bson::DateTime(_) => "Date",
        Bson::Symbol(_) => "Symbol",
        Bson::Decimal128(_) => "Decimal128",
        Bson::Undefined => "Undefined",
        Bson::MaxKey => "MaxKey",
        Bson::MinKey => "MinKey",
        Bson::DbPointer(_) => "DbPointer",
    }
}
