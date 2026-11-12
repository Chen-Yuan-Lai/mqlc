//! BSON → IR. Each function returns the first construct it cannot represent;
//! [`stage`] turns that into a raw stage.

use mongodb::bson::{Bson, Document};

use super::*;

type Parsed<T> = Result<T, Unsupported>;

pub(super) fn stage(doc: &Document) -> Stage {
    typed_stage(doc).unwrap_or_else(|reason| {
        Stage::Raw(RawStage {
            document: doc.clone(),
            reason,
        })
    })
}

fn typed_stage(doc: &Document) -> Parsed<Stage> {
    let mut keys = doc.iter();
    let (Some((name, arg)), None) = (keys.next(), keys.next()) else {
        return Err(Unsupported::Shape("a stage must have exactly one key"));
    };
    Ok(match name.as_str() {
        "$match" => Stage::Match(filter(document(arg, "$match takes a document")?)?),
        "$project" => Stage::Project(fields(
            document(arg, "$project takes a document")?,
            projection,
        )?),
        "$unwind" => Stage::Unwind(unwind(arg)?),
        "$group" => Stage::Group(group(document(arg, "$group takes a document")?)?),
        "$sort" => Stage::Sort(sort(document(arg, "$sort takes a document")?)?),
        "$lookup" => Stage::Lookup(lookup(document(arg, "$lookup takes a document")?)?),
        "$limit" => Stage::Limit(integer(arg, "$limit takes an integer")?),
        "$count" => Stage::Count(string(arg, "$count takes a string")?.to_string()),
        "$addFields" => {
            Stage::AddFields(fields(document(arg, "$addFields takes a document")?, expr)?)
        }
        "$facet" => Stage::Facet(fields(document(arg, "$facet takes a document")?, pipeline)?),
        "$replaceRoot" => Stage::ReplaceRoot(replace_root(document(
            arg,
            "$replaceRoot takes a document",
        )?)?),
        "$unionWith" => Stage::UnionWith(union_with(arg)?),
        "$skip" => Stage::Skip(integer(arg, "$skip takes an integer")?),
        "$setWindowFields" => Stage::SetWindowFields(set_window_fields(document(
            arg,
            "$setWindowFields takes a document",
        )?)?),
        other => return Err(Unsupported::Stage(other.to_string())),
    })
}

fn expr(value: &Bson) -> Parsed<Expr> {
    Ok(match value {
        Bson::String(s) => {
            if let Some(var) = s.strip_prefix("$$") {
                Expr::Variable(var.to_string())
            } else if let Some(path) = s.strip_prefix('$') {
                Expr::Path(path.to_string())
            } else {
                Expr::Literal(value.clone())
            }
        }
        Bson::Array(items) => Expr::Array(items.iter().map(expr).collect::<Parsed<_>>()?),
        Bson::Document(d) => match operator(d)? {
            None => Expr::Object(fields(d, expr)?),
            Some((name, arg)) => {
                let op = ExprOp::from_name(name)
                    .ok_or_else(|| Unsupported::Operator(name.to_string()))?;
                let arg = match op {
                    ExprOp::Literal => Expr::Literal(arg.clone()),
                    _ => expr(arg)?,
                };
                Expr::Op(op, Box::new(arg))
            }
        },
        _ => Expr::Literal(value.clone()),
    })
}

/// The operator and argument of a `{$op: argument}` document, or `None` for a
/// document of plain fields.
fn operator(d: &Document) -> Parsed<Option<(&str, &Bson)>> {
    match (operator_count(d), d.len()) {
        (0, _) => Ok(None),
        (1, 1) => Ok(d.iter().next().map(|(k, v)| (k.as_str(), v))),
        _ => Err(Unsupported::Shape(
            "an operator document must have exactly one key",
        )),
    }
}

fn filter(d: &Document) -> Parsed<Filter> {
    d.iter()
        .map(|(key, value)| {
            Ok(match key.as_str() {
                "$and" => Clause::And(filters(value)?),
                "$or" => Clause::Or(filters(value)?),
                "$expr" => Clause::Expr(expr(value)?),
                op if op.starts_with('$') => return Err(Unsupported::Operator(op.to_string())),
                field => Clause::Field(field.to_string(), condition(value)?),
            })
        })
        .collect::<Parsed<_>>()
        .map(Filter)
}

fn filters(value: &Bson) -> Parsed<Vec<Filter>> {
    const SHAPE: &str = "$and and $or take an array of documents";
    let Bson::Array(items) = value else {
        return Err(Unsupported::Shape(SHAPE));
    };
    items.iter().map(|i| filter(document(i, SHAPE)?)).collect()
}

fn condition(value: &Bson) -> Parsed<Condition> {
    match value {
        Bson::Document(d) if operator_count(d) > 0 => predicates(d).map(Condition::Predicates),
        _ => Ok(Condition::Equals(value.clone())),
    }
}

fn predicates(d: &Document) -> Parsed<Vec<Predicate>> {
    if operator_count(d) < d.len() {
        return Err(Unsupported::Shape("query operators mixed with fields"));
    }
    d.iter()
        .map(|(name, operand)| match name.as_str() {
            "$elemMatch" => elem_match(document(operand, "$elemMatch takes a document")?)
                .map(Predicate::ElemMatch),
            name => QueryOp::from_name(name)
                .map(|op| Predicate::Compare(op, operand.clone()))
                .ok_or_else(|| Unsupported::Operator(name.to_string())),
        })
        .collect()
}

/// Operators on the element itself (`$gt`, ...) for arrays of scalars;
/// otherwise a query on the element's fields.
fn elem_match(d: &Document) -> Parsed<ElemMatch> {
    match d.keys().next().map(String::as_str) {
        Some(k) if k.starts_with('$') && !matches!(k, "$and" | "$or" | "$expr") => {
            predicates(d).map(ElemMatch::Predicates)
        }
        _ => filter(d).map(ElemMatch::Filter),
    }
}

fn operator_count(d: &Document) -> usize {
    d.keys().filter(|k| k.starts_with('$')).count()
}

fn projection(value: &Bson) -> Parsed<Projection> {
    let include = match value {
        Bson::Boolean(b) => Some(*b),
        Bson::Int32(n) => Some(*n != 0),
        Bson::Int64(n) => Some(*n != 0),
        Bson::Double(x) => Some(*x != 0.0),
        _ => None,
    };
    match (include, value) {
        (Some(true), _) => Ok(Projection::Include(value.clone())),
        (Some(false), _) => Ok(Projection::Exclude(value.clone())),
        (None, Bson::Document(d)) if d.is_empty() => {
            Err(Unsupported::Shape("a projection cannot be empty"))
        }
        (None, Bson::Document(d)) if operator(d)?.is_none() => {
            Ok(Projection::Nested(fields(d, projection)?))
        }
        _ => Ok(Projection::Computed(expr(value)?)),
    }
}

fn unwind(value: &Bson) -> Parsed<Unwind> {
    let path = |v: &Bson| {
        string(v, "$unwind path must be a string")?
            .strip_prefix('$')
            .map(str::to_string)
            .ok_or(Unsupported::Shape("$unwind path must start with $"))
    };
    let d = match value {
        Bson::String(_) => return Ok(Unwind::Path(path(value)?)),
        Bson::Document(d) => d,
        _ => return Err(Unsupported::Shape("$unwind takes a path or a document")),
    };
    let (mut unwound, mut index, mut preserve) = (None, None, None);
    for (key, v) in d {
        match key.as_str() {
            "path" => unwound = Some(path(v)?),
            "includeArrayIndex" => {
                index = Some(string(v, "includeArrayIndex takes a string")?.to_string())
            }
            "preserveNullAndEmptyArrays" => match v {
                Bson::Boolean(b) => preserve = Some(*b),
                _ => {
                    return Err(Unsupported::Shape(
                        "preserveNullAndEmptyArrays takes a boolean",
                    ));
                }
            },
            _ => return Err(Unsupported::Shape("unknown $unwind option")),
        }
    }
    Ok(Unwind::Options {
        path: unwound.ok_or(Unsupported::Shape("$unwind needs a path"))?,
        include_array_index: index,
        preserve_null_and_empty_arrays: preserve,
    })
}

fn group(d: &Document) -> Parsed<Group> {
    let id = expr(
        d.get("_id")
            .ok_or(Unsupported::Shape("$group needs an _id"))?,
    )?;
    let fields = d
        .iter()
        .filter(|(k, _)| *k != "_id")
        .map(|(k, v)| Ok((k.clone(), accumulation(v)?)))
        .collect::<Parsed<_>>()?;
    Ok(Group { id, fields })
}

fn accumulation(value: &Bson) -> Parsed<Accumulation> {
    const SHAPE: &str = "an accumulator must be a {$op: argument} document";
    let Some((name, arg)) = operator(document(value, SHAPE)?)? else {
        return Err(Unsupported::Shape(SHAPE));
    };
    let op = Accumulator::from_name(name).ok_or_else(|| Unsupported::Operator(name.to_string()))?;
    Ok(Accumulation {
        op,
        arg: expr(arg)?,
    })
}

fn sort(d: &Document) -> Parsed<Vec<(String, SortOrder)>> {
    fields(d, |value| {
        let n = match value {
            Bson::Int32(n) => f64::from(*n),
            Bson::Int64(n) => *n as f64,
            Bson::Double(x) => *x,
            _ => f64::NAN,
        };
        match n {
            1.0 => Ok(SortOrder::Ascending(value.clone())),
            -1.0 => Ok(SortOrder::Descending(value.clone())),
            _ => Err(Unsupported::Shape("a sort order must be 1 or -1")),
        }
    })
}

fn lookup(d: &Document) -> Parsed<Lookup> {
    let text = |v: &Bson| {
        string(
            v,
            "$lookup from, localField, foreignField and as take strings",
        )
        .map(str::to_string)
    };
    let (mut from, mut local, mut foreign, mut let_vars, mut inner, mut as_field) =
        (None, None, None, None, None, None);
    for (key, v) in d {
        match key.as_str() {
            "from" => from = Some(text(v)?),
            "localField" => local = Some(text(v)?),
            "foreignField" => foreign = Some(text(v)?),
            "let" => let_vars = Some(fields(document(v, "$lookup let takes a document")?, expr)?),
            "pipeline" => inner = Some(pipeline(v)?),
            "as" => as_field = Some(text(v)?),
            _ => return Err(Unsupported::Shape("unknown $lookup option")),
        }
    }
    Ok(Lookup {
        from: from.ok_or(Unsupported::Shape("$lookup needs from"))?,
        local_field: local,
        foreign_field: foreign,
        let_vars,
        pipeline: inner,
        as_field: as_field.ok_or(Unsupported::Shape("$lookup needs as"))?,
    })
}

fn replace_root(d: &Document) -> Parsed<Expr> {
    match (d.get("newRoot"), d.len()) {
        (Some(root), 1) => expr(root),
        _ => Err(Unsupported::Shape("$replaceRoot takes only newRoot")),
    }
}

fn union_with(value: &Bson) -> Parsed<UnionWith> {
    let d = match value {
        Bson::String(coll) => return Ok(UnionWith::Collection(coll.clone())),
        Bson::Document(d) => d,
        _ => {
            return Err(Unsupported::Shape(
                "$unionWith takes a collection or a document",
            ));
        }
    };
    let (mut coll, mut inner) = (None, None);
    for (key, v) in d {
        match key.as_str() {
            "coll" => coll = Some(string(v, "$unionWith coll takes a string")?.to_string()),
            "pipeline" => inner = Some(pipeline(v)?),
            _ => return Err(Unsupported::Shape("unknown $unionWith option")),
        }
    }
    Ok(UnionWith::Options {
        coll: coll.ok_or(Unsupported::Shape("$unionWith needs coll"))?,
        pipeline: inner,
    })
}

fn set_window_fields(d: &Document) -> Parsed<SetWindowFields> {
    let (mut partition_by, mut sort_by, mut output) = (None, None, None);
    for (key, v) in d {
        match key.as_str() {
            "partitionBy" => partition_by = Some(expr(v)?),
            "sortBy" => sort_by = Some(sort(document(v, "sortBy takes a document")?)?),
            "output" => {
                output = Some(fields(
                    document(v, "output takes a document")?,
                    accumulation,
                )?)
            }
            _ => return Err(Unsupported::Shape("unknown $setWindowFields option")),
        }
    }
    Ok(SetWindowFields {
        partition_by,
        sort_by,
        output: output.ok_or(Unsupported::Shape("$setWindowFields needs output"))?,
    })
}

fn pipeline(value: &Bson) -> Parsed<Pipeline> {
    const SHAPE: &str = "a sub-pipeline must be an array of documents";
    let Bson::Array(stages) = value else {
        return Err(Unsupported::Shape(SHAPE));
    };
    let stages = stages
        .iter()
        .map(|s| document(s, SHAPE).cloned())
        .collect::<Parsed<Vec<_>>>()?;
    Ok(Pipeline::parse(&stages))
}

fn fields<T>(d: &Document, parse: impl Fn(&Bson) -> Parsed<T>) -> Parsed<Vec<(String, T)>> {
    d.iter().map(|(k, v)| Ok((k.clone(), parse(v)?))).collect()
}

fn document<'a>(value: &'a Bson, shape: &'static str) -> Parsed<&'a Document> {
    match value {
        Bson::Document(d) => Ok(d),
        _ => Err(Unsupported::Shape(shape)),
    }
}

fn string<'a>(value: &'a Bson, shape: &'static str) -> Parsed<&'a str> {
    match value {
        Bson::String(s) => Ok(s),
        _ => Err(Unsupported::Shape(shape)),
    }
}

fn integer(value: &Bson, shape: &'static str) -> Parsed<Integer> {
    match value {
        Bson::Int32(n) => Ok(Integer::Int32(*n)),
        Bson::Int64(n) => Ok(Integer::Int64(*n)),
        _ => Err(Unsupported::Shape(shape)),
    }
}
