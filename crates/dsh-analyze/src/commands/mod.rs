mod agg;
mod ingest;
mod inspect;
mod join;
mod manage;
mod quality;
mod relation;
mod sample;
mod series;

use std::path::Path;

use crate::dataset::{load_dataset, Meta, Table};

pub(crate) use agg::cmd_agg;
pub(crate) use ingest::cmd_ingest;
pub(crate) use inspect::{cmd_describe, cmd_head, cmd_schema, cmd_values};
pub(crate) use join::cmd_join;
pub(crate) use manage::{cmd_export, cmd_list, cmd_rm};
pub(crate) use quality::{cmd_dedup, cmd_outliers, cmd_profile};
pub(crate) use relation::{cmd_corr, cmd_crosstab};
pub(crate) use sample::cmd_sample;
pub(crate) use series::cmd_series;

/// Load a dataset or exit with the shared CLI error code (unknown id → 5).
pub(crate) fn load_or_fail(workspace: &Path, id: &str) -> Result<(Meta, Table), u8> {
    load_dataset(workspace, id).map_err(|e| {
        eprintln!("{e}");
        if e.contains("unknown") {
            5u8
        } else {
            2u8
        }
    })
}

/// Split a comma-separated column list; empty segments are dropped.
pub(crate) fn parse_column_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Finite f64 → plain JSON number; non-finite → null.
pub(crate) fn json_f64(v: f64) -> String {
    if v.is_finite() {
        format!("{v}")
    } else {
        "null".into()
    }
}
