//! The static check run on gold pipelines, which are right by definition:
//! everything it reports there is a false positive.

use std::collections::BTreeMap;
use std::fmt;

use super::{CheckKind, Pipeline, Schema};
use crate::{Dataset, DatasetError};

/// Train only: tuning the check on dev would leak (ADR 0006). Gold
/// pipelines that are not valid Extended JSON are reported by `Coverage` and
/// skipped here.
#[derive(Debug, Default)]
pub struct GoldCheck {
    pub samples: usize,
    /// Samples the check reports anything on.
    pub flagged: usize,
    /// Samples per kind of error.
    pub by_kind: BTreeMap<CheckKind, usize>,
    pub by_db: BTreeMap<String, DbCount>,
    /// Every error, in sample order.
    pub errors: Vec<GoldError>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DbCount {
    pub samples: usize,
    pub flagged: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoldError {
    pub sample_id: String,
    pub kind: CheckKind,
    pub message: String,
}

impl GoldCheck {
    /// Every train question, or only those on `db_id`.
    pub fn of(dataset: &Dataset, db_id: Option<&str>) -> Result<Self, DatasetError> {
        let mut report = Self::default();
        let mut schemas: BTreeMap<String, Schema> = BTreeMap::new();
        let samples = dataset
            .samples()
            .iter()
            .filter(|s| s.split == "train")
            .filter(|s| db_id.is_none_or(|db| s.db_id == db));
        for sample in samples {
            let Ok(pipeline) = Pipeline::from_extended_json(&sample.mql.aggregation_pipeline)
            else {
                continue;
            };
            if !schemas.contains_key(&sample.db_id) {
                let fields = dataset.official_field_types(&sample.split, &sample.db_id)?;
                schemas.insert(sample.db_id.clone(), Schema::from_official(&fields));
            }
            let errors = pipeline.check(&sample.mql.collection, &schemas[&sample.db_id]);
            report.samples += 1;
            let db = report.by_db.entry(sample.db_id.clone()).or_default();
            db.samples += 1;
            if errors.is_empty() {
                continue;
            }
            report.flagged += 1;
            db.flagged += 1;
            let mut kinds: Vec<CheckKind> = errors.iter().map(|e| e.kind).collect();
            kinds.sort();
            kinds.dedup();
            for kind in kinds {
                *report.by_kind.entry(kind).or_default() += 1;
            }
            report.errors.extend(errors.into_iter().map(|e| GoldError {
                sample_id: sample.sample_id.clone(),
                kind: e.kind,
                message: e.message,
            }));
        }
        Ok(report)
    }
}

impl fmt::Display for GoldCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let pct = |n: usize, of: usize| 100.0 * n as f64 / of.max(1) as f64;
        writeln!(
            f,
            "Static check on train gold pipelines: {} of {} samples flagged ({:.1}%), all false positives",
            self.flagged,
            self.samples,
            pct(self.flagged, self.samples)
        )?;
        writeln!(f)?;
        writeln!(f, "Samples per kind of error:")?;
        for (kind, n) in &self.by_kind {
            writeln!(f, "  {n:>5}  {}", kind.name())?;
        }
        writeln!(f)?;
        writeln!(f, "{:<26}{:>9}{:>9}", "database", "samples", "flagged")?;
        for (db, count) in &self.by_db {
            writeln!(f, "{db:<26}{:>9}{:>9}", count.samples, count.flagged)?;
        }
        writeln!(f)?;
        writeln!(f, "Errors:")?;
        for e in &self.errors {
            writeln!(f, "  {}  [{}] {}", e.sample_id, e.kind.name(), e.message)?;
        }
        Ok(())
    }
}
