//! Time-series transforms: lag / lead / diff / pct_change / rolling (v0.1.5).

use std::cmp::Ordering;
use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::Path;

use crate::cli::{env_u64, fail, flag_value};
use crate::commands::load_or_fail;
use crate::dataset::{
    ensure_within_workspace, json_string, new_dataset_id, parse_json_object, save_meta,
    write_table_parquet_placeholder, Column, Meta, Table, Value,
};
use crate::expr::validate_as_name;

const NULL: Value = Value::Null;
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

fn map_str(map: &BTreeMap<String, Value>, key: &str) -> Result<Option<String>, String> {
    match map.get(key) {
        None => Ok(None),
        Some(Value::Str(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("analysis/invalid-args: {key} must be a string")),
    }
}

fn map_usize(map: &BTreeMap<String, Value>, key: &str) -> Result<Option<usize>, String> {
    match map.get(key) {
        None => Ok(None),
        // Do not compare against `usize::MAX as i64` — on 64-bit that wraps to -1.
        Some(Value::I64(n)) if *n >= 0 => Ok(Some(*n as usize)),
        Some(Value::F64(f))
            if f.fract() == 0.0 && *f >= 0.0 && *f <= usize::MAX as f64 =>
        {
            Ok(Some(*f as usize))
        }
        _ => Err(format!(
            "analysis/invalid-args: {key} must be a non-negative integer"
        )),
    }
}

fn has_unknown_keys(map: &BTreeMap<String, Value>, allowed: &[&str]) -> bool {
    map.keys().any(|k| !allowed.contains(&k.as_str()))
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
        // Real flat-object parse (map keys), not substring search — column names
        // may equal field names like "periods"/"window"/"agg".
        let map = parse_json_object(obj).map_err(|_| {
            "analysis/invalid-args: each op must be a JSON object with scalar fields".to_string()
        })?;
        let fn_name = map_str(&map, "fn")?
            .ok_or_else(|| "analysis/invalid-args: op requires fn".to_string())?;
        let column = map_str(&map, "column")?
            .ok_or_else(|| "analysis/invalid-args: op requires column".to_string())?;
        let as_name = map_str(&map, "as")?
            .ok_or_else(|| "analysis/invalid-args: op requires as".to_string())?;
        if column.trim().is_empty() {
            return Err("analysis/invalid-args: column must be non-empty".into());
        }
        if !validate_as_name(&as_name) {
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

        let periods = map_usize(&map, "periods")?;
        match fn_name.as_str() {
            "lag" | "lead" | "diff" | "pct_change" => {
                if has_unknown_keys(&map, &["fn", "column", "as", "periods"]) {
                    return Err(format!(
                        "analysis/invalid-args: unknown field in op fn={fn_name}"
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
                if has_unknown_keys(&map, &["fn", "column", "as", "window", "agg"]) {
                    return Err(
                        "analysis/invalid-args: unknown field in op fn=rolling".into(),
                    );
                }
                if periods.is_some() {
                    return Err("analysis/invalid-args: rolling takes window, not periods".into());
                }
                let window = map_usize(&map, "window")?.ok_or_else(|| {
                    "analysis/invalid-args: rolling requires window".to_string()
                })?;
                if !(1..=MAX_WINDOW).contains(&window) {
                    return Err(format!(
                        "analysis/invalid-args: window must be 1..={MAX_WINDOW}"
                    ));
                }
                let agg = map_str(&map, "agg")?
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
    fn rank(v: &Value) -> u8 {
        match v {
            Value::Bool(_) => 0,
            Value::I64(_) | Value::F64(_) => 1,
            Value::Str(_) => 2,
            Value::Null => 3,
        }
    }
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Greater,
        (_, Value::Null) => Ordering::Less,
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
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
        _ => rank(a).cmp(&rank(b)),
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

    let (meta, mut table) = load_or_fail(workspace, &id)?;
    let _ = meta;
    // Series never drops rows; reject oversized input before sort/ops.
    if table.rows.len() > max_rows {
        return fail(3, "analysis/limit: series result too large");
    }
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

    // Stable sort by order-by; nulls last. Compare by reference (no Value clones).
    let mut order: Vec<usize> = (0..table.rows.len()).collect();
    order.sort_by(|&i, &j| {
        let a = table.rows[i].get(order_idx);
        let b = table.rows[j].get(order_idx);
        match (a, b) {
            (Some(a), Some(b)) => compare_keys(a, b).then(i.cmp(&j)),
            (None, None) => i.cmp(&j),
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
        }
    });

    let n = order.len();
    let mut out_cols: Vec<Column> = table.columns.clone();
    for op in &ops {
        out_cols.push(Column {
            name: op.as_name.clone(),
            dtype: "f64".into(),
        });
    }

    let cell = |i: usize, ci: usize| -> &Value {
        table.rows[order[i]].get(ci).unwrap_or(&NULL)
    };

    // Whole source column is only I64/null → rolling min/max emit I64 and dtype i64.
    let mut col_all_i64: Vec<bool> = table.columns.iter().map(|_| true).collect();
    for row in &table.rows {
        for (ci, cell_val) in row.iter().enumerate() {
            match cell_val {
                Value::Null | Value::I64(_) => {}
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
                for i in 0..n {
                    if i < op.periods {
                        col.push(Value::Null);
                    } else {
                        col.push(cell(i - op.periods, ci).clone());
                    }
                }
            }
            SeriesFn::Lead => {
                for i in 0..n {
                    let j = i + op.periods;
                    if j >= n {
                        col.push(Value::Null);
                    } else {
                        col.push(cell(j, ci).clone());
                    }
                }
            }
            SeriesFn::Diff => {
                for i in 0..n {
                    if i < op.periods {
                        col.push(Value::Null);
                        continue;
                    }
                    let cur = finite_num(cell(i, ci));
                    let prev = finite_num(cell(i - op.periods, ci));
                    match (cur, prev) {
                        (Some(c), Some(p)) => {
                            let d = c - p;
                            let both_i64 =
                                matches!(cell(i, ci), Value::I64(_))
                                    && matches!(cell(i - op.periods, ci), Value::I64(_));
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
                    let cur = finite_num(cell(i, ci));
                    let prev = finite_num(cell(i - op.periods, ci));
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
                // Spec: non-null only when positions i-window+1..=i are all finite
                // (count of finite must equal window). Incomplete windows → Null.
                let mut vals: Vec<Option<f64>> = Vec::with_capacity(n);
                for i in 0..n {
                    vals.push(finite_num(cell(i, ci)));
                }
                let promote_i64 = col_all_i64[ci];
                match agg {
                    "mean" | "sum" => {
                        // O(n) sliding sum; bad = non-finite count in the trailing window.
                        let mut sum = 0.0f64;
                        let mut bad = 0usize;
                        for i in 0..n {
                            match vals[i] {
                                Some(v) => sum += v,
                                None => bad += 1,
                            }
                            if i >= window {
                                match vals[i - window] {
                                    Some(v) => sum -= v,
                                    None => bad -= 1,
                                }
                            }
                            if i + 1 < window || bad > 0 {
                                col.push(Value::Null);
                                continue;
                            }
                            let raw = if agg == "sum" {
                                sum
                            } else {
                                sum / window as f64
                            };
                            col.push(numeric_result(agg, false, raw));
                        }
                    }
                    "min" | "max" => {
                        // O(n) monotonic deque over finite values + non-finite count.
                        let want_min = agg == "min";
                        let mut bad = 0usize;
                        let mut dq: VecDeque<usize> = VecDeque::new();
                        for i in 0..n {
                            match vals[i] {
                                Some(v) => {
                                    while let Some(&b) = dq.back() {
                                        let bv = vals[b].unwrap_or(f64::NAN);
                                        let pop = if want_min { bv >= v } else { bv <= v };
                                        if pop {
                                            dq.pop_back();
                                        } else {
                                            break;
                                        }
                                    }
                                    dq.push_back(i);
                                }
                                None => bad += 1,
                            }
                            if i >= window {
                                match vals[i - window] {
                                    Some(_) => {
                                        if dq.front() == Some(&(i - window)) {
                                            dq.pop_front();
                                        }
                                    }
                                    None => bad -= 1,
                                }
                            }
                            if i + 1 < window || bad > 0 {
                                col.push(Value::Null);
                                continue;
                            }
                            let raw = vals[dq.front().copied().unwrap_or(i)].unwrap_or(0.0);
                            col.push(numeric_result(agg, promote_i64, raw));
                        }
                    }
                    _ => unreachable!(),
                }
            }
        }
        new_cols.push(col);
    }

    // Assemble output rows in sorted order without keeping a full sorted_rows copy.
    let mut out_rows: Vec<Vec<Value>> = Vec::with_capacity(n);
    let mut slots: Vec<Option<Vec<Value>>> =
        std::mem::take(&mut table.rows).into_iter().map(Some).collect();
    for (i, &ri) in order.iter().enumerate() {
        let mut row = slots[ri].take().expect("each source row used once");
        for col in &new_cols {
            row.push(col[i].clone());
        }
        out_rows.push(row);
    }
    drop(slots);

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
                } else if col_all_i64[col_idxs[k]] {
                    "i64".into()
                } else {
                    "f64".into()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ops_reads_integer_fields_from_map() {
        let ops = parse_ops(r#"[{"fn":"lag","column":"v","periods":1,"as":"v_lag"}]"#)
            .expect("parse");
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].periods, 1);
        assert_eq!(ops[0].column, "v");
    }

    #[test]
    fn parse_ops_column_named_periods_uses_real_key() {
        let ops = parse_ops(r#"[{"fn":"lag","column":"periods","as":"y","periods":3}]"#)
            .expect("parse");
        assert_eq!(ops[0].column, "periods");
        assert_eq!(ops[0].periods, 3);
    }
}
