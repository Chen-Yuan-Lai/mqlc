//! IR coverage: per split, the share of samples whose gold pipeline needs no
//! raw stage, and what the uncovered ones need.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use super::{IrError, Pipeline, Unsupported};
use crate::Sample;

#[derive(Debug, thiserror::Error)]
#[error("{sample_id}: {source}")]
pub struct CoverageError {
    pub sample_id: String,
    pub source: IrError,
}

#[derive(Debug)]
pub struct Coverage {
    pub splits: BTreeMap<String, SplitCoverage>,
}

#[derive(Debug, Default)]
pub struct SplitCoverage {
    pub samples: usize,
    /// Samples whose gold pipeline needs no raw stage.
    pub covered: usize,
    /// For each unsupported stage, operator or form, how many samples need it.
    pub unsupported: BTreeMap<Unsupported, usize>,
}

impl SplitCoverage {
    pub fn rate(&self) -> f64 {
        self.covered as f64 / self.samples as f64
    }
}

impl Coverage {
    pub fn of<'a>(samples: impl IntoIterator<Item = &'a Sample>) -> Result<Self, CoverageError> {
        let mut splits: BTreeMap<String, SplitCoverage> = BTreeMap::new();
        for sample in samples {
            let pipeline = Pipeline::from_extended_json(&sample.mql.aggregation_pipeline).map_err(
                |source| CoverageError {
                    sample_id: sample.sample_id.clone(),
                    source,
                },
            )?;
            let split = splits.entry(sample.split.clone()).or_default();
            split.samples += 1;
            let needs: BTreeSet<Unsupported> = pipeline
                .raw_stages()
                .iter()
                .flat_map(|r| r.unsupported())
                .collect();
            if needs.is_empty() {
                split.covered += 1;
            }
            for construct in needs {
                *split.unsupported.entry(construct).or_default() += 1;
            }
        }
        Ok(Self { splits })
    }
}

impl fmt::Display for Coverage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "IR coverage: samples whose gold pipeline needs no raw stage"
        )?;
        writeln!(f)?;
        writeln!(
            f,
            "{:<8}{:>9}{:>9}{:>10}",
            "split", "samples", "covered", "coverage"
        )?;
        for (name, s) in &self.splits {
            writeln!(
                f,
                "{name:<8}{:>9}{:>9}{:>9.1}%",
                s.samples,
                s.covered,
                100.0 * s.rate()
            )?;
        }
        for (name, s) in &self.splits {
            writeln!(f)?;
            writeln!(
                f,
                "Unsupported in {name}, most needed first (samples needing each):"
            )?;
            let mut top: Vec<_> = s.unsupported.iter().collect();
            top.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
            if top.is_empty() {
                writeln!(f, "  none")?;
            }
            for (construct, n) in top {
                writeln!(f, "  {n:>5}  {construct}")?;
            }
        }
        Ok(())
    }
}
