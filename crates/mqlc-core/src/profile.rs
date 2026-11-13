//! The schema profiler: samples each collection of a database and infers its
//! field paths, their BSON types and presence, arrays and embedded
//! documents, as an auto MDL project.

use std::collections::{BTreeMap, HashMap};

use futures_util::TryStreamExt;
use mongodb::Client;
use mongodb::bson::{Bson, DateTime, Document, doc, oid::ObjectId};

use crate::mdl::{
    Column, Instruction, JoinType, Mdl, MdlProject, Model, Relationship, TableReference,
};

const CATALOG: &str = "mqlc";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileOptions {
    /// Documents read per collection: the first ones in `_id` order, so the
    /// same data always gives the same profile.
    pub sample_size: usize,
    /// Sensitive fields, as `<collection>.<path>`, left out of the profile
    /// with everything under them: MongoDB never returns their values, and
    /// the MDL keeps only their paths, as the model's `mqlc.excluded`.
    pub exclude: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error(transparent)]
    Mongo(#[from] mongodb::error::Error),
    /// Excluding a field that is not there is more likely a typo than an
    /// intent, and a typo would let the field through.
    #[error("cannot exclude {0}: no collection.field of that name in the database")]
    UnknownExclusion(String),
}

impl Default for ProfileOptions {
    /// On AptMQL-Bench, 100,000 documents find every field a full scan finds
    /// (1,000 miss 42 sparse ones) and profile all 21 databases in about 35 s.
    fn default() -> Self {
        Self {
            sample_size: 100_000,
            exclude: Vec::new(),
        }
    }
}

/// Profiles every collection of `db_id` (system collections aside).
pub async fn profile(
    client: &Client,
    db_id: &str,
    options: &ProfileOptions,
) -> Result<MdlProject, ProfileError> {
    let db = client.database(db_id);
    let mut names: Vec<String> = db
        .list_collection_names()
        .await?
        .into_iter()
        .filter(|n| !n.starts_with("system."))
        .collect();
    names.sort();
    let excluded = exclusions(&db, &names, &options.exclude).await?;
    let mut sampled = Vec::new();
    for name in names {
        let collection = db.collection::<Document>(&name);
        let excluded = excluded.get(&name).cloned().unwrap_or_default();
        let hidden: Document = excluded
            .iter()
            .map(|p| (p.clone(), Bson::Int32(0)))
            .collect();
        let docs: Vec<Document> = collection
            .find(doc! {})
            .projection(hidden)
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
        sampled.push(CollectionSample {
            name,
            sampled: docs.len() as u64,
            documents,
            fields,
            excluded,
        });
    }
    let mut relationships = Vec::new();
    for s in &sampled {
        for field in s.fields.all() {
            if field.path == "_id" {
                continue;
            }
            if let Some(r) = reference(&db, s, field, &sampled).await? {
                relationships.push(r);
            }
        }
    }
    let models = sampled
        .into_iter()
        .map(|s| {
            let mut columns = Vec::new();
            s.fields.columns(s.sampled, true, &mut columns);
            let mut properties = BTreeMap::from([
                ("mqlc.sampled".to_string(), s.sampled.into()),
                ("mqlc.documents".to_string(), s.documents.into()),
            ]);
            if !s.excluded.is_empty() {
                properties.insert("mqlc.excluded".to_string(), s.excluded.into());
            }
            Model {
                table_reference: TableReference {
                    table: s.name.clone(),
                },
                name: s.name,
                columns,
                properties,
            }
        })
        .collect();
    Ok(MdlProject {
        mdl: Mdl {
            catalog: CATALOG.to_string(),
            schema: db_id.to_string(),
            models,
            relationships,
        },
        instructions: instructions(options.sample_size),
    })
}

/// One collection's sample.
struct CollectionSample {
    name: String,
    sampled: u64,
    documents: u64,
    fields: Fields,
    /// Paths left out, in order, none under another.
    excluded: Vec<String>,
}

/// The excluded paths of each collection, checked to exist. Whether a field
/// exists is asked of MongoDB without reading any value.
async fn exclusions(
    db: &mongodb::Database,
    collections: &[String],
    exclude: &[String],
) -> Result<HashMap<String, Vec<String>>, ProfileError> {
    let mut paths: HashMap<String, Vec<String>> = HashMap::new();
    for e in exclude {
        let unknown = || ProfileError::UnknownExclusion(e.clone());
        let (collection, path) = e.split_once('.').ok_or_else(unknown)?;
        if !collections.iter().any(|c| c == collection) {
            return Err(unknown());
        }
        let found = db
            .collection::<Document>(collection)
            .count_documents(doc! { path: { "$exists": true } })
            .limit(1)
            .await?;
        if found == 0 {
            return Err(unknown());
        }
        paths
            .entry(collection.to_string())
            .or_default()
            .push(path.to_string());
    }
    for list in paths.values_mut() {
        list.sort();
        list.dedup();
        // MongoDB refuses a projection of a path and a path under it.
        let all = list.clone();
        list.retain(|p| {
            !all.iter().any(|q| {
                p.strip_prefix(q.as_str())
                    .is_some_and(|r| r.starts_with('.'))
            })
        });
    }
    Ok(paths)
}

/// Values of a field checked against the `_id`s of a collection it may
/// refer to.
const REFERENCE_PROBE: usize = 100;
/// The share of those values that must be `_id`s there.
const REFERENCE_SHARE: f64 = 0.9;

/// The collection `field` refers to, if any: the collection it is named for
/// (`accountId`, `account_id` or `account.id` for `accounts`), else, for an
/// ObjectId field, any collection with ObjectId `_id`s. Nine in ten of the
/// field's first distinct strings, integers and ObjectIds must be `_id`s of
/// that collection.
async fn reference(
    db: &mongodb::Database,
    from: &CollectionSample,
    field: &Field,
    collections: &[CollectionSample],
) -> Result<Option<Relationship>, ProfileError> {
    let probe: Vec<Bson> = field
        .values
        .counts
        .iter()
        .filter_map(|(v, _)| v.key())
        .take(REFERENCE_PROBE)
        .collect();
    if probe.is_empty() {
        return Ok(None);
    }
    let mut targets: Vec<&CollectionSample> = Vec::new();
    let named = named_collection(&field.path, &from.name, collections);
    targets.extend(named);
    if probe.iter().any(|v| matches!(v, Bson::ObjectId(_))) {
        targets.extend(collections.iter().filter(|c| {
            !named.is_some_and(|n| std::ptr::eq(n, *c))
                && c.fields
                    .get("_id")
                    .is_some_and(|id| id.types.has("ObjectId"))
        }));
    }
    for target in targets {
        let matched = db
            .collection::<Document>(&target.name)
            .count_documents(doc! { "_id": { "$in": probe.clone() } })
            .await?;
        if (matched as f64) < REFERENCE_SHARE * probe.len() as f64 {
            continue;
        }
        let join_type = if field.types.has("Array") || field.in_array.is_some() {
            JoinType::ManyToMany
        } else if field.values.all_distinct() {
            JoinType::OneToOne
        } else {
            JoinType::ManyToOne
        };
        let name = format!("{}.{}", from.name, field.path);
        return Ok(Some(Relationship {
            condition: format!("{name} = {}._id", target.name),
            name,
            models: vec![from.name.clone(), target.name.clone()],
            join_type,
            properties: BTreeMap::from([
                ("mqlc.path".to_string(), field.path.clone().into()),
                (
                    "mqlc.matched".to_string(),
                    format!("{matched}/{}", probe.len()).into(),
                ),
            ]),
        }));
    }
    Ok(None)
}

/// The collection a key at `path` is named for: its last segment less an
/// `Id`, `ID` or `_id` suffix (plural too), or the segment before a bare
/// `id`, ending in the collection's name, singular or plural, ignoring case
/// and underscores. The longest such name wins. A key named just for its own
/// collection (`customerId` in `customers`) is a copy of its `_id`, not a
/// reference.
fn named_collection<'a>(
    path: &str,
    from: &str,
    collections: &'a [CollectionSample],
) -> Option<&'a CollectionSample> {
    let mut segments = path.rsplit('.');
    let last = segments.next()?;
    let stem = ["Ids", "IDs", "_ids", "Id", "ID", "_id"]
        .iter()
        .find_map(|suffix| last.strip_suffix(suffix))
        .or((last == "id" || last == "ids").then_some(""))?;
    let stem = match stem {
        "" => segments.next()?,
        stem => stem,
    };
    let normalize = |s: &str| s.to_ascii_lowercase().replace('_', "");
    let stem = normalize(stem);
    collections
        .iter()
        .filter_map(|c| {
            let name = normalize(&c.name);
            let forms = [
                Some(name.clone()),
                name.strip_suffix("ies").map(|s| format!("{s}y")),
                name.strip_suffix("es").map(str::to_string),
                name.strip_suffix('s').map(str::to_string),
            ];
            let forms: Vec<String> = forms.into_iter().flatten().collect();
            if c.name == from && forms.contains(&stem) {
                return None;
            }
            forms
                .into_iter()
                .filter(|f| !f.is_empty() && stem.ends_with(f.as_str()))
                .map(|f| f.len())
                .max()
                .map(|len| (len, c))
        })
        // The first of the longest, in name order.
        .fold(
            None,
            |best: Option<(usize, &CollectionSample)>, (len, c)| match best {
                Some((b, _)) if b >= len => best,
                _ => Some((len, c)),
            },
        )
        .map(|(_, c)| c)
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
        "references names the collection _id a field's values refer to, for \
         $lookup; such a field's values are not listed."
            .to_string(),
        format!(
            "values lists every value a field takes (array elements for an array), \
             most frequent first. A field with more values has common_values instead, \
             up to {TOP_K} of its most frequent that repeat, with min and max for numbers and dates, and a \
             format: a regex at least {}% of its strings match in full.",
            (FORMAT_SHARE * 100.0) as u32
        ),
        format!(
            "Types, presence and values come from up to the first {sample_size} \
             documents of each collection in _id order; other documents may differ."
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
    values: Values,
    /// Embedded documents found at this path (values or array elements):
    /// what `children` were looked for in.
    enclosed: u64,
    children: Fields,
}

impl Fields {
    fn get(&self, key: &str) -> Option<&Field> {
        self.index.get(key).map(|&i| &self.fields[i])
    }

    /// Every field, each followed by its children.
    fn all(&self) -> Vec<&Field> {
        let mut out = Vec::new();
        for f in &self.fields {
            out.push(f);
            out.extend(f.children.all());
        }
        out
    }

    fn visit(&mut self, doc: &Document, prefix: &str, in_array: Option<&str>) {
        for (key, value) in doc {
            let field = self.field(key, prefix, in_array);
            field.present += 1;
            field.types.add(type_name(value));
            field.values.add(value);
            match value {
                Bson::Document(d) => {
                    field.enclosed += 1;
                    field.children.visit(d, &field.path, in_array);
                }
                Bson::Array(elements) => {
                    for e in elements {
                        field.element_types.add(type_name(e));
                        field.values.add(e);
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
                values: Values::default(),
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
            f.values.describe(&mut properties);
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

/// Distinct values kept per field; past this many, new values are counted
/// as values but not kept.
const DISTINCT_CAP: usize = 10_000;
/// A field with at most this many distinct values lists them all.
const ENUM_LIMIT: usize = 20;
/// Longer strings are never listed as values.
const MAX_VALUE_CHARS: usize = 40;
/// Common values listed for a field with too many to list them all: the
/// most frequent of those seen more than once.
const TOP_K: usize = 5;
/// Shapes kept per field; strings of other shapes match no format.
const SHAPE_CAP: usize = 100;
/// Longer strings have no shape.
const MAX_SHAPE_CHARS: usize = 64;
/// The share of a field's values one string shape must cover to be its
/// format.
const FORMAT_SHARE: f64 = 0.9;

/// A scalar value, as a key for counting.
#[derive(Clone, PartialEq, Eq, Hash)]
enum Scalar {
    Str(String),
    Int(i64),
    /// The bits of the `f64`.
    Double(u64),
    Bool(bool),
    ObjectId(ObjectId),
    Date(i64),
}

impl Scalar {
    fn of(value: &Bson) -> Option<Self> {
        Some(match value {
            Bson::String(s) => Self::Str(s.clone()),
            Bson::Int32(i) => Self::Int((*i).into()),
            Bson::Int64(i) => Self::Int(*i),
            Bson::Double(d) => Self::Double(d.to_bits()),
            Bson::Boolean(b) => Self::Bool(*b),
            Bson::ObjectId(id) => Self::ObjectId(*id),
            Bson::DateTime(d) => Self::Date(d.timestamp_millis()),
            _ => return None,
        })
    }

    /// Whether the value may be listed: neither an id nor a date, and short.
    fn listable(&self) -> bool {
        match self {
            Self::Str(s) => s.chars().count() <= MAX_VALUE_CHARS,
            Self::Int(_) | Self::Double(_) | Self::Bool(_) => true,
            Self::ObjectId(_) | Self::Date(_) => false,
        }
    }

    /// The value as BSON, if it may be a key.
    fn key(&self) -> Option<Bson> {
        match self {
            Self::Str(s) => Some(Bson::String(s.clone())),
            Self::Int(i) => Some(Bson::Int64(*i)),
            Self::ObjectId(id) => Some(Bson::ObjectId(*id)),
            Self::Double(_) | Self::Bool(_) | Self::Date(_) => None,
        }
    }

    fn json(&self) -> serde_json::Value {
        match self {
            Self::Str(s) => s.clone().into(),
            Self::Int(i) => (*i).into(),
            Self::Double(bits) => f64::from_bits(*bits).into(),
            Self::Bool(b) => (*b).into(),
            Self::ObjectId(id) => id.to_hex().into(),
            Self::Date(ms) => DateTime::from_millis(*ms)
                .try_to_rfc3339_string()
                .unwrap_or_else(|_| ms.to_string())
                .into(),
        }
    }
}

/// The scalar values seen at a field, with their counts, in the order first
/// seen.
#[derive(Default)]
struct Values {
    counts: Vec<(Scalar, u64)>,
    index: HashMap<Scalar, usize>,
    /// More than `DISTINCT_CAP` distinct values were seen.
    overflow: bool,
    numbers: Range<f64>,
    dates: Range<i64>,
    /// Scalar values seen, distinct or not.
    seen: u64,
    /// String shapes with each run of a character class written as its
    /// length (`\d{4}`), then as any length (`\d+`).
    exact_shapes: ShapeCounts,
    loose_shapes: ShapeCounts,
}

/// The smallest and largest values seen, each with the value itself.
struct Range<T> {
    min: Option<(T, Scalar)>,
    max: Option<(T, Scalar)>,
}

impl<T> Default for Range<T> {
    fn default() -> Self {
        Self {
            min: None,
            max: None,
        }
    }
}

impl<T: PartialOrd + Copy> Range<T> {
    fn add(&mut self, key: T, value: &Scalar) {
        if self.min.as_ref().is_none_or(|(m, _)| key < *m) {
            self.min = Some((key, value.clone()));
        }
        if self.max.as_ref().is_none_or(|(m, _)| key > *m) {
            self.max = Some((key, value.clone()));
        }
    }

    fn describe(&self, properties: &mut BTreeMap<String, String>) -> bool {
        let (Some((_, min)), Some((_, max))) = (&self.min, &self.max) else {
            return false;
        };
        for (k, v) in [("mqlc.min", min), ("mqlc.max", max)] {
            properties.insert(k.to_string(), v.json().to_string());
        }
        true
    }
}

/// Counts of up to `SHAPE_CAP` keys.
#[derive(Default)]
struct ShapeCounts(HashMap<String, u64>);

impl ShapeCounts {
    fn add(&mut self, key: String) {
        if let Some(count) = self.0.get_mut(&key) {
            *count += 1;
        } else if self.0.len() < SHAPE_CAP {
            self.0.insert(key, 1);
        }
    }

    /// The key counted at least `share` of `total` times.
    fn covering(&self, total: u64, share: f64) -> Option<&str> {
        self.0
            .iter()
            .find(|(_, count)| **count as f64 >= share * total as f64)
            .map(|(k, _)| k.as_str())
    }
}

/// `s` as a regex of runs of digits, upper and lower case ASCII letters, and
/// other characters, each run with its length; if `loose`, runs of digits or
/// letters have any length.
fn shape(s: &str, loose: bool) -> String {
    fn class(c: char) -> String {
        match c {
            '0'..='9' => r"\d".to_string(),
            'A'..='Z' => "[A-Z]".to_string(),
            'a'..='z' => "[a-z]".to_string(),
            c if r"\.+*?()|[]{}^$".contains(c) => format!("\\{c}"),
            c => c.to_string(),
        }
    }
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        let class = class(c);
        let mut run = 1;
        while chars.peek().is_some_and(|&next| shape_class_eq(next, c)) {
            chars.next();
            run += 1;
        }
        out.push_str(&class);
        if loose && c.is_ascii_alphanumeric() {
            out.push('+');
        } else if run > 1 {
            out.push_str(&format!("{{{run}}}"));
        }
    }
    out
}

/// Whether two characters fall in the same run of `shape`.
fn shape_class_eq(a: char, b: char) -> bool {
    a.is_ascii_digit() && b.is_ascii_digit()
        || a.is_ascii_uppercase() && b.is_ascii_uppercase()
        || a.is_ascii_lowercase() && b.is_ascii_lowercase()
        || a == b
}

impl Values {
    fn add(&mut self, value: &Bson) {
        let Some(value) = Scalar::of(value) else {
            return;
        };
        self.seen += 1;
        match &value {
            Scalar::Int(i) => self.numbers.add(*i as f64, &value),
            Scalar::Double(bits) => self.numbers.add(f64::from_bits(*bits), &value),
            Scalar::Date(ms) => self.dates.add(*ms, &value),
            Scalar::Str(s) if s.chars().count() <= MAX_SHAPE_CHARS => {
                self.exact_shapes.add(shape(s, false));
                self.loose_shapes.add(shape(s, true));
            }
            _ => {}
        }
        if let Some(&i) = self.index.get(&value) {
            self.counts[i].1 += 1;
        } else if self.counts.len() < DISTINCT_CAP {
            self.index.insert(value.clone(), self.counts.len());
            self.counts.push((value, 1));
        } else {
            self.overflow = true;
        }
    }

    /// Whether no value was seen twice (as far as values were kept).
    fn all_distinct(&self) -> bool {
        !self.overflow && self.counts.iter().all(|(_, count)| *count == 1)
    }

    /// Most frequent first; the sort is stable, so ties stay in seen order.
    fn sorted(&self) -> Vec<&(Scalar, u64)> {
        let mut counts: Vec<_> = self.counts.iter().collect();
        counts.sort_by_key(|c| std::cmp::Reverse(c.1));
        counts
    }

    fn describe(&self, properties: &mut BTreeMap<String, String>) {
        if self.counts.is_empty() {
            return;
        }
        let mut set = |k: &str, v: String| properties.insert(k.to_string(), v);
        set(
            "mqlc.distinct",
            if self.overflow {
                format!(">{DISTINCT_CAP}")
            } else {
                self.counts.len().to_string()
            },
        );
        if !self.overflow
            && self.counts.len() <= ENUM_LIMIT
            && self.counts.iter().all(|(v, _)| v.listable())
        {
            let values: Vec<_> = self.sorted().iter().map(|(v, _)| v.json()).collect();
            set(
                "mqlc.enum",
                serde_json::to_string(&values).expect("values serialize"),
            );
            return;
        }
        let top: Vec<_> = self
            .sorted()
            .into_iter()
            .filter(|(v, count)| v.listable() && *count > 1)
            .take(TOP_K)
            .map(|(v, count)| serde_json::json!([v.json(), count]))
            .collect();
        if !top.is_empty() {
            set(
                "mqlc.topValues",
                serde_json::to_string(&top).expect("values serialize"),
            );
        }
        let format = self
            .exact_shapes
            .covering(self.seen, FORMAT_SHARE)
            .or_else(|| self.loose_shapes.covering(self.seen, FORMAT_SHARE));
        if let Some(format) = format {
            set("mqlc.format", format.to_string());
        }
        if !self.numbers.describe(properties) {
            self.dates.describe(properties);
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
