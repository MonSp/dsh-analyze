//! Time-series transforms: lag / lead / diff / pct_change / rolling (v0.1.5).

use std::fs;
use std::path::Path;

use crate::cli::{env_u64, fail, flag_value};
use crate::commands::load_or_fail;
use crate::dataset::{
    ensure_within_workspace, json_string, new_dataset_id, save_meta, write_table_parquet_placeholder,
    Column, Meta, Table, Value,
};

const MAX_OPS: usize = 8;
const MAX_PERIODS: usize = 10_000;
const MAX_WINDOW: usize = 10_000;

#[derive(Debug, Clone)]
enum SeriesFn {
    Lag,
    Lead,
    Diff,
    PctChange,
    Rolling { window: usize, agg: String },
}

#[derive(Debug, Clone)]
struct SeriesOp {
    r#fn: SeriesFn,
    column: String,
    as_name: String,
    periods: usize,
}

fn is_valid_as(name: &str) -> bool {
    let bytes = name.as_bytes();
    if name.is_empty() || name.len() > 64 {
        return false;
    }
    let first = bytes[0];
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

fn extract_str(obj: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let idx = obj.find(&needle)?;
    let after = obj[idx + needle.len()..].trim_start();
    if !after.starts_with(':') {
        return None;
    }
    let after = after[1..].trim_start();
    if !after.starts_with('"') {
        return None;
    }
    let mut out = String::new();
    let mut chars = after[1..].chars();
    while let Some(c) = chars.next() {
        if c == '"' {
            break;
        }
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    Some(out)
}

fn extract_usize(obj: &str, key: &str) -> Result<Option<usize>, String> {
    let needle = format!("\"{key}\"");
    let Some(idx) = obj.find(&needle) else {
        return Ok(None);
    };
    let after = obj[idx + needle.len()..].trim_start();
    if !after.starts_with(':') {
        return Ok(None);
    }
    let after = after[1..].trim_start();
    let end = after
        .find([',', '}', ']'])
        .unwrap_or(after.len());
    after[..end].trim().parse::<usize>().map(Some).map_err(|_| {
        format!("analysis/invalid-args: {key} must be a non-negative integer")
    })
}

fn has_unknown_keys(obj: &str, allowed: &[&str]) -> bool {
    // Cheap check: any "key": not in allowed set → reject. Best-effort for agent errors.
    let mut rest = obj;
    while let Some(q) = rest.find('"') {
        let after = &rest[q + 1..];
        let Some(end) = after.find('"') else { break };
        let key = &after[..end];
        let after_key = &after[end + 1..];
        if after_key.trim_start().starts_with(':') && !allowed.contains(&key) {
            return true;
        }
        rest = &after[end + 1..];
    }
    false
}

fn split_objects(arr: &str) -> Result<Vec<String>, String> {
    let inner = arr
        .trim()
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| "analysis/invalid-args: --ops must be a JSON array".to_string())?;
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut cur = String::new();
    let mut in_str = false;
    let mut esc = false;
    for ch in inner.chars() {
        if in_str {
            cur.push(ch);
            if esc {
                esc = false;
            } else if ch == '\\' {
                esc = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            '"' => {
                in_str = true;
                cur.push(ch);
            }
            '{' => {
                depth += 1;
                cur.push(ch);
            }
            '}' => {
                if depth == 0 {
                    return Err("analysis/invalid-args: bad ops array".into());
                }
                depth -= 1;
                cur.push(ch);
                if depth == 0 {
                    out.push(cur.trim().to_string());
                    cur.clear();
                }
            }
            ',' if depth == 0 => {}
            c if c.is_whitespace() && depth == 0 => {}
            c => {
                if depth == 0 {
                    return Err("analysis/invalid-args: ops array must contain objects".into());
                }
                cur.push(c);
            }
        }
    }
    if depth != 0 || !cur.trim().is_empty() {
        return Err("analysis/invalid-args: bad ops array".into());
    }
    Ok(out)
}

fn parse_ops(text: &str) -> Result<Vec<SeriesOp>, String> {
    let text = text.trim();
    let text = text.strip_prefix('\u{feff}').unwrap_or(text).trim();
    let objs = split_objects(text)?;
    if objs.is_empty() {
        return Err("analysis/invalid-args: ops must list at least one transform".into());
    }
    if objs.len() > MAX_OPS {
        return Err(format!(
            "analysis/invalid-args: ops must be 1..={MAX_OPS} transforms"
        ));
    }
    let mut ops = Vec::with_capacity(objs.len());
    let mut names: Vec<String> = Vec::new();
    for obj in &objs {
        if !obj.trim_start().starts_with('{') {
            return Err("analysis/invalid-args: each op must be a JSON object".into());
        }
        let fn_name = extract_str(obj, "fn")
            .ok_or_else(|| "analysis/invalid-args: op requires fn".to_string())?;
        let column = extract_str(obj, "column")
            .ok_or_else(|| "analysis/invalid-args: op requires column".to_string())?;
        let as_name = extract_str(obj, "as")
            .ok_or_else(|| "analysis/invalid-args: op requires as".to_string())?;
        if column.trim().is_empty() {
            return Err("analysis/invalid-args: column must be non-empty".into());
        }
        if !is_valid_as(&as_name) {
            return Err(format!(
                "analysis/invalid-args: as must match [A-Za-z_][A-Za-z0-9_]* (<=64), got {as_name}"
            ));
        }
        if names.contains(&as_name) {
            return Err(format!(
                "analysis/invalid-args: duplicate as column {as_name}"
            ));
        }
        names.push(as_name.clone());

        let periods = extract_usize(obj, "periods")?;
        match fn_name.as_str() {
            "lag" | "lead" | "diff" | "pct_change" => {
                if has_unknown_keys(obj, &["fn", "column", "as", "periods"]) {
                    return Err(format!(
                        "analysis/invalid-args: unknown field in op fn={fn_name}"
                    ));
                }
                if obj.contains("\"window\"") || obj.contains("\"agg\"") {
                    return Err(format!(
                        "analysis/invalid-args: fn {fn_name} does not take window/agg"
                    ));
                }
                let p = periods.unwrap_or(1);
                if !(1..=MAX_PERIODS).contains(&p) {
                    return Err(format!(
                        "analysis/invalid-args: periods must be 1..={MAX_PERIODS}"
                    ));
                }
                ops.push(SeriesOp {
                    r#fn: match fn_name.as_str() {
                        "lag" => SeriesFn::Lag,
                        "lead" => SeriesFn::Lead,
                        "diff" => SeriesFn::Diff,
                        "pct_change" => SeriesFn::PctChange,
                        _ => unreachable!(),
                    },
                    column,
                    as_name,
                    periods: p,
                });
            }
            "rolling" => {
                if has_unknown_keys(obj, &["fn", "column", "as", "window", "agg"]) {
                    return Err(
                        "analysis/invalid-args: unknown field in op fn=rolling".into(),
                    );
                }
                if periods.is_some() {
                    return Err("analysis/invalid-args: rolling takes window, not periods".into());
                }
                let window = extract_usize(obj, "window")?.ok_or_else(|| {
                    "analysis/invalid-args: rolling requires window".to_string()
                })?;
                if !(1..=MAX_WINDOW).contains(&window) {
                    return Err(format!(
                        "analysis/invalid-args: window must be 1..={MAX_WINDOW}"
                    ));
                }
                let agg = extract_str(obj, "agg")
                    .ok_or_else(|| "analysis/invalid-args: rolling requires agg".to_string())?;
                if !matches!(agg.as_str(), "mean" | "sum" | "min" | "max") {
                    return Err(
                        "analysis/invalid-args: rolling agg must be mean|sum|min|max".into(),
                    );
                }
                ops.push(SeriesOp {
                    r#fn: SeriesFn::Rolling { window, agg },
                    column,
                    as_name,
                    periods: 1,
                });
            }
            other => {
                return Err(format!(
                    "analysis/invalid-args: unknown fn {other}; expected lag|lead|diff|pct_change|rolling"
                ))
            }
        }
    }
    Ok(ops)
}

fn compare_keys(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Greater,
        (_, Value::Null) => Ordering::Less,
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Bool(_), _) => Ordering::Less,
        (_, Value::Bool(_)) => Ordering::Greater,
        (Value::I64(_) | Value::F64(_), Value::I64(_) | Value::F64(_)) => {
            let ax = match a {
                Value::I64(v) => *v as f64,
                Value::F64(v) => *v,
                _ => 0.0,
            };
            let bx = match b {
                Value::I64(v) => *v as f64,
                Value::F64(v) => *v,
                _ => 0.0,
            };
            ax.partial_cmp(&bx).unwrap_or(Ordering::Equal)
        }
        (Value::Str(x), Value::Str(y)) => x.cmp(y),
        _ => Ordering::Equal,
    }
}

fn finite_num(v: &Value) -> Option<f64> {
    match v {
        Value::I64(n) => Some(*n as f64),
        Value::F64(n) if n.is_finite() => Some(*n),
        Value::Str(s) => s.trim().parse::<f64>().ok().filter(|n| n.is_finite()),
        _ => None,
    }
}

fn numeric_result(agg_or_diff: &str, all_i64: bool, raw: f64) -> Value {
    if agg_or_diff == "pct" || agg_or_diff == "mean" || agg_or_diff == "sum" {
        return Value::F64(raw);
    }
    // min / max / diff
    if all_i64 && raw.fract() == 0.0 && raw.is_finite() && raw.abs() < i64::MAX as f64 {
        Value::I64(raw as i64)
    } else {
        Value::F64(raw)
    }
}

pub(crate) fn cmd_series(workspace: &Path, args: &[String]) -> Result<String, u8> {
    let id = match flag_value(args, "--dataset-id") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --dataset-id required"),
    };
    let order_by = match flag_value(args, "--order-by") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --order-by required"),
    };
    let ops_raw = match flag_value(args, "--ops") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --ops required"),
    };
    let label = flag_value(args, "--out").map(|s| s.to_string());
    let max_rows: usize =
        env_u64("DSH_ANALYZE_MAX_ROWS", "INV_ANALYSIS_MAX_ROWS", 5_000_000) as usize;

    let ops_text = {
        let t = ops_raw.trim_start();
        if t.starts_with('[') {
            ops_raw
        } else {
            let path = ensure_within_workspace(workspace, Path::new(&ops_raw)).map_err(|e| {
                eprintln!("{e}");
                2u8
            })?;
            fs::read_to_string(&path).map_err(|e| {
                eprintln!("analysis/invalid-args: cannot read ops file: {e}");
                2u8
            })?
        }
    };
    let ops = parse_ops(&ops_text).map_err(|e| {
        eprintln!("{e}");
        2u8
    })?;

    let (meta, table) = load_or_fail(workspace, &id)?;
    let order_idx = table
        .columns
        .iter()
        .position(|c| c.name == order_by)
        .ok_or_else(|| {
            eprintln!("analysis/invalid-args: unknown order-by column {order_by}");
            2u8
        })?;

    let mut col_idxs = Vec::with_capacity(ops.len());
    let mut as_names: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();
    for op in &ops {
        let ci = table
            .columns
            .iter()
            .position(|c| c.name == op.column)
            .ok_or_else(|| {
                eprintln!("analysis/invalid-args: unknown column {}", op.column);
                2u8
            })?;
        if as_names.contains(&op.as_name) {
            eprintln!(
                "analysis/invalid-args: as column collides: {}",
                op.as_name
            );
            return Err(2u8);
        }
        as_names.push(op.as_name.clone());
        col_idxs.push(ci);
    }

    // Stable sort by order-by; nulls last.
    let mut order: Vec<usize> = (0..table.rows.len()).collect();
    order.sort_by(|&i, &j| {
        let a = table.rows[i]
            .get(order_idx)
            .cloned()
            .unwrap_or(Value::Null);
        let b = table.rows[j]
            .get(order_idx)
            .cloned()
            .unwrap_or(Value::Null);
        compare_keys(&a, &b).then(i.cmp(&j))
    });

    let n = order.len();
    let mut out_cols: Vec<Column> = table.columns.clone();
    for op in &ops {
        out_cols.push(Column {
            name: op.as_name.clone(),
            dtype: "f64".into(),
        });
    }

    // Build sorted value matrix for source columns used by ops.
    let mut sorted_rows: Vec<Vec<Value>> = Vec::with_capacity(n);
    for &ri in &order {
        sorted_rows.push(table.rows[ri].clone());
    }

    // Track whether a numeric source column is entirely I64 (for min/max dtype).
    let mut col_all_i64: Vec<bool> = table.columns.iter().map(|_| true).collect();
    for row in &table.rows {
        for (ci, cell) in row.iter().enumerate() {
            match cell {
                Value::Null => {}
                Value::I64(_) => {}
                _ => {
                    col_all_i64[ci] = false;
                }
            }
        }
    }

    let mut new_cols: Vec<Vec<Value>> = Vec::with_capacity(ops.len());
    for (op, &ci) in ops.iter().zip(&col_idxs) {
        let mut col: Vec<Value> = Vec::with_capacity(n);
        match &op.r#fn {
            SeriesFn::Lag => {
                let dtype_i64 = table.columns[ci].dtype == "i64";
                let _ = dtype_i64;
                for i in 0..n {
                    if i < op.periods {
                        col.push(Value::Null);
                    } else {
                        col.push(sorted_rows[i - op.periods][ci].clone());
                    }
                }
            }
            SeriesFn::Lead => {
                for i in 0..n {
                    let j = i + op.periods;
                    if j >= n {
                        col.push(Value::Null);
                    } else {
                        col.push(sorted_rows[j][ci].clone());
                    }
                }
            }
            SeriesFn::Diff => {
                for i in 0..n {
                    if i < op.periods {
                        col.push(Value::Null);
                        continue;
                    }
                    let cur = finite_num(&sorted_rows[i][ci]);
                    let prev = finite_num(&sorted_rows[i - op.periods][ci]);
                    match (cur, prev) {
                        (Some(c), Some(p)) => {
                            let d = c - p;
                            let both_i64 = matches!(sorted_rows[i][ci], Value::I64(_))
                                && matches!(sorted_rows[i - op.periods][ci], Value::I64(_));
                            col.push(numeric_result("diff", both_i64 && d.fract() == 0.0, d));
                        }
                        _ => col.push(Value::Null),
                    }
                }
            }
            SeriesFn::PctChange => {
                for i in 0..n {
                    if i < op.periods {
                        col.push(Value::Null);
                        continue;
                    }
                    let cur = finite_num(&sorted_rows[i][ci]);
                    let prev = finite_num(&sorted_rows[i - op.periods][ci]);
                    match (cur, prev) {
                        (Some(c), Some(p)) if p != 0.0 => {
                            col.push(Value::F64((c - p) / p));
                        }
                        _ => col.push(Value::Null),
                    }
                }
            }
            SeriesFn::Rolling { window, agg } => {
                let window = *window;
                let agg = agg.as_str();
                // Running buffer of finite values in trailing window of positions.
                // Spec: count non-null finite in positions i-window+1..=i; if count < window → Null.
                for i in 0..n {
                    let start = i.saturating_sub(window - 1);
                    let mut nums: Vec<f64> = Vec::with_capacity(window);
                    for j in start..=i {
                        if let Some(v) = finite_num(&sorted_rows[j][ci]) {
                            nums.push(v);
                        }
                    }
                    if nums.len() < window {
                        col.push(Value::Null);
                        continue;
                    }
                    let raw = match agg {
                        "mean" => nums.iter().sum::<f64>() / nums.len() as f64,
                        "sum" => nums.iter().sum::<f64>(),
                        "min" => nums.iter().cloned().fold(f64::INFINITY, f64::min),
                        "max" => nums.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
                        _ => unreachable!(),
                    };
                    let all_i64 = match agg {
                        "mean" | "sum" => false,
                        _ => {
                            // min/max: all contributing values in column slice are I64
                            (start..=i).all(|j| matches!(sorted_rows[j][ci], Value::Null | Value::I64(_)))
                        }
                    };
                    col.push(numeric_result(agg, all_i64, raw));
                }
            }
        }
        new_cols.push(col);
    }

    // Assemble output rows in sorted order (original cols + new cols).
    let mut out_rows: Vec<Vec<Value>> = Vec::with_capacity(n);
    for (i, base) in sorted_rows.iter().enumerate() {
        if out_rows.len() >= max_rows {
            return fail(3, "analysis/limit: series result too large");
        }
        let mut row = base.clone();
        for col in &new_cols {
            row.push(col[i].clone());
        }
        out_rows.push(row);
    }

    // Fix dtypes for lag/lead to source dtype; rolling mean/sum f64; min/max per column.
    for (k, op) in ops.iter().enumerate() {
        match &op.r#fn {
            SeriesFn::Lag | SeriesFn::Lead => {
                out_cols[table.columns.len() + k].dtype =
                    table.columns[col_idxs[k]].dtype.clone();
            }
            SeriesFn::Diff => {
                let all_i64 = new_cols[k].iter().all(|v| {
                    matches!(v, Value::Null | Value::I64(_))
                }) && new_cols[k].iter().any(|v| matches!(v, Value::I64(_)));
                out_cols[table.columns.len() + k].dtype = if all_i64 {
                    "i64".into()
                } else {
                    "f64".into()
                };
            }
            SeriesFn::PctChange => {
                out_cols[table.columns.len() + k].dtype = "f64".into();
            }
            SeriesFn::Rolling { agg, .. } => {
                out_cols[table.columns.len() + k].dtype = if agg == "mean" || agg == "sum" {
                    "f64".into()
                } else {
                    let ci = col_idxs[k];
                    let all_i64 = (0..n).all(|i| {
                        matches!(
                            sorted_rows[i][ci],
                            Value::Null | Value::I64(_)
                        )
                    });
                    if all_i64 {
                        "i64".into()
                    } else {
                        "f64".into()
                    }
                };
            }
        }
    }

    let new_id = new_dataset_id();
    let result = Table {
        columns: out_cols,
        rows: out_rows,
    };
    write_table_parquet_placeholder(workspace, &new_id, &result).map_err(|e| {
        eprintln!("{e}");
        1u8
    })?;
    let out_meta = Meta {
        dataset_id: new_id.clone(),
        row_count: result.rows.len(),
        columns: result.columns.clone(),
        source_kind: "series".into(),
        source_path: format!("series:{id}"),
        label,
        produced_by: "series".into(),
        parent_dataset_id: Some(id.clone()),
    };
    save_meta(workspace, &out_meta).map_err(|e| {
        eprintln!("{e}");
        1u8
    })?;
    let _ = meta;
    Ok(format!(
        "{{\"ok\":true,\"datasetId\":{},\"rowCount\":{},\"columnCount\":{},\"columns\":[{}],\"sourceDatasetId\":{},\"orderBy\":{},\"opCount\":{}}}",
        json_string(&new_id),
        result.rows.len(),
        result.columns.len(),
        out_meta.columns_json(),
        json_string(&id),
        json_string(&order_by),
        ops.len()
    ))
}
