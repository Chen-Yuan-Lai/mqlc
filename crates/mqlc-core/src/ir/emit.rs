//! IR → BSON.

use mongodb::bson::{Bson, Document};

use super::*;

pub(super) fn stage(stage: &Stage) -> Document {
    let (name, arg) = match stage {
        Stage::Match(f) => ("$match", Bson::Document(filter(f))),
        Stage::Project(p) => ("$project", fields(p, projection)),
        Stage::Unwind(u) => ("$unwind", unwind(u)),
        Stage::Group(g) => ("$group", group(g)),
        Stage::Sort(s) => ("$sort", sort(s)),
        Stage::Lookup(l) => ("$lookup", lookup(l)),
        Stage::Limit(n) => ("$limit", integer(*n)),
        Stage::Count(field) => ("$count", Bson::String(field.clone())),
        Stage::AddFields(f) => ("$addFields", fields(f, expr)),
        Stage::Facet(f) => ("$facet", fields(f, pipeline)),
        Stage::ReplaceRoot(root) => (
            "$replaceRoot",
            present_keys([("newRoot", Some(expr(root)))]),
        ),
        Stage::UnionWith(u) => ("$unionWith", union_with(u)),
        Stage::Skip(n) => ("$skip", integer(*n)),
        Stage::SetWindowFields(w) => ("$setWindowFields", set_window_fields(w)),
        Stage::Raw(raw) => return raw.document.clone(),
    };
    let mut d = Document::new();
    d.insert(name, arg);
    d
}

fn expr(e: &Expr) -> Bson {
    match e {
        Expr::Path(p) => Bson::String(format!("${p}")),
        Expr::Variable(v) => Bson::String(format!("$${v}")),
        Expr::Literal(b) => b.clone(),
        Expr::Array(items) => Bson::Array(items.iter().map(expr).collect()),
        Expr::Object(f) => fields(f, expr),
        Expr::Op(op, arg) => present_keys([(op.name(), Some(expr(arg)))]),
    }
}

fn filter(f: &Filter) -> Document {
    f.0.iter()
        .map(|clause| match clause {
            Clause::Field(field, Condition::Equals(value)) => (field.clone(), value.clone()),
            Clause::Field(field, Condition::Predicates(ps)) => (
                field.clone(),
                Bson::Document(ps.iter().map(predicate).collect()),
            ),
            Clause::And(fs) => ("$and".to_string(), filters(fs)),
            Clause::Or(fs) => ("$or".to_string(), filters(fs)),
            Clause::Expr(e) => ("$expr".to_string(), expr(e)),
        })
        .collect()
}

fn filters(fs: &[Filter]) -> Bson {
    Bson::Array(fs.iter().map(|f| Bson::Document(filter(f))).collect())
}

fn predicate(p: &Predicate) -> (String, Bson) {
    match p {
        Predicate::Compare(op, operand) => (op.name().to_string(), operand.clone()),
        Predicate::ElemMatch(ElemMatch::Filter(f)) => {
            ("$elemMatch".to_string(), Bson::Document(filter(f)))
        }
        Predicate::ElemMatch(ElemMatch::Predicates(ps)) => (
            "$elemMatch".to_string(),
            Bson::Document(ps.iter().map(predicate).collect()),
        ),
    }
}

fn projection(p: &Projection) -> Bson {
    match p {
        Projection::Include(b) | Projection::Exclude(b) => b.clone(),
        Projection::Nested(f) => fields(f, projection),
        Projection::Computed(e) => expr(e),
    }
}

fn unwind(u: &Unwind) -> Bson {
    match u {
        Unwind::Path(p) => Bson::String(format!("${p}")),
        Unwind::Options {
            path,
            include_array_index,
            preserve_null_and_empty_arrays,
        } => present_keys([
            ("path", Some(Bson::String(format!("${path}")))),
            (
                "includeArrayIndex",
                include_array_index.clone().map(Bson::String),
            ),
            (
                "preserveNullAndEmptyArrays",
                preserve_null_and_empty_arrays.map(Bson::Boolean),
            ),
        ]),
    }
}

fn group(g: &Group) -> Bson {
    let mut d = Document::new();
    d.insert("_id", expr(&g.id));
    for (field, acc) in &g.fields {
        d.insert(field, accumulation(acc));
    }
    Bson::Document(d)
}

fn accumulation(a: &Accumulation) -> Bson {
    present_keys([(a.op.name(), Some(expr(&a.arg)))])
}

fn sort(s: &[(String, SortOrder)]) -> Bson {
    fields(s, |order| match order {
        SortOrder::Ascending(written) | SortOrder::Descending(written) => written.clone(),
    })
}

fn lookup(l: &Lookup) -> Bson {
    present_keys([
        ("from", Some(Bson::String(l.from.clone()))),
        ("localField", l.local_field.clone().map(Bson::String)),
        ("foreignField", l.foreign_field.clone().map(Bson::String)),
        ("let", l.let_vars.as_deref().map(|v| fields(v, expr))),
        ("pipeline", l.pipeline.as_ref().map(pipeline)),
        ("as", Some(Bson::String(l.as_field.clone()))),
    ])
}

fn union_with(u: &UnionWith) -> Bson {
    match u {
        UnionWith::Collection(coll) => Bson::String(coll.clone()),
        UnionWith::Options { coll, pipeline: p } => present_keys([
            ("coll", Some(Bson::String(coll.clone()))),
            ("pipeline", p.as_ref().map(pipeline)),
        ]),
    }
}

fn set_window_fields(w: &SetWindowFields) -> Bson {
    present_keys([
        ("partitionBy", w.partition_by.as_ref().map(expr)),
        ("sortBy", w.sort_by.as_deref().map(sort)),
        ("output", Some(fields(&w.output, accumulation))),
    ])
}

fn pipeline(p: &Pipeline) -> Bson {
    Bson::Array(p.to_documents().into_iter().map(Bson::Document).collect())
}

fn fields<T>(f: &[(String, T)], emit: impl Fn(&T) -> Bson) -> Bson {
    Bson::Document(f.iter().map(|(k, v)| (k.clone(), emit(v))).collect())
}

/// A document of the keys whose value is present, in the order given.
fn present_keys<const N: usize>(entries: [(&str, Option<Bson>); N]) -> Bson {
    Bson::Document(
        entries
            .into_iter()
            .filter_map(|(k, v)| Some((k.to_string(), v?)))
            .collect(),
    )
}

fn integer(n: Integer) -> Bson {
    match n {
        Integer::Int32(n) => Bson::Int32(n),
        Integer::Int64(n) => Bson::Int64(n),
    }
}
