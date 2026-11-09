//! Wide-table pivot: rows = index combos, columns = distinct pivot values (R-16b).

use std::collections::HashMap;
use std::path::Path;

use crate::cli::{env_u64, fail, flag_value};
use crate::commands::{load_or_fail, parse_column_list};
use crate::dataset::{
    infer_dtype, json_string, new_dataset_id, save_meta, write_table_parquet_placeholder, Column,
    Meta, Table, Value,
};

const MAX_PIVOT_COLUMNS: usize = 50;

pub(crate) fn cmd_pivot(workspace: &Path, args: &[String]) -> Result<String, u8> {
    let id = match flag_value(args, "--dataset-id") {
        Some(v) => v.to_string(),
        None => return pivot_required(),
    };
    let index_raw = match flag_value(args, "--index") {
        Some(v) => v.to_string(),
        None => return pivot_required(),
    };
    let columns_name = match flag_value(args, "--columns") {
        Some(v) => v.trim().to_string(),
        None => return pivot_required(),
    };
    let values_name = match flag_value(args, "--values") {
        Some(v) => v.trim().to_string(),
        None => return pivot_required(),
    };
    let agg = match flag_value(args, "--agg") {
        Some(v) => v.to_string(),
        None => return pivot_required(),
    };
    if !matches!(agg.as_str(), "sum" | "mean" | "min" | "max" | "count") {
        return fail(2, "analysis/invalid-args: --agg must be sum|mean|min|max|count");
    }
    let label = flag_value(args, "--out").map(|s| s.to_string());
    let max_rows: usize =
        env_u64("DSH_ANALYZE_MAX_ROWS", "INV_ANALYSIS_MAX_ROWS", 5_000_000) as usize;

    let (_meta, table) = load_or_fail(workspace, &id)?;

    let index_names = parse_column_list(&index_raw);
    if index_names.is_empty() || index_names.len() > 16 {
        return fail(2, "analysis/invalid-args: --index must list 1..16 columns");
    }
    let index_idx: Vec<usize> = index_names
        .iter()
        .map(|name| {
            table
                .columns
                .iter()
                .position(|c| c.name == *name)
                .ok_or_else(|| {
                    eprintln!("analysis/invalid-args: unknown column {name}");
                    2u8
                })
        })
        .collect::<Result<_, u8>>()?;
    let pcol = table
        .columns
        .iter()
        .position(|c| c.name == columns_name)
        .ok_or_else(|| {
            eprintln!("analysis/invalid-args: unknown column {columns_name}");
            2u8
        })?;
    let vcol = table
        .columns
        .iter()
        .position(|c| c.name == values_name)
        .ok_or_else(|| {
            eprintln!("analysis/invalid-args: unknown column {values_name}");
            2u8
        })?;

    // Distinct pivot values in first-seen order; >50 -> analysis/limit.
    let mut pivot_keys: Vec<String> = Vec::new();
    let mut pivot_names: Vec<String> = Vec::new();
    for row in &table.rows {
        let v = row.get(pcol).cloned().unwrap_or(Value::Null);
        let k = v.category_key().to_json();
        if !pivot_keys.contains(&k) {
            if pivot_keys.len() >= MAX_PIVOT_COLUMNS {
                return fail(3, "analysis/limit: pivot columns exceed 50");
            }
            pivot_keys.push(k);
            pivot_names.push(pivot_value_name(&v));
        }
    }

    // Group rows by index combo, keeping input order.
    let mut row_keys: Vec<Vec<String>> = Vec::new();
    let mut groups: HashMap<Vec<String>, Vec<usize>> = HashMap::new();
    for (ri, row) in table.rows.iter().enumerate() {
        let rk: Vec<String> = index_idx
            .iter()
            .map(|&ci| row.get(ci).cloned().unwrap_or(Value::Null).category_key().to_json())
            .collect();
        groups.entry(rk.clone()).or_insert_with(|| {
            row_keys.push(rk.clone());
            Vec::new()
        });
        groups.get_mut(&rk).unwrap().push(ri);
    }

    let mut out_cols: Vec<Column> = index_names
        .iter()
        .map(|n| Column {
            name: n.clone(),
            dtype: "utf8".into(),
        })
        .collect();
    for name in &pivot_names {
        out_cols.push(Column {
            name: name.clone(),
            dtype: "utf8".into(),
        });
    }

    let mut out_rows: Vec<Vec<Value>> = Vec::with_capacity(row_keys.len());
    for rk in &row_keys {
        let members = &groups[rk];
        // Index cell values from the first member row.
        let first = &table.rows[members[0]];
        let mut row: Vec<Value> = index_idx
            .iter()
            .map(|&ci| first.get(ci).cloned().unwrap_or(Value::Null))
            .collect();
        for pk in &pivot_keys {
            let mut nums: Vec<f64> = Vec::new();
            let mut count: usize = 0;
            for &ri in members {
                let pv = table.rows[ri].get(pcol).cloned().unwrap_or(Value::Null);
                if pv.category_key().to_json() != *pk {
                    continue;
                }
                count += 1;
                let vv = table.rows[ri].get(vcol).cloned().unwrap_or(Value::Null);
                if let Some(n) = vv.as_f64() {
                    if n.is_finite() {
                        nums.push(n);
                    }
                }
            }
            let cell = if count == 0 {
                Value::Null
            } else {
                match agg.as_str() {
                    "count" => Value::I64(count as i64),
                    "sum" if !nums.is_empty() => Value::F64(nums.iter().sum()),
                    "mean" if !nums.is_empty() => {
                        Value::F64(nums.iter().sum::<f64>() / nums.len() as f64)
                    }
                    "min" if !nums.is_empty() => {
                        Value::F64(nums.iter().cloned().fold(f64::INFINITY, f64::min))
                    }
                    "max" if !nums.is_empty() => {
                        Value::F64(nums.iter().cloned().fold(f64::NEG_INFINITY, f64::max))
                    }
                    _ => Value::Null,
                }
            };
            row.push(cell);
        }
        out_rows.push(row);
    }

    if out_rows.len() > max_rows {
        return fail(3, "analysis/limit: pivot result too large");
    }

    for (ci, col) in out_cols.iter_mut().enumerate() {
        let vals: Vec<Value> = out_rows.iter().filter_map(|r| r.get(ci).cloned()).collect();
        col.dtype = infer_dtype(&vals);
    }
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
        source_kind: "pivot".into(),
        source_path: format!("pivot:{id}"),
        label,
        produced_by: "pivot".into(),
        parent_dataset_id: Some(id.clone()),
        parent_dataset_ids: vec![id.clone()],
        source_checksum: None,
    };
    save_meta(workspace, &meta).map_err(|e| {
        eprintln!("{e}");
        1u8
    })?;
    Ok(format!(
        "{{\"ok\":true,\"datasetId\":{},\"rowCount\":{},\"columns\":[{}],\"indexColumns\":[{}],\"pivotColumn\":{}}}",
        json_string(&new_id),
        result.rows.len(),
        columns_json,
        index_names
            .iter()
            .map(|c| json_string(c))
            .collect::<Vec<_>>()
            .join(","),
        json_string(&columns_name)
    ))
}

fn pivot_required() -> Result<String, u8> {
    fail(
        2,
        "analysis/invalid-args: --dataset-id --index --columns --values --agg required",
    )
}

/// Column label for a pivot value: JS `String(v)` style (Str unquoted).
fn pivot_value_name(v: &Value) -> String {
    match v.category_key() {
        Value::Null => "null".into(),
        Value::Bool(b) => if b { "true" } else { "false" }.into(),
        Value::I64(i) => i.to_string(),
        Value::F64(f) => format!("{f}"),
        Value::Str(s) => s,
    }
}
