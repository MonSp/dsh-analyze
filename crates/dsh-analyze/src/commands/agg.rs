use std::fs;
use std::path::Path;

use crate::cli::{env_u64, fail, flag_value};
use crate::dataset::{
    ensure_within_workspace, json_string, load_dataset, new_dataset_id, save_meta,
    write_table_parquet_placeholder, Meta,
};

pub(crate) fn cmd_agg(workspace: &Path, args: &[String]) -> Result<String, u8> {
    let id = match flag_value(args, "--dataset-id") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --dataset-id required"),
    };
    let spec_raw = match flag_value(args, "--spec") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --spec required"),
    };
    // Spec may be inline JSON object or a workspace-relative file path (interface spec §4).
    let spec_text = if spec_raw.trim_start().starts_with('{') {
        spec_raw
    } else {
        let spec_path = ensure_within_workspace(workspace, Path::new(&spec_raw)).map_err(|e| {
            eprintln!("{e}");
            2u8
        })?;
        fs::read_to_string(&spec_path).map_err(|e| {
            eprintln!("analysis/invalid-args: cannot read spec file: {e}");
            2u8
        })?
    };
    // Windows tools may write a UTF-8 BOM; trim() does not strip U+FEFF.
    let spec_text = spec_text.strip_prefix('\u{feff}').unwrap_or(&spec_text);
    let out = flag_value(args, "--out").map(|s| s.to_string());
    let default_limit: usize = env_u64(
        "DSH_ANALYZE_DEFAULT_ROW_LIMIT",
        "INV_ANALYSIS_DEFAULT_ROW_LIMIT",
        1000,
    ) as usize;
    let (_meta, table) = load_dataset(workspace, &id).map_err(|e| {
        eprintln!("{e}");
        if e.contains("unknown") {
            5u8
        } else {
            2u8
        }
    })?;
    let spec = crate::agg::parse_spec_json(&spec_text).map_err(|e| {
        eprintln!("{e}");
        2u8
    })?;
    let outcome = crate::agg::run_agg(&table, &spec, default_limit, 5000).map_err(|e| {
        eprintln!("{e}");
        2u8
    })?;
    let result = outcome.table;

    let mut out_dataset_id = String::new();
    if let Some(out_label) = out {
        let new_id = new_dataset_id();
        let (bytes, _) = write_table_parquet_placeholder(workspace, &new_id, &result).map_err(|e| {
            eprintln!("{e}");
            1u8
        })?;
        let _ = bytes;
        let meta = Meta {
            dataset_id: new_id.clone(),
            row_count: result.rows.len(),
            columns: result.columns.clone(),
            source_kind: "csv".into(),
            source_path: format!("agg:{id}"),
            label: Some(out_label),
            produced_by: "agg".into(),
            parent_dataset_id: Some(id.clone()),
        };
        save_meta(workspace, &meta).map_err(|e| {
            eprintln!("{e}");
            1u8
        })?;
        out_dataset_id = new_id;
    }

    let budget_rows = 20;
    let include_rows = out_dataset_id.is_empty() && result.rows.len() <= budget_rows;
    let truncated =
        !include_rows && (out_dataset_id.is_empty() || result.rows.len() > budget_rows);
    let cols: Vec<String> = result.columns.iter().map(|c| c.name.clone()).collect();
    let rows_json = if include_rows {
        result
            .rows
            .iter()
            .map(|row| {
                format!(
                    "[{}]",
                    row.iter().map(|v| v.to_json()).collect::<Vec<_>>().join(",")
                )
            })
            .collect::<Vec<_>>()
            .join(",")
    } else {
        String::new()
    };

    let parse_field = if outcome.parse_fail_count > 0 {
        format!(",\"parseFailCount\":{}", outcome.parse_fail_count)
    } else {
        String::new()
    };

    Ok(format!(
        "{{\"ok\":true,\"datasetId\":{},\"outDatasetId\":{},\"rowCount\":{},\"columns\":[{}],\"rows\":[{}],\"truncated\":{}{parse_field}}}",
        json_string(&id),
        if out_dataset_id.is_empty() {
            "null".into()
        } else {
            json_string(&out_dataset_id)
        },
        result.rows.len(),
        cols.iter().map(|c| json_string(c)).collect::<Vec<_>>().join(","),
        rows_json,
        if truncated { "true" } else { "false" }
    ))
}
