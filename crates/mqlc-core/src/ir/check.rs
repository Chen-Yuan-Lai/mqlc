//! Static check: a pipeline against a schema, before it runs.
//!
//! The check follows the documents through the pipeline: it starts from the
//! starting collection's fields and works out, stage by stage, which fields
//! the next stage sees and what types they hold. A field whose contents are
//! not described (a `Document` or `Array` without fields under it, or a value
//! of unknown type) may hold anything, and after a raw stage nothing is
//! known, so nothing is checked: the check only reports what the schema
//! rules out.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use mongodb::bson::Bson;

use super::*;
use crate::dataset::CollectionFields;

/// A BSON type, as far as the check tells types apart: every number is a
/// `Number`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ValueType {
    String,
    Number,
    Date,
    Boolean,
    ObjectId,
    Document,
    Array,
    Null,
    Binary,
    Regex,
}

impl ValueType {
    pub fn name(self) -> &'static str {
        match self {
            Self::String => "String",
            Self::Number => "Number",
            Self::Date => "Date",
            Self::Boolean => "Boolean",
            Self::ObjectId => "ObjectId",
            Self::Document => "Document",
            Self::Array => "Array",
            Self::Null => "Null",
            Self::Binary => "Binary",
            Self::Regex => "Regex",
        }
    }

    /// A word of an official `data_type` (`"Null, String"` has two).
    fn from_official(word: &str) -> Option<Self> {
        Some(match word {
            "String" => Self::String,
            "Number" | "Float" | "Double" | "Decimal128" | "Int32" | "Int64" | "Integer" => {
                Self::Number
            }
            "Date" => Self::Date,
            "Boolean" => Self::Boolean,
            "ObjectId" => Self::ObjectId,
            "Document" | "Object" => Self::Document,
            "Array" => Self::Array,
            "Null" => Self::Null,
            "Binary" => Self::Binary,
            _ => return None,
        })
    }

    /// `None` for types the check does not compare (timestamps, code, ...).
    fn of(value: &Bson) -> Option<Self> {
        Some(match value {
            Bson::String(_) => Self::String,
            Bson::Int32(_) | Bson::Int64(_) | Bson::Double(_) | Bson::Decimal128(_) => Self::Number,
            Bson::DateTime(_) => Self::Date,
            Bson::Boolean(_) => Self::Boolean,
            Bson::ObjectId(_) => Self::ObjectId,
            Bson::Document(_) => Self::Document,
            Bson::Array(_) => Self::Array,
            Bson::Null | Bson::Undefined => Self::Null,
            Bson::Binary(_) => Self::Binary,
            Bson::RegularExpression(_) => Self::Regex,
            _ => return None,
        })
    }
}

/// The types a field may hold; `None` when they are not known.
type Types = Option<BTreeSet<ValueType>>;

fn one(kind: ValueType) -> Types {
    Some(BTreeSet::from([kind]))
}

fn describe(types: &BTreeSet<ValueType>) -> String {
    types
        .iter()
        .map(|k| k.name())
        .collect::<Vec<_>>()
        .join(" or ")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CheckKind {
    /// The starting collection, or one a stage reads, is not in the schema.
    UnknownCollection,
    /// A field path that the documents at that stage do not have.
    UnknownField,
    /// A value compared with a field that never holds its type, or an
    /// operator given a field of a type it does not take.
    TypeMismatch,
    /// `$unwind` on a field that is never an array.
    UnwindNonArray,
}

impl CheckKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::UnknownCollection => "unknown_collection",
            Self::UnknownField => "unknown_field",
            Self::TypeMismatch => "type_mismatch",
            Self::UnwindNonArray => "unwind_non_array",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckError {
    pub kind: CheckKind,
    /// Says where in the pipeline, in words meant for the model.
    pub message: String,
}

impl fmt::Display for CheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// The fields of each collection of one database.
#[derive(Debug, Clone, Default)]
pub struct Schema {
    collections: BTreeMap<String, Shape>,
}

impl Schema {
    /// From an official schema description: each collection with its
    /// `(field_name, data_type)` pairs. Some descriptions name every field
    /// after its collection (`schools.cdsCode` in `schools`); that prefix is
    /// dropped unless the collection has a field of that name.
    pub fn from_official(collections: &[CollectionFields]) -> Self {
        let collections = collections
            .iter()
            .map(|(name, fields)| {
                let prefix = format!("{name}.");
                let prefixed = !fields.iter().any(|(f, _)| f == name);
                let mut shape = Shape::new(format!("collection `{name}`"));
                for (field, data_type) in fields {
                    let path = match field.strip_prefix(&prefix) {
                        Some(rest) if prefixed => rest,
                        _ => field.as_str(),
                    };
                    // Only ever null is what a migration that saw no values
                    // writes, not a type to check against.
                    let types = data_type
                        .split(',')
                        .map(|w| ValueType::from_official(w.trim()))
                        .collect::<Option<BTreeSet<_>>>()
                        .filter(|t| t != &BTreeSet::from([ValueType::Null]));
                    shape.fields.insert(path.to_string(), types);
                }
                shape.fields.entry("_id".to_string()).or_insert(None);
                (name.clone(), shape)
            })
            .collect();
        Self { collections }
    }
}

impl Pipeline {
    /// Everything the schema rules out in this pipeline run on `collection`,
    /// in pipeline order. Empty when the check finds nothing, which does not
    /// make the pipeline right.
    pub fn check(&self, collection: &str, schema: &Schema) -> Vec<CheckError> {
        let mut checker = Checker {
            schema,
            location: Vec::new(),
            errors: Vec::new(),
        };
        let shape = checker.collection(collection);
        checker.pipeline(self, shape);
        checker.errors
    }
}

/// The fields of the documents at some point of a pipeline.
#[derive(Debug, Clone)]
struct Shape {
    /// Every known path, nested ones included, with its types.
    fields: BTreeMap<String, Types>,
    /// Nothing is known: any path may exist, with any type.
    open: bool,
    /// Where these documents come from, for messages.
    origin: String,
}

impl Shape {
    fn new(origin: String) -> Self {
        Self {
            fields: BTreeMap::new(),
            open: false,
            origin,
        }
    }

    fn open() -> Self {
        Self {
            open: true,
            ..Self::new(String::new())
        }
    }

    fn has_children(&self, path: &str) -> bool {
        let prefix = format!("{path}.");
        self.fields
            .range(prefix.clone()..)
            .next()
            .is_some_and(|(k, _)| k.starts_with(&prefix))
    }

    /// May hold fields the shape does not list.
    fn opaque(&self, path: &str, types: &Types) -> bool {
        match types {
            None => true,
            Some(t) => {
                (t.contains(&ValueType::Document) || t.contains(&ValueType::Array))
                    && !self.has_children(path)
            }
        }
    }

    /// The types at `path`, or why it cannot exist. Numeric parts index
    /// into arrays (`a.0.b`).
    fn resolve(&self, path: &str) -> Result<Types, String> {
        if self.open {
            return Ok(None);
        }
        let mut known = String::new();
        let mut types = one(ValueType::Document);
        for part in path.split('.') {
            let candidate = if known.is_empty() {
                part.to_string()
            } else {
                format!("{known}.{part}")
            };
            if let Some(t) = self.fields.get(&candidate) {
                known = candidate;
                types = t.clone();
                continue;
            }
            let array = types.as_ref().is_none_or(|t| t.contains(&ValueType::Array));
            if !known.is_empty() && array && part.parse::<usize>().is_ok() {
                continue;
            }
            if !known.is_empty() && self.opaque(&known, &types) {
                return Ok(None);
            }
            let mut reason = format!("`{path}` does not exist in {}", self.origin);
            if let Some(t) = types.as_ref().filter(|t| {
                !known.is_empty()
                    && !t.contains(&ValueType::Document)
                    && !t.contains(&ValueType::Array)
            }) {
                reason.push_str(&format!(" (`{known}` is {})", describe(t)));
            }
            return Err(reason);
        }
        Ok(types)
    }

    /// Whether `path` goes through an array: in an expression its value is
    /// then an array of what it reaches in each element (in a query it
    /// matches each element instead).
    fn through_array(&self, path: &str) -> bool {
        let parts: Vec<&str> = path.split('.').collect();
        (1..parts.len()).any(|end| {
            let indexed = parts[end].parse::<usize>().is_ok();
            !indexed
                && self
                    .fields
                    .get(&parts[..end].join("."))
                    .is_some_and(|t| t.as_ref().is_some_and(|t| t.contains(&ValueType::Array)))
        })
    }

    /// What `$path` evaluates to in an expression, as `subtree` entries.
    fn value_of(&self, path: &str) -> Vec<(String, Types)> {
        if self.through_array(path) {
            vec![(String::new(), one(ValueType::Array))]
        } else {
            self.subtree(path)
        }
    }

    /// The types of `$path` in an expression.
    fn value_types(&self, path: &str) -> Types {
        self.value_of(path).swap_remove(0).1
    }

    /// `path` and everything under it, as `(suffix, types)` with suffix `""`
    /// for `path` itself and `".b"` for `path.b`.
    fn subtree(&self, path: &str) -> Vec<(String, Types)> {
        let types = self.resolve(path).unwrap_or(None);
        let mut entries = vec![(String::new(), types)];
        if !self.open && self.fields.contains_key(path) {
            let prefix = format!("{path}.");
            entries.extend(
                self.fields
                    .range(prefix.clone()..)
                    .take_while(|(k, _)| k.starts_with(&prefix))
                    .map(|(k, t)| (k[path.len()..].to_string(), t.clone())),
            );
        }
        entries
    }

    fn remove(&mut self, path: &str) {
        let prefix = format!("{path}.");
        self.fields
            .retain(|k, _| k != path && !k.starts_with(&prefix));
    }

    /// Sets `path` to `entries` (see `subtree`), replacing what was there.
    /// Missing parents get their types in `parents`, or `Document`.
    fn set(&mut self, path: &str, entries: Vec<(String, Types)>, parents: &Shape) {
        self.remove(path);
        let mut parent = String::new();
        for part in path.split('.').collect::<Vec<_>>().split_last().unwrap().1 {
            parent = if parent.is_empty() {
                part.to_string()
            } else {
                format!("{parent}.{part}")
            };
            if !self.fields.contains_key(&parent) {
                let types = match parents.fields.get(&parent) {
                    Some(t) => t.clone(),
                    None => one(ValueType::Document),
                };
                self.fields.insert(parent.clone(), types);
            }
        }
        for (suffix, types) in entries {
            self.fields.insert(format!("{path}{suffix}"), types);
        }
    }

    /// The documents under `path`: a sub-document's fields, or the fields of
    /// an array's documents.
    fn below(&self, path: &str, origin: String) -> Shape {
        if self.open || !self.has_children(path) {
            return Shape::open();
        }
        let mut shape = Shape::new(origin);
        for (suffix, types) in self.subtree(path).into_iter().skip(1) {
            shape.fields.insert(suffix[1..].to_string(), types);
        }
        shape
    }

    /// An array of documents of shape `items`, as `subtree` entries.
    fn array_of(items: &Shape) -> Vec<(String, Types)> {
        let mut entries = vec![(String::new(), one(ValueType::Array))];
        if !items.open {
            entries.extend(
                items
                    .fields
                    .iter()
                    .map(|(k, t)| (format!(".{k}"), t.clone())),
            );
        }
        entries
    }

    /// Documents that are either of these shapes (`$unionWith`).
    fn union(mut self, other: Shape) -> Shape {
        if self.open || other.open {
            return Shape::open();
        }
        for (path, types) in other.fields {
            let merged = match (self.fields.remove(&path), types) {
                (None, t) => t,
                (Some(Some(mut a)), Some(b)) => {
                    a.extend(b);
                    Some(a)
                }
                (Some(_), _) => None,
            };
            self.fields.insert(path, merged);
        }
        self
    }
}

struct Checker<'a> {
    schema: &'a Schema,
    /// The stage being checked, outermost first: `stage 2 ($lookup)`.
    location: Vec<String>,
    errors: Vec<CheckError>,
}

impl Checker<'_> {
    fn report(&mut self, kind: CheckKind, message: String) {
        let message = if self.location.is_empty() {
            message
        } else {
            format!("{}: {message}", self.location.join(" > "))
        };
        let error = CheckError { kind, message };
        if !self.errors.contains(&error) {
            self.errors.push(error);
        }
    }

    fn collection(&mut self, name: &str) -> Shape {
        match self.schema.collections.get(name) {
            Some(shape) => shape.clone(),
            None => {
                self.report(
                    CheckKind::UnknownCollection,
                    format!("collection `{name}` is not in the schema description"),
                );
                Shape::open()
            }
        }
    }

    /// The types at `path`, reporting it when it cannot exist.
    fn field(&mut self, shape: &Shape, path: &str) -> Types {
        match shape.resolve(path) {
            Ok(types) => types,
            Err(reason) => {
                self.report(CheckKind::UnknownField, reason);
                None
            }
        }
    }

    fn pipeline(&mut self, pipeline: &Pipeline, mut shape: Shape) -> Shape {
        for (i, stage) in pipeline.stages.iter().enumerate() {
            let at = format!("stage {} ({})", i + 1, stage.name());
            self.location.push(at.clone());
            shape = self.stage(stage, shape);
            if !shape.open {
                shape.origin = format!("the documents after {}", self.location.join(" > "));
            }
            self.location.pop();
        }
        shape
    }

    fn stage(&mut self, stage: &Stage, shape: Shape) -> Shape {
        match stage {
            Stage::Match(filter) => {
                self.filter(filter, &shape);
                shape
            }
            Stage::Project(items) => self.project(items, &shape),
            Stage::Unwind(unwind) => self.unwind(unwind, shape),
            Stage::Group(group) => {
                self.expr(&group.id, &shape);
                let mut out = Shape::new(String::new());
                out.set("_id", self.value(&group.id, &shape), &shape);
                for (name, acc) in &group.fields {
                    let entries = self.accumulation(acc, &shape);
                    out.set(name, entries, &shape);
                }
                out
            }
            Stage::Sort(keys) => {
                for (path, _) in keys {
                    self.field(&shape, path);
                }
                shape
            }
            Stage::Lookup(lookup) => {
                let foreign = self.collection(&lookup.from);
                if let Some(local) = &lookup.local_field {
                    self.field(&shape, local);
                }
                if let Some(foreign_field) = &lookup.foreign_field {
                    self.field(&foreign, foreign_field);
                }
                for (_, e) in lookup.let_vars.iter().flatten() {
                    self.expr(e, &shape);
                }
                let joined = match &lookup.pipeline {
                    Some(p) => self.pipeline(p, foreign),
                    None => foreign,
                };
                let mut out = shape.clone();
                out.set(&lookup.as_field, Shape::array_of(&joined), &shape);
                out
            }
            Stage::Limit(_) | Stage::Skip(_) => shape,
            Stage::Count(name) => {
                let mut out = Shape::new(String::new());
                out.fields.insert(name.clone(), one(ValueType::Number));
                out
            }
            Stage::AddFields(items) => {
                let mut out = shape.clone();
                for (path, e) in items {
                    self.expr(e, &shape);
                    out.set(path, self.value(e, &shape), &shape);
                }
                out
            }
            Stage::Facet(facets) => {
                let mut out = Shape::new(String::new());
                for (name, p) in facets {
                    let result = self.pipeline(p, shape.clone());
                    out.set(name, Shape::array_of(&result), &shape);
                }
                out
            }
            Stage::ReplaceRoot(root) => {
                self.expr(root, &shape);
                match root {
                    Expr::Path(p) => shape.below(p, String::new()),
                    Expr::Object(_) => {
                        let mut out = Shape::new(String::new());
                        for (suffix, types) in self.value(root, &shape).into_iter().skip(1) {
                            out.fields.insert(suffix[1..].to_string(), types);
                        }
                        out
                    }
                    _ => Shape::open(),
                }
            }
            Stage::UnionWith(union) => {
                let (coll, pipeline) = match union {
                    UnionWith::Collection(c) => (c, None),
                    UnionWith::Options { coll, pipeline } => (coll, pipeline.as_ref()),
                };
                let mut other = self.collection(coll);
                if let Some(p) = pipeline {
                    other = self.pipeline(p, other);
                }
                shape.union(other)
            }
            Stage::SetWindowFields(window) => {
                if let Some(e) = &window.partition_by {
                    self.expr(e, &shape);
                }
                for (path, _) in window.sort_by.iter().flatten() {
                    self.field(&shape, path);
                }
                let mut out = shape.clone();
                for (name, acc) in &window.output {
                    let entries = self.accumulation(acc, &shape);
                    out.set(name, entries, &shape);
                }
                out
            }
            Stage::Raw(_) => Shape::open(),
        }
    }

    fn project(&mut self, items: &[(String, Projection)], shape: &Shape) -> Shape {
        fn includes(p: &Projection) -> bool {
            match p {
                Projection::Include(_) | Projection::Computed(_) => true,
                Projection::Exclude(_) => false,
                Projection::Nested(items) => items.iter().any(|(_, p)| includes(p)),
            }
        }
        if !items.iter().any(|(_, p)| includes(p)) {
            let mut out = shape.clone();
            Self::exclude(items, "", &mut out);
            return out;
        }
        let mut out = Shape::new(String::new());
        if !items
            .iter()
            .any(|(k, p)| k == "_id" && matches!(p, Projection::Exclude(_)))
        {
            out.set("_id", shape.subtree("_id"), shape);
        }
        self.include(items, "", shape, &mut out);
        out
    }

    fn include(
        &mut self,
        items: &[(String, Projection)],
        prefix: &str,
        shape: &Shape,
        out: &mut Shape,
    ) {
        for (key, p) in items {
            let path = format!("{prefix}{key}");
            match p {
                Projection::Include(_) => {
                    self.field(shape, &path);
                    out.set(&path, shape.subtree(&path), shape);
                }
                Projection::Exclude(_) => out.remove(&path),
                Projection::Nested(items) => self.include(items, &format!("{path}."), shape, out),
                Projection::Computed(e) => {
                    self.expr(e, shape);
                    out.set(&path, self.value(e, shape), shape);
                }
            }
        }
    }

    /// MongoDB excludes a missing field without complaint, so nothing here
    /// is checked.
    fn exclude(items: &[(String, Projection)], prefix: &str, out: &mut Shape) {
        for (key, p) in items {
            let path = format!("{prefix}{key}");
            match p {
                Projection::Nested(items) => Self::exclude(items, &format!("{path}."), out),
                _ => out.remove(&path),
            }
        }
    }

    fn unwind(&mut self, unwind: &Unwind, shape: Shape) -> Shape {
        let (path, index) = match unwind {
            Unwind::Path(p) => (p, None),
            Unwind::Options {
                path,
                include_array_index,
                ..
            } => (path, include_array_index.as_ref()),
        };
        let types = self.field(&shape, path);
        let through_array = shape.through_array(path);
        if let Some(t) = types
            .as_ref()
            .filter(|t| !through_array && !t.contains(&ValueType::Array))
        {
            self.report(
                CheckKind::UnwindNonArray,
                format!(
                    "`$unwind` on `{path}`, which is {} and never an array, passes each document \
                     through unchanged: drop it, or if you took `{path}` for an array, rethink \
                     the stages that rely on that",
                    describe(t)
                ),
            );
        }
        let mut out = shape.clone();
        if !shape.open && shape.fields.contains_key(path) {
            let element = if shape.has_children(path) {
                one(ValueType::Document)
            } else {
                None
            };
            out.fields.insert(path.clone(), element);
        }
        if let Some(index) = index {
            out.set(index, vec![(String::new(), one(ValueType::Number))], &shape);
        }
        out
    }

    fn accumulation(&mut self, acc: &Accumulation, shape: &Shape) -> Vec<(String, Types)> {
        self.expr(&acc.arg, shape);
        match acc.op {
            Accumulator::Sum | Accumulator::Avg => vec![(String::new(), one(ValueType::Number))],
            Accumulator::First | Accumulator::Max | Accumulator::Min => self.value(&acc.arg, shape),
            Accumulator::Push | Accumulator::AddToSet => {
                let mut entries = self.value(&acc.arg, shape);
                entries[0].1 = one(ValueType::Array);
                entries
            }
        }
    }

    /// What an expression evaluates to, as `Shape::subtree` entries.
    fn value(&self, e: &Expr, shape: &Shape) -> Vec<(String, Types)> {
        match e {
            Expr::Path(p) => shape.value_of(p),
            Expr::Object(fields) => {
                let mut entries = vec![(String::new(), one(ValueType::Document))];
                for (key, e) in fields {
                    entries.extend(
                        self.value(e, shape)
                            .into_iter()
                            .map(|(suffix, t)| (format!(".{key}{suffix}"), t)),
                    );
                }
                entries
            }
            _ => vec![(String::new(), type_of(e, shape))],
        }
    }

    fn filter(&mut self, filter: &Filter, shape: &Shape) {
        for clause in &filter.0 {
            match clause {
                Clause::Field(path, condition) => {
                    let types = self.field(shape, path);
                    match condition {
                        Condition::Equals(value) => self.compare(path, &types, value),
                        Condition::Predicates(predicates) => {
                            for p in predicates {
                                self.predicate(path, &types, p, shape);
                            }
                        }
                    }
                }
                Clause::And(filters) | Clause::Or(filters) => {
                    for f in filters {
                        self.filter(f, shape);
                    }
                }
                Clause::Expr(e) => self.expr(e, shape),
            }
        }
    }

    fn predicate(&mut self, path: &str, types: &Types, predicate: &Predicate, shape: &Shape) {
        match predicate {
            Predicate::Compare(op, value) => match op {
                QueryOp::Eq
                | QueryOp::Ne
                | QueryOp::Gt
                | QueryOp::Gte
                | QueryOp::Lt
                | QueryOp::Lte => self.compare(path, types, value),
                QueryOp::In | QueryOp::Nin | QueryOp::All => {
                    if let Bson::Array(values) = value {
                        for v in values {
                            self.compare(path, types, v);
                        }
                    }
                }
                QueryOp::Exists | QueryOp::Type => {}
            },
            Predicate::ElemMatch(ElemMatch::Filter(f)) => {
                let elements = shape.below(path, format!("the elements of `{path}`"));
                self.filter(f, &elements);
            }
            // The check knows nothing of an array's scalar elements.
            Predicate::ElemMatch(ElemMatch::Predicates(_)) => {}
        }
    }

    /// Reports comparing `path` with `value` when the field never holds the
    /// value's type. Arrays match their elements, of unknown type.
    fn compare(&mut self, path: &str, types: &Types, value: &Bson) {
        let Some(types) = types else { return };
        if types.contains(&ValueType::Array) {
            return;
        }
        let ok = match ValueType::of(value) {
            None | Some(ValueType::Null | ValueType::Array | ValueType::Document) => true,
            Some(ValueType::Regex) => types.contains(&ValueType::String),
            Some(kind) => types.contains(&kind),
        };
        if !ok {
            let kind = ValueType::of(value).expect("checked above").name();
            self.report(
                CheckKind::TypeMismatch,
                format!(
                    "`{path}` is {} but is compared with the {kind} {}",
                    describe(types),
                    value.clone().into_relaxed_extjson()
                ),
            );
        }
    }

    fn expr(&mut self, e: &Expr, shape: &Shape) {
        match e {
            Expr::Path(p) => {
                self.field(shape, p);
            }
            Expr::Variable(_) | Expr::Literal(_) => {}
            Expr::Array(items) => items.iter().for_each(|e| self.expr(e, shape)),
            Expr::Object(fields) => fields.iter().for_each(|(_, e)| self.expr(e, shape)),
            Expr::Op(op, arg) => {
                self.expr(arg, shape);
                self.operands(*op, arg, shape);
            }
        }
    }

    /// Type checks of one operator's operands.
    fn operands(&mut self, op: ExprOp, arg: &Expr, shape: &Shape) {
        use ExprOp::*;
        match (op, arg) {
            (Eq | Ne | Gt | Gte | Lt | Lte, Expr::Array(pair)) if pair.len() == 2 => {
                match (&pair[0], &pair[1]) {
                    (Expr::Path(p), Expr::Literal(v)) | (Expr::Literal(v), Expr::Path(p)) => {
                        let types = shape.value_types(p);
                        self.compare(p, &types, v);
                    }
                    _ => {}
                }
            }
            (Year, _) => {
                let date = match arg {
                    Expr::Array(items) if items.len() == 1 => &items[0],
                    Expr::Object(fields) => match fields.iter().find(|(k, _)| k == "date") {
                        Some((_, e)) => e,
                        None => return,
                    },
                    e => e,
                };
                self.date_operand(op, date, shape);
            }
            (DateDiff, Expr::Object(fields)) => {
                for (key, e) in fields {
                    if key == "startDate" || key == "endDate" {
                        self.date_operand(op, e, shape);
                    }
                }
            }
            _ => {}
        }
    }

    fn date_operand(&mut self, op: ExprOp, e: &Expr, shape: &Shape) {
        let Expr::Path(p) = e else { return };
        if shape.resolve(p).is_err() {
            return;
        }
        let Some(types) = shape.value_types(p) else {
            return;
        };
        if !types.contains(&ValueType::Date) && !types.contains(&ValueType::Array) {
            self.report(
                CheckKind::TypeMismatch,
                format!(
                    "`{}` needs a Date, but `{p}` is {}",
                    op.name(),
                    describe(&types)
                ),
            );
        }
    }
}

/// The types of an expression's value, where the check knows them.
fn type_of(e: &Expr, shape: &Shape) -> Types {
    use ExprOp::*;
    match e {
        Expr::Path(p) => shape.value_types(p),
        Expr::Variable(_) => None,
        Expr::Literal(v) => ValueType::of(v).map(|k| BTreeSet::from([k])),
        Expr::Array(_) => one(ValueType::Array),
        Expr::Object(_) => one(ValueType::Document),
        Expr::Op(op, _) => match op {
            Multiply | Divide | Floor | Size | StrLenBytes | ToDouble | ToInt | DateDiff | Year
            | Sum => one(ValueType::Number),
            Concat | ToLower | ToString | Trim | ReplaceAll => one(ValueType::String),
            Eq | Ne | Gt | Gte | Lt | Lte | And | Or | In | RegexMatch | AllElementsTrue
            | AnyElementTrue => one(ValueType::Boolean),
            DateFromString => one(ValueType::Date),
            Split | Map | Filter | ConcatArrays | SetUnion | SetDifference | Range | Slice => {
                one(ValueType::Array)
            }
            MergeObjects => one(ValueType::Document),
            _ => None,
        },
    }
}
