use std::collections::HashMap;
use std::path::Path;

use crate::cli::{fail, flag_value};
use crate::dataset::{json_string, load_dataset, Value};

pub(crate) fn cmd_head(workspace: &Path, args: &[String]) -> Result<String, u8> {
    let id = match flag_value(args, "--dataset-id") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --dataset-id required"),
    };
    let n: usize = flag_value(args, "--n")
        .and_then(|s| s.parse().ok())
        .unwrap_or(5)
        .clamp(1, 20);
    let (meta, table) = load_dataset(workspace, &id).map_err(|e| {
        eprintln!("{e}");
        if e.contains("unknown") {
            5u8
        } else {
            2u8
        }
    })?;
    let cols: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();
    let rows_json = table
        .rows
        .iter()
        .take(n)
        .map(|row| {
            let cells = row
                .iter()
                .map(|v| v.to_json())
                .collect::<Vec<_>>()
                .join(",");
            format!("[{cells}]")
        })
        .collect::<Vec<_>>()
        .join(",");
    Ok(format!(
        "{{\"ok\":true,\"datasetId\":{},\"rowCount\":{},\"columns\":[{}],\"rows\":[{}]}}",
        json_string(&id),
        meta.row_count,
        cols.iter().map(|c| json_string(c)).collect::<Vec<_>>().join(","),
        rows_json
    ))
}

pub(crate) fn cmd_schema(workspace: &Path, args: &[String]) -> Result<String, u8> {
    let id = match flag_value(args, "--dataset-id") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --dataset-id required"),
    };
    let (meta, table) = load_dataset(workspace, &id).map_err(|e| {
        eprintln!("{e}");
        if e.contains("unknown") {
            5u8
        } else {
            2u8
        }
    })?;
    let _ = table;
    Ok(format!(
        "{{\"ok\":true,\"datasetId\":{},\"rowCount\":{},\"columns\":[{}]}}",
        json_string(&id),
        meta.row_count,
        meta.columns_json()
    ))
}

pub(crate) fn cmd_describe(workspace: &Path, args: &[String]) -> Result<String, u8> {
    let id = match flag_value(args, "--dataset-id") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --dataset-id required"),
    };
    let columns_filter = flag_value(args, "--columns")
        .map(|s| s.split(',').map(|x| x.trim().to_string()).collect::<Vec<_>>());
    let (_meta, table) = load_dataset(workspace, &id).map_err(|e| {
        eprintln!("{e}");
        if e.contains("unknown") {
            5u8
        } else {
            2u8
        }
    })?;
    let mut stats = Vec::new();
    for (i, col) in table.columns.iter().enumerate() {
        if let Some(filter) = &columns_filter {
            if !filter.iter().any(|c| c == &col.name) {
                continue;
            }
        }
        let mut nulls = 0usize;
        let mut nums: Vec<f64> = Vec::new();
        for row in &table.rows {
            let v = row.get(i).cloned().unwrap_or(Value::Null);
            match v {
                Value::Null => nulls += 1,
                other => {
                    if let Some(n) = other.as_f64() {
                        nums.push(n);
                    }
                }
            }
        }
        let mut min = Value::Null;
        let mut max = Value::Null;
        let mut mean = Value::Null;
        let mut std = Value::Null;
        let mut q25 = Value::Null;
        let mut q50 = Value::Null;
        let mut q75 = Value::Null;
        let mut q01 = Value::Null;
        let mut q10 = Value::Null;
        let mut q90 = Value::Null;
        let mut q99 = Value::Null;
        if !nums.is_empty() {
            let mut s = nums.clone();
            s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            min = Value::F64(s[0]);
            max = Value::F64(s[s.len() - 1]);
            mean = Value::F64(s.iter().sum::<f64>() / s.len() as f64);
            if s.len() > 1 {
                let m = mean.as_f64().unwrap();
                let var = s.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (s.len() - 1) as f64;
                std = Value::F64(var.sqrt());
            }
            let pct = |p: f64| -> f64 {
                let idx = ((s.len() as f64 - 1.0) * p).round() as usize;
                s[idx.min(s.len() - 1)]
            };
            q25 = Value::F64(pct(0.25));
            q50 = Value::F64(pct(0.50));
            q75 = Value::F64(pct(0.75));
            q01 = Value::F64(pct(0.01));
            q10 = Value::F64(pct(0.10));
            q90 = Value::F64(pct(0.90));
            q99 = Value::F64(pct(0.99));
        }
        stats.push(format!(
            "{{\"name\":{},\"dtype\":{},\"nullCount\":{},\"min\":{},\"max\":{},\"mean\":{},\"std\":{},\"q25\":{},\"q50\":{},\"q75\":{},\"q01\":{},\"q10\":{},\"q90\":{},\"q99\":{}}}",
            json_string(&col.name),
            json_string(&col.dtype),
            nulls,
            min.to_json(),
            max.to_json(),
            mean.to_json(),
            std.to_json(),
            q25.to_json(),
            q50.to_json(),
            q75.to_json(),
            q01.to_json(),
            q10.to_json(),
            q90.to_json(),
            q99.to_json()
        ));
    }
    Ok(format!(
        "{{\"ok\":true,\"datasetId\":{},\"stats\":[{}]}}",
        json_string(&id),
        stats.join(",")
    ))
}

pub(crate) fn cmd_values(workspace: &Path, args: &[String]) -> Result<String, u8> {
    let id = match flag_value(args, "--dataset-id") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --dataset-id required"),
    };
    let column = match flag_value(args, "--column") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --column required"),
    };
    let limit: usize = flag_value(args, "--limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(20)
        .clamp(1, 100);
    let (meta, table) = load_dataset(workspace, &id).map_err(|e| {
        eprintln!("{e}");
        if e.contains("unknown") {
            5u8
        } else {
            2u8
        }
    })?;
    let Some(ci) = table.columns.iter().position(|c| c.name == column) else {
        return fail(2, &format!("analysis/invalid-args: unknown column {column}"));
    };
    let dtype = table.columns[ci].dtype.clone();
    let mut null_count = 0usize;
    let mut counts: HashMap<Value, usize> = HashMap::new();
    for row in &table.rows {
        let v = row.get(ci).cloned().unwrap_or(Value::Null);
        if matches!(v, Value::Null) {
            null_count += 1;
        } else {
            *counts.entry(v).or_insert(0) += 1;
        }
    }
    let nunique = counts.len();
    let non_null = (meta.row_count.max(table.rows.len()))
        .saturating_sub(null_count)
        .max(1);
    let mut top: Vec<(Value, usize)> = counts.into_iter().collect();
    top.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.to_json().cmp(&b.0.to_json()))
    });
    top.truncate(limit);
    let top_json = top
        .iter()
        .map(|(v, c)| {
            format!(
                "{{\"value\":{},\"count\":{},\"ratio\":{}}}",
                v.to_json(),
                c,
                format!("{:.6}", (*c as f64) / (non_null as f64))
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    Ok(format!(
        "{{\"ok\":true,\"datasetId\":{},\"column\":{},\"dtype\":{},\"rowCount\":{},\"nullCount\":{},\"nunique\":{},\"limit\":{},\"top\":[{}]}}",
        json_string(&id),
        json_string(&column),
        json_string(&dtype),
        meta.row_count.max(table.rows.len()),
        null_count,
        nunique,
        limit,
        top_json
    ))
}
