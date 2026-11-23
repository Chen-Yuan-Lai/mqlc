//! The static check run on gold pipelines, which are right by definition:
//! everything it reports there is a false positive.

use std::collections::BTreeMap;
use std::fmt;

use super::{CheckKind, Pipeline, Schema};
use crate::{Dataset, DatasetError, Sample};

/// Errors that a gold pipeline is not valid Extended JSON are reported by
/// `Coverage`; such samples are skipped here.
#[derive(Debug, Default)]
pub struct GoldCheck {
    pub samples: usize,
    /// Samples the check reports anything on.
    pub flagged: usize,
    /// Samples per kind of error.
    pub by_kind: BTreeMap<CheckKind, usize>,
    /// Samples per database.
    pub by_db: BTreeMap<String, (usize, usize)>,
    /// Every error, by sample id.
    pub errors: Vec<(String, String)>,
}

impl GoldCheck {
    pub fn of<'a>(
        dataset: &Dataset,
        samples: impl IntoIterator<Item = &'a Sample>,
    ) -> Result<Self, DatasetError> {
        let mut report = Self::default();
        let mut schemas: BTreeMap<String, Schema> = BTreeMap::new();
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
            db.0 += 1;
            if errors.is_empty() {
                continue;
            }
            report.flagged += 1;
            db.1 += 1;
            let mut kinds: Vec<CheckKind> = errors.iter().map(|e| e.kind).collect();
            kinds.sort();
            kinds.dedup();
            for kind in kinds {
                *report.by_kind.entry(kind).or_default() += 1;
            }
            report.errors.extend(
                errors
                    .into_iter()
                    .map(|e| (sample.sample_id.clone(), format!("[{}] {e}", e.kind.name()))),
            );
        }
        Ok(report)
    }
}

impl fmt::Display for GoldCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let pct = |n: usize, of: usize| 100.0 * n as f64 / of.max(1) as f64;
        writeln!(
            f,
            "Static check on gold pipelines: {} of {} samples flagged ({:.1}%), all false positives",
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
        for (db, (n, flagged)) in &self.by_db {
            writeln!(f, "{db:<26}{n:>9}{flagged:>9}")?;
        }
        writeln!(f)?;
        writeln!(f, "Errors:")?;
        for (sample_id, error) in &self.errors {
            writeln!(f, "  {sample_id}  {error}")?;
        }
        Ok(())
    }
}
