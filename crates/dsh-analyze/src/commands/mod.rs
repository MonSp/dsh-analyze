mod agg;
mod diff;
mod ingest;
mod inspect;
mod join;
mod manage;
mod pivot;
mod quality;
mod relation;
mod sample;
mod series;
mod union;

use std::path::Path;

use crate::dataset::{load_dataset, Meta, Table};

pub(crate) use agg::cmd_agg;
pub(crate) use diff::cmd_diff;
pub(crate) use ingest::cmd_ingest;
pub(crate) use inspect::{cmd_describe, cmd_head, cmd_schema, cmd_values};
pub(crate) use join::cmd_join;
pub(crate) use manage::{cmd_export, cmd_list, cmd_rm, cmd_verify};
pub(crate) use pivot::cmd_pivot;
pub(crate) use quality::{cmd_dedup, cmd_outliers, cmd_profile};
pub(crate) use relation::{cmd_corr, cmd_crosstab};
pub(crate) use sample::cmd_sample;
pub(crate) use series::cmd_series;
pub(crate) use union::cmd_union;

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

/// Parse `--on` entries `pair(,pair)*`, `pair := name | leftName:rightName`.
/// Returns (leftName, rightName) tuples. Validates pair syntax and duplicate
/// key names per side; entry-count limits are enforced by the caller.
pub(crate) fn parse_on_pairs(on_raw: &str) -> Result<Vec<(String, String)>, String> {
    let entries: Vec<&str> = on_raw
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    let mut out = Vec::with_capacity(entries.len());
    let mut seen_left: Vec<String> = Vec::new();
    let mut seen_right: Vec<String> = Vec::new();
    for entry in entries {
        let parts: Vec<&str> = entry.split(':').collect();
        let (ln, rn) = match parts.as_slice() {
            [name] => (*name, *name),
            [l, r] if !l.is_empty() && !r.is_empty() => (*l, *r),
            _ => {
                return Err("analysis/invalid-args: on pairs must be leftKey:rightKey".into())
            }
        };
        if seen_left.iter().any(|s| s == ln) {
            return Err(format!("analysis/invalid-args: duplicate join key {ln}"));
        }
        if seen_right.iter().any(|s| s == rn) {
            return Err(format!("analysis/invalid-args: duplicate join key {rn}"));
        }
        seen_left.push(ln.to_string());
        seen_right.push(rn.to_string());
        out.push((ln.to_string(), rn.to_string()));
    }
    Ok(out)
}

/// `clamp` warning entry shared by head/sample/values/diff (R-20).
pub(crate) fn clamp_warning_json(field: &str, requested: u64, actual: u64) -> String {
    format!(
        "{{\"code\":\"clamp\",\"field\":{},\"requested\":{},\"actual\":{}}}",
        crate::dataset::json_string(field),
        requested,
        actual
    )
}
