//! Typed IR of an aggregation pipeline, parsed after generation (ADR 0003).
//!
//! The model writes Extended JSON as usual; [`Pipeline::parse`] turns every
//! stage it can represent into a typed [`Stage`] and keeps the rest as
//! [`Stage::Raw`], so any pipeline parses. Serialising gives back an
//! equivalent pipeline: the same values with the same BSON types, in the same
//! key order wherever MongoDB reads key order. Stage options (`$lookup`,
//! `$unwind`, ...) come out in a fixed order, which MongoDB ignores.
//!
//! The operators below are the ones the train split's gold pipelines use, and
//! no others (ADR 0006): a stage using any other operator stays raw.

mod coverage;
mod emit;
mod parse;

use std::collections::BTreeSet;

use mongodb::bson::{Bson, Document};

pub use coverage::{Coverage, CoverageError, SplitCoverage};

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum IrError {
    #[error("pipeline is not valid Extended JSON: {0}")]
    BadExtendedJson(String),
    #[error("pipeline is not an array")]
    NotArray,
    #[error("pipeline stage {0} is not a document")]
    StageNotDocument(usize),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pipeline {
    pub stages: Vec<Stage>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stage {
    Match(Filter),
    Project(Vec<(String, Projection)>),
    Unwind(Unwind),
    Group(Group),
    Sort(Vec<(String, SortOrder)>),
    Lookup(Lookup),
    Limit(Integer),
    Count(String),
    AddFields(Vec<(String, Expr)>),
    Facet(Vec<(String, Pipeline)>),
    ReplaceRoot(Expr),
    UnionWith(UnionWith),
    Skip(Integer),
    SetWindowFields(SetWindowFields),
    Raw(RawStage),
}

/// A `$limit` or `$skip` count, in the integer type it was written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Integer {
    Int32(i32),
    Int64(i64),
}

/// A stage the IR cannot represent, kept as written.
#[derive(Debug, Clone, PartialEq)]
pub struct RawStage {
    pub document: Document,
    /// The first construct in the stage that the IR does not support.
    pub reason: Unsupported,
}

impl RawStage {
    /// The reason, plus every other operator in the stage that the IR does
    /// not know in any position.
    pub fn unsupported(&self) -> BTreeSet<Unsupported> {
        let mut found = BTreeSet::from([self.reason.clone()]);
        unknown_operators(&Bson::Document(self.document.clone()), &mut found);
        found
    }
}

fn unknown_operators(value: &Bson, found: &mut BTreeSet<Unsupported>) {
    match value {
        Bson::Document(d) => {
            for (key, v) in d {
                if key.starts_with('$') && !known_operator(key) {
                    found.insert(Unsupported::Operator(key.clone()));
                }
                unknown_operators(v, found);
            }
        }
        Bson::Array(items) => items.iter().for_each(|v| unknown_operators(v, found)),
        _ => {}
    }
}

fn known_operator(name: &str) -> bool {
    const OTHERS: [&str; 18] = [
        "$and",
        "$or",
        "$expr",
        "$elemMatch",
        "$match",
        "$project",
        "$unwind",
        "$group",
        "$sort",
        "$lookup",
        "$limit",
        "$count",
        "$addFields",
        "$facet",
        "$replaceRoot",
        "$unionWith",
        "$skip",
        "$setWindowFields",
    ];
    OTHERS.contains(&name)
        || ExprOp::from_name(name).is_some()
        || QueryOp::from_name(name).is_some()
        || Accumulator::from_name(name).is_some()
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, thiserror::Error)]
pub enum Unsupported {
    #[error("{0} stage")]
    Stage(String),
    #[error("{0}")]
    Operator(String),
    /// A known stage or operator written in a form the IR does not model.
    #[error("{0}")]
    Shape(&'static str),
}

/// An aggregation expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// `"$a.b"`, stored without the `$`.
    Path(String),
    /// `"$$this.a"`, stored without the `$$`.
    Variable(String),
    /// Any other value: strings without a leading `$`, numbers, dates, ...,
    /// and the argument of `$literal`.
    Literal(Bson),
    Array(Vec<Expr>),
    /// A document of fields, each an expression.
    Object(Vec<(String, Expr)>),
    /// `{<op>: <argument>}`, the argument as written: an array of operands,
    /// an object of named arguments or a single operand.
    Op(ExprOp, Box<Expr>),
}

/// A query (`$match`, `$elemMatch`): clauses that must all hold.
#[derive(Debug, Clone, PartialEq)]
pub struct Filter(pub Vec<Clause>);

#[derive(Debug, Clone, PartialEq)]
pub enum Clause {
    Field(String, Condition),
    And(Vec<Filter>),
    Or(Vec<Filter>),
    Expr(Expr),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Condition {
    /// `{field: value}`: equality, or a regex match for a regex value.
    Equals(Bson),
    /// `{field: {$gt: 1, $lt: 5}}`
    Predicates(Vec<Predicate>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Predicate {
    /// The operand is a query value, never an expression.
    Compare(QueryOp, Bson),
    ElemMatch(ElemMatch),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ElemMatch {
    /// On an array of documents: `{$elemMatch: {qty: {$gt: 5}}}`
    Filter(Filter),
    /// On an array of scalars: `{$elemMatch: {$gt: 5, $lt: 9}}`
    Predicates(Vec<Predicate>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Projection {
    /// `1`, `true`, ... as written.
    Include(Bson),
    /// `0`, `false`, ... as written.
    Exclude(Bson),
    /// `{a: {b: 1}}`
    Nested(Vec<(String, Projection)>),
    Computed(Expr),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Unwind {
    /// `"$a"`, stored without the `$`.
    Path(String),
    Options {
        path: String,
        include_array_index: Option<String>,
        preserve_null_and_empty_arrays: Option<bool>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub id: Expr,
    pub fields: Vec<(String, Accumulation)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Accumulation {
    pub op: Accumulator,
    pub arg: Expr,
}

/// `1` or `-1`, as written (`1`, `1.0`, `{"$numberLong": "1"}`).
#[derive(Debug, Clone, PartialEq)]
pub enum SortOrder {
    Ascending(Bson),
    Descending(Bson),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Lookup {
    pub from: String,
    pub local_field: Option<String>,
    pub foreign_field: Option<String>,
    pub let_vars: Option<Vec<(String, Expr)>>,
    pub pipeline: Option<Pipeline>,
    pub as_field: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum UnionWith {
    Collection(String),
    Options {
        coll: String,
        pipeline: Option<Pipeline>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct SetWindowFields {
    pub partition_by: Option<Expr>,
    pub sort_by: Option<Vec<(String, SortOrder)>>,
    /// Window bounds (`window: {...}`) are not modelled.
    pub output: Vec<(String, Accumulation)>,
}

macro_rules! operators {
    ($(#[$meta:meta])* $name:ident { $($variant:ident = $op:literal,)* }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name {
            $($variant,)*
        }

        impl $name {
            pub fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => $op,)*
                }
            }

            pub fn from_name(name: &str) -> Option<Self> {
                match name {
                    $($op => Some(Self::$variant),)*
                    _ => None,
                }
            }
        }
    };
}

operators! {
    /// Aggregation expression operators.
    ExprOp {
        Add = "$add",
        AllElementsTrue = "$allElementsTrue",
        And = "$and",
        AnyElementTrue = "$anyElementTrue",
        ArrayElemAt = "$arrayElemAt",
        Concat = "$concat",
        ConcatArrays = "$concatArrays",
        Cond = "$cond",
        Convert = "$convert",
        DateDiff = "$dateDiff",
        DateFromString = "$dateFromString",
        Divide = "$divide",
        Eq = "$eq",
        Filter = "$filter",
        First = "$first",
        Floor = "$floor",
        GetField = "$getField",
        Gt = "$gt",
        Gte = "$gte",
        IfNull = "$ifNull",
        In = "$in",
        Let = "$let",
        Literal = "$literal",
        Lt = "$lt",
        Lte = "$lte",
        Map = "$map",
        Max = "$max",
        MergeObjects = "$mergeObjects",
        Multiply = "$multiply",
        Ne = "$ne",
        Or = "$or",
        Range = "$range",
        Reduce = "$reduce",
        RegexFind = "$regexFind",
        RegexMatch = "$regexMatch",
        ReplaceAll = "$replaceAll",
        SetDifference = "$setDifference",
        SetUnion = "$setUnion",
        Size = "$size",
        Slice = "$slice",
        Split = "$split",
        StrLenBytes = "$strLenBytes",
        Subtract = "$subtract",
        Sum = "$sum",
        Switch = "$switch",
        ToDouble = "$toDouble",
        ToInt = "$toInt",
        ToLower = "$toLower",
        ToString = "$toString",
        Trim = "$trim",
        Type = "$type",
        Year = "$year",
    }
}

operators! {
    /// `$group` and `$setWindowFields` accumulators.
    Accumulator {
        AddToSet = "$addToSet",
        Avg = "$avg",
        First = "$first",
        Max = "$max",
        Min = "$min",
        Push = "$push",
        Sum = "$sum",
    }
}

operators! {
    /// Query operators on one field, other than `$elemMatch`.
    QueryOp {
        All = "$all",
        Eq = "$eq",
        Exists = "$exists",
        Gt = "$gt",
        Gte = "$gte",
        In = "$in",
        Lt = "$lt",
        Lte = "$lte",
        Ne = "$ne",
        Nin = "$nin",
        Type = "$type",
    }
}

impl Pipeline {
    /// Never fails: a stage the IR cannot represent becomes [`Stage::Raw`].
    pub fn parse(stages: &[Document]) -> Self {
        Self {
            stages: stages.iter().map(parse::stage).collect(),
        }
    }

    pub fn from_extended_json(json: &serde_json::Value) -> Result<Self, IrError> {
        let Bson::Array(stages) =
            Bson::try_from(json.clone()).map_err(|e| IrError::BadExtendedJson(e.to_string()))?
        else {
            return Err(IrError::NotArray);
        };
        let stages = stages
            .into_iter()
            .enumerate()
            .map(|(i, s)| match s {
                Bson::Document(d) => Ok(d),
                _ => Err(IrError::StageNotDocument(i)),
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self::parse(&stages))
    }

    pub fn to_documents(&self) -> Vec<Document> {
        self.stages.iter().map(emit::stage).collect()
    }

    /// Relaxed Extended JSON, except that 64-bit integers stay `$numberLong`
    /// (relaxed form writes them as plain numbers, which read back as 32-bit).
    pub fn to_extended_json(&self) -> serde_json::Value {
        extended_json(Bson::Array(
            self.to_documents()
                .into_iter()
                .map(Bson::Document)
                .collect(),
        ))
    }

    /// Every raw stage, including those inside `$lookup`, `$facet` and
    /// `$unionWith` pipelines.
    pub fn raw_stages(&self) -> Vec<&RawStage> {
        let mut raw = Vec::new();
        self.collect_raw(&mut raw);
        raw
    }

    fn collect_raw<'a>(&'a self, raw: &mut Vec<&'a RawStage>) {
        for stage in &self.stages {
            match stage {
                Stage::Raw(r) => raw.push(r),
                Stage::Lookup(Lookup {
                    pipeline: Some(p), ..
                })
                | Stage::UnionWith(UnionWith::Options {
                    pipeline: Some(p), ..
                }) => p.collect_raw(raw),
                Stage::Facet(facets) => facets.iter().for_each(|(_, p)| p.collect_raw(raw)),
                _ => {}
            }
        }
    }
}

fn extended_json(value: Bson) -> serde_json::Value {
    match value {
        Bson::Int64(n) => serde_json::json!({"$numberLong": n.to_string()}),
        Bson::Array(items) => items.into_iter().map(extended_json).collect(),
        Bson::Document(d) => {
            serde_json::Value::Object(d.into_iter().map(|(k, v)| (k, extended_json(v))).collect())
        }
        other => other.into_relaxed_extjson(),
    }
}
