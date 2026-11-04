//! Row-wise union (append) of two datasets with column alignment (R-8a).

use std::path::Path;

use crate::cli::{env_u64, fail, flag_value};
use crate::commands::load_or_fail;
use crate::dataset::{
    infer_dtype, json_string, new_dataset_id, save_meta, write_table_parquet_placeholder, Column,
    Meta, Table, Value,
};

pub(crate) fn cmd_union(workspace: &Path, args: &[String]) -> Result<String, u8> {
    let left_id = match flag_value(args, "--left") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --left required"),
    };
    let right_id = match flag_value(args, "--right") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --right required"),
    };
    let policy = flag_value(args, "--columns-policy")
        .map(|s| s.to_string())
        .unwrap_or_else(|| "strict".into());
    if policy != "strict" && policy != "intersect" {
        return fail(
            2,
            "analysis/invalid-args: --columns-policy must be strict|intersect",
        );
    }
    let label = flag_value(args, "--out").map(|s| s.to_string());
    let max_rows: usize =
        env_u64("DSH_ANALYZE_MAX_ROWS", "INV_ANALYSIS_MAX_ROWS", 5_000_000) as usize;

    let (_left_meta, left) = load_or_fail(workspace, &left_id)?;
    let (_right_meta, right) = load_or_fail(workspace, &right_id)?;

    let left_names: Vec<String> = left.columns.iter().map(|c| c.name.clone()).collect();
    let right_names: Vec<String> = right.columns.iter().map(|c| c.name.clone()).collect();

    let out_names: Vec<String> = if policy == "strict" {
        let missing_right: Vec<&String> = left_names
            .iter()
            .filter(|n| !right_names.contains(n))
            .collect();
        let missing_left: Vec<&String> = right_names
            .iter()
            .filter(|n| !left_names.contains(n))
            .collect();
        if !missing_right.is_empty() || !missing_left.is_empty() {
            let mut bits: Vec<String> = Vec::new();
            for n in &missing_right {
                bits.push(format!("left-only {n}"));
            }
            for n in &missing_left {
                bits.push(format!("right-only {n}"));
            }
            return fail(
                2,
                &format!("analysis/invalid-args: union column mismatch: {}", bits.join(", ")),
            );
        }
        left_names.clone()
    } else {
        let out: Vec<String> = left_names
            .iter()
            .filter(|n| right_names.contains(n))
            .cloned()
            .collect();
        if out.is_empty() {
            return fail(
                2,
                "analysis/invalid-args: union column mismatch: no shared columns",
            );
        }
        out
    };

    let project = |table: &Table| -> Vec<Vec<Value>> {
        let idxs: Vec<Option<usize>> = out_names
            .iter()
            .map(|n| table.columns.iter().position(|c| &c.name == n))
            .collect();
        table
            .rows
            .iter()
            .map(|row| {
                idxs.iter()
                    .map(|oi| match oi {
                        Some(i) => row.get(*i).cloned().unwrap_or(Value::Null),
                        None => Value::Null,
                    })
                    .collect()
            })
            .collect()
    };
    let l_rows = project(&left);
    let r_rows = project(&right);
    let mut out_rows = l_rows.clone();
    out_rows.extend(r_rows.iter().cloned());
    if out_rows.len() > max_rows {
        return fail(3, "analysis/limit: union result too large");
    }

    // Column dtypes follow the existing mixed-dtype rules (utf8 wins).
    let out_cols: Vec<Column> = out_names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let vals: Vec<Value> = out_rows.iter().filter_map(|r| r.get(i).cloned()).collect();
            Column {
                name: name.clone(),
                dtype: infer_dtype(&vals),
            }
        })
        .collect();
    let columns_json = out_cols
        .iter()
        .map(|c| format!("{{\"name\":{},\"dtype\":{}}}", json_string(&c.name), json_string(&c.dtype)))
        .collect::<Vec<_>>()
        .join(",");

    let new_id = new_dataset_id();
    let result = Table {
        columns: out_cols,
        rows: out_rows,
    };
    write_table_parquet_placeholder(workspace, &new_id, &result).map_err(|e| {
        eprintln!("{e}");
        1u8
    })?;
    let meta = Meta {
        dataset_id: new_id.clone(),
        row_count: result.rows.len(),
        columns: result.columns.clone(),
        source_kind: "union".into(),
        source_path: format!("union:{left_id}+{right_id}"),
        label,
        produced_by: "union".into(),
        parent_dataset_id: Some(left_id.clone()),
    };
    save_meta(workspace, &meta).map_err(|e| {
        eprintln!("{e}");
        1u8
    })?;
    Ok(format!(
        "{{\"ok\":true,\"datasetId\":{},\"rowCount\":{},\"leftRows\":{},\"rightRows\":{},\"columns\":[{}]}}",
        json_string(&new_id),
        result.rows.len(),
        l_rows.len(),
        r_rows.len(),
        columns_json
    ))
}
