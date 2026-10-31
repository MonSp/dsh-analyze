use std::collections::HashMap;
use std::path::Path;

use crate::cli::{env_u64, fail, flag_value};
use crate::dataset::{
    json_string, load_dataset, new_dataset_id, save_meta, write_table_parquet_placeholder, Column,
    Meta, Table, Value,
};

pub(crate) fn cmd_join(workspace: &Path, args: &[String]) -> Result<String, u8> {
    let left_id = match flag_value(args, "--left") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --left required"),
    };
    let right_id = match flag_value(args, "--right") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --right required"),
    };
    let on_raw = match flag_value(args, "--on") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --on required"),
    };
    let on: Vec<String> = on_raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if on.is_empty() {
        return fail(2, "analysis/invalid-args: --on must list at least one column");
    }
    let how = flag_value(args, "--how")
        .map(|s| s.to_string())
        .unwrap_or_else(|| "inner".into());
    if how != "inner" && how != "left" && how != "anti" && how != "semi" {
        return fail(2, "analysis/invalid-args: --how must be inner|left|anti|semi");
    }
    let left_only = how == "anti" || how == "semi";
    let label = flag_value(args, "--out").map(|s| s.to_string());
    let max_rows: usize =
        env_u64("DSH_ANALYZE_MAX_ROWS", "INV_ANALYSIS_MAX_ROWS", 5_000_000) as usize;

    let (left_meta, left) = load_dataset(workspace, &left_id).map_err(|e| {
        eprintln!("{e}");
        if e.contains("unknown") {
            5u8
        } else {
            2u8
        }
    })?;
    let (_right_meta, right) = load_dataset(workspace, &right_id).map_err(|e| {
        eprintln!("{e}");
        if e.contains("unknown") {
            5u8
        } else {
            2u8
        }
    })?;
    let _ = left_meta;

    let mut left_key_idx = Vec::with_capacity(on.len());
    let mut right_key_idx = Vec::with_capacity(on.len());
    for col in &on {
        let li = left
            .columns
            .iter()
            .position(|c| &c.name == col)
            .ok_or_else(|| {
                eprintln!("analysis/invalid-args: unknown column {col} on left");
                2u8
            })?;
        let ri = right
            .columns
            .iter()
            .position(|c| &c.name == col)
            .ok_or_else(|| {
                eprintln!("analysis/invalid-args: unknown column {col} on right");
                2u8
            })?;
        left_key_idx.push(li);
        right_key_idx.push(ri);
    }

    // Column layout: left cols + right non-key cols (collision → _r).
    // anti/semi: left columns only.
    let mut out_cols: Vec<Column> = left.columns.clone();
    let mut names: Vec<String> = out_cols.iter().map(|c| c.name.clone()).collect();
    let mut right_payload: Vec<usize> = Vec::new();
    if !left_only {
        for (i, c) in right.columns.iter().enumerate() {
            if right_key_idx.contains(&i) {
                continue;
            }
            let mut name = c.name.clone();
            while names.contains(&name) {
                name = format!("{name}_r");
            }
            names.push(name.clone());
            out_cols.push(Column {
                name,
                dtype: c.dtype.clone(),
            });
            right_payload.push(i);
        }
    }

    // Hash right by key
    let mut right_map: HashMap<Vec<Value>, Vec<usize>> = HashMap::new();
    for (ri, row) in right.rows.iter().enumerate() {
        let key: Vec<Value> = right_key_idx
            .iter()
            .map(|&ci| row.get(ci).cloned().unwrap_or(Value::Null))
            .collect();
        if key.iter().any(|v| matches!(v, Value::Null)) {
            continue;
        }
        right_map.entry(key).or_default().push(ri);
    }

    let mut out_rows: Vec<Vec<Value>> = Vec::new();
    for lrow in &left.rows {
        let key: Vec<Value> = left_key_idx
            .iter()
            .map(|&ci| lrow.get(ci).cloned().unwrap_or(Value::Null))
            .collect();
        let matched = if key.iter().any(|v| matches!(v, Value::Null)) {
            None
        } else {
            right_map.get(&key)
        };
        match matched {
            Some(_) if how == "semi" => {
                out_rows.push(lrow.clone());
                if out_rows.len() > max_rows {
                    return fail(3, "analysis/limit: join result too large");
                }
            }
            Some(_) if how == "anti" => {}
            Some(ris) => {
                for &ri in ris {
                    let mut row = lrow.clone();
                    for &ci in &right_payload {
                        row.push(right.rows[ri].get(ci).cloned().unwrap_or(Value::Null));
                    }
                    out_rows.push(row);
                    if out_rows.len() > max_rows {
                        return fail(3, "analysis/limit: join result too large");
                    }
                }
            }
            None if how == "anti" => {
                out_rows.push(lrow.clone());
                if out_rows.len() > max_rows {
                    return fail(3, "analysis/limit: join result too large");
                }
            }
            None if how == "left" => {
                let mut row = lrow.clone();
                for _ in &right_payload {
                    row.push(Value::Null);
                }
                out_rows.push(row);
                if out_rows.len() > max_rows {
                    return fail(3, "analysis/limit: join result too large");
                }
            }
            None => {}
        }
    }

    let new_id = new_dataset_id();
    let result = Table {
        columns: out_cols,
        rows: out_rows,
    };
    let (bytes, _) = write_table_parquet_placeholder(workspace, &new_id, &result).map_err(|e| {
        eprintln!("{e}");
        1u8
    })?;
    let _ = bytes;
    let meta = Meta {
        dataset_id: new_id.clone(),
        row_count: result.rows.len(),
        columns: result.columns.clone(),
        source_kind: "join".into(),
        source_path: format!("join:{left_id}+{right_id}"),
        label,
        produced_by: "join".into(),
        parent_dataset_id: Some(left_id.clone()),
    };
    save_meta(workspace, &meta).map_err(|e| {
        eprintln!("{e}");
        1u8
    })?;
    Ok(format!(
        "{{\"ok\":true,\"datasetId\":{},\"rowCount\":{},\"columnCount\":{},\"columns\":[{}],\"leftDatasetId\":{},\"rightDatasetId\":{},\"how\":{}}}",
        json_string(&new_id),
        result.rows.len(),
        result.columns.len(),
        meta.columns_json(),
        json_string(&left_id),
        json_string(&right_id),
        json_string(&how)
    ))
}
