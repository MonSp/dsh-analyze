//! Data-quality commands: profile / dedup / outliers (v0.1.3).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::cli::{env_u64, fail, flag_value};
use crate::dataset::{
    json_string, load_dataset, new_dataset_id, save_meta, write_table_parquet_placeholder, Meta,
    Table, Value,
};

fn load_or_fail(workspace: &Path, id: &str) -> Result<(crate::dataset::Meta, Table), u8> {
    load_dataset(workspace, id).map_err(|e| {
        eprintln!("{e}");
        if e.contains("unknown") {
            5u8
        } else {
            2u8
        }
    })
}

fn parse_column_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn json_f64(v: f64) -> String {
    if v.is_finite() {
        // Match existing style: short decimal for rates; values as plain floats.
        let s = format!("{v}");
        s
    } else {
        "null".into()
    }
}

fn json_rate(num: usize, den: usize) -> String {
    let r = (num as f64) / (den.max(1) as f64);
    if r.is_finite() {
        format!("{:.6}", r)
    } else {
        "null".into()
    }
}

fn json_f64_fixed(v: f64) -> String {
    if v.is_finite() {
        format!("{:.6}", v)
    } else {
        "null".into()
    }
}

// ---------------------------------------------------------------------------
// profile
// ---------------------------------------------------------------------------

pub(crate) fn cmd_profile(workspace: &Path, args: &[String]) -> Result<String, u8> {
    let id = match flag_value(args, "--dataset-id") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --dataset-id required"),
    };
    let hc_threshold: usize = match flag_value(args, "--high-cardinality-threshold") {
        Some(raw) => match raw.parse::<usize>() {
            Ok(n) if n >= 1 => n,
            _ => {
                return fail(
                    2,
                    "analysis/invalid-args: --high-cardinality-threshold must be integer >= 1",
                )
            }
        },
        None => 200,
    };

    let (meta, table) = load_or_fail(workspace, &id)?;
    let row_count = meta.row_count.max(table.rows.len());

    let mut columns_json = Vec::with_capacity(table.columns.len());
    let mut warnings: Vec<(String, Option<String>, usize, Option<usize>)> = Vec::new();
    // (code, column, nunique_or_0, threshold_opt)

    if row_count == 0 {
        warnings.push(("empty_table".into(), None, 0, None));
    }

    for (ci, col) in table.columns.iter().enumerate() {
        let mut nulls = 0usize;
        let mut numeric_count = 0usize;
        let mut bool_count = 0usize;
        let mut seen: HashSet<Value> = HashSet::new();
        let mut samples: Vec<Value> = Vec::new();
        let mut has_numeric = false;
        let mut has_non_numeric_non_null = false;

        for row in &table.rows {
            let v = row.get(ci).cloned().unwrap_or(Value::Null);
            if matches!(v, Value::Null) {
                nulls += 1;
                continue;
            }
            seen.insert(v.clone());
            if samples.len() < 3 && !samples.iter().any(|s| s == &v) {
                samples.push(v.clone());
            }
            match &v {
                Value::Bool(_) => bool_count += 1,
                Value::I64(_) => {
                    numeric_count += 1;
                    has_numeric = true;
                }
                Value::F64(_) => {
                    numeric_count += 1;
                    has_numeric = true;
                }
                Value::Str(s) => {
                    if s.trim().parse::<f64>().is_ok() {
                        numeric_count += 1;
                        has_numeric = true;
                    } else {
                        has_non_numeric_non_null = true;
                    }
                }
                Value::Null => {}
            }
        }

        let non_null = table.rows.len().saturating_sub(nulls);
        let nunique = seen.len();
        let null_rate = if row_count > 0 {
            nulls as f64 / row_count as f64
        } else {
            0.0
        };
        let unique_rate = if non_null > 0 {
            nunique as f64 / non_null as f64
        } else {
            0.0
        };
        let high_cardinality = nunique > hc_threshold;
        let numeric_share = if row_count > 0 {
            numeric_count as f64 / row_count as f64
        } else {
            0.0
        };

        // typeStability from cell kinds among non-null
        let type_stability = classify_stability(&table, ci, nulls);

        // warnings
        if row_count > 0 && nulls == row_count {
            warnings.push(("all_null".into(), Some(col.name.clone()), 0, None));
        } else if null_rate >= 0.5 {
            warnings.push(("mostly_null".into(), Some(col.name.clone()), 0, None));
        }
        if high_cardinality {
            warnings.push((
                "high_cardinality".into(),
                Some(col.name.clone()),
                nunique,
                Some(hc_threshold),
            ));
        }
        if type_stability == "mixed" || type_stability == "mixed_numeric" {
            warnings.push(("mixed_types".into(), Some(col.name.clone()), 0, None));
        }

        let samples_json = samples
            .iter()
            .map(|v| v.to_json())
            .collect::<Vec<_>>()
            .join(",");

        columns_json.push(format!(
            concat!(
                "{{\"name\":{},\"dtype\":{},\"nullCount\":{},\"nullRate\":{},",
                "\"nunique\":{},\"uniqueRate\":{},\"highCardinality\":{},",
                "\"typeStability\":{},\"numericCount\":{},\"numericShare\":{},",
                "\"boolCount\":{},\"sampleValues\":[{}]}}"
            ),
            json_string(&col.name),
            json_string(&col.dtype),
            nulls,
            json_f64_fixed(null_rate),
            nunique,
            json_f64_fixed(unique_rate),
            high_cardinality,
            json_string(&type_stability),
            numeric_count,
            json_f64_fixed(numeric_share),
            bool_count,
            samples_json
        ));

        let _ = (has_numeric, has_non_numeric_non_null);
    }

    // suspect primary keys: single-column unique non-null
    let mut suspect_keys = Vec::new();
    for (ci, col) in table.columns.iter().enumerate() {
        let mut nulls = 0usize;
        let mut seen: HashSet<Value> = HashSet::new();
        for row in &table.rows {
            let v = row.get(ci).cloned().unwrap_or(Value::Null);
            if matches!(v, Value::Null) {
                nulls += 1;
            } else {
                seen.insert(v);
            }
        }
        if row_count > 0 && nulls == 0 && seen.len() == row_count {
            suspect_keys.push(format!(
                "{{\"columns\":[{}],\"reason\":\"unique_non_null\",\"nullCount\":0,\"nunique\":{}}}",
                json_string(&col.name),
                row_count
            ));
            if suspect_keys.len() >= 10 {
                break;
            }
        }
    }

    // warnings cap 50
    let total_warnings = warnings.len();
    if total_warnings > 50 {
        warnings.truncate(49);
        warnings.push((
            "warnings_truncated".into(),
            None,
            50,
            Some(total_warnings),
        ));
    }
    let warnings_json = warnings
        .iter()
        .map(|(code, col, n, thr)| {
            let mut parts = vec![format!("\"code\":{}", json_string(code))];
            if let Some(c) = col {
                parts.push(format!("\"column\":{}", json_string(c)));
            }
            match code.as_str() {
                "high_cardinality" => {
                    parts.push(format!("\"nunique\":{n}"));
                    if let Some(t) = thr {
                        parts.push(format!("\"threshold\":{t}"));
                    }
                }
                "warnings_truncated" => {
                    parts.push(format!("\"emitted\":{n}"));
                    if let Some(t) = thr {
                        parts.push(format!("\"total\":{t}"));
                    }
                }
                _ => {}
            }
            format!("{{{}}}", parts.join(","))
        })
        .collect::<Vec<_>>()
        .join(",");

    Ok(format!(
        "{{\"ok\":true,\"datasetId\":{},\"rowCount\":{},\"columnCount\":{},\"columns\":[{}],\"suspectKeys\":[{}],\"warnings\":[{}]}}",
        json_string(&id),
        row_count,
        table.columns.len(),
        columns_json.join(","),
        suspect_keys.join(","),
        warnings_json
    ))
}

fn classify_stability(table: &Table, ci: usize, nulls: usize) -> &'static str {
    let non_null = table.rows.len().saturating_sub(nulls);
    if non_null == 0 {
        return "empty";
    }
    let mut has_bool = false;
    let mut has_i64 = false;
    let mut has_f64 = false;
    let mut has_str = false;
    let mut has_numeric_str = false;
    let mut has_non_numeric_str = false;
    for row in &table.rows {
        match row.get(ci) {
            None | Some(Value::Null) => {}
            Some(Value::Bool(_)) => has_bool = true,
            Some(Value::I64(_)) => has_i64 = true,
            Some(Value::F64(_)) => has_f64 = true,
            Some(Value::Str(s)) => {
                has_str = true;
                if s.trim().parse::<f64>().is_ok() {
                    has_numeric_str = true;
                } else {
                    has_non_numeric_str = true;
                }
            }
        }
    }
    // Spec: string = every non-null cell is Str
    if has_str && !has_bool && !has_i64 && !has_f64 {
        return "string";
    }
    if has_bool && !has_str && !has_i64 && !has_f64 {
        return "bool";
    }
    let any_numeric = has_i64 || has_f64 || has_numeric_str;
    let any_non_numeric_non_null = has_bool || has_non_numeric_str;
    if any_numeric && any_non_numeric_non_null {
        return "mixed_numeric";
    }
    if has_bool && has_str {
        return "mixed";
    }
    if has_i64 && !has_f64 && !has_str {
        return "int";
    }
    if has_f64 || (has_i64 && has_str) || (has_f64 && has_i64) {
        return "float";
    }
    if has_i64 && has_f64 {
        return "float";
    }
    "mixed"
}

// ---------------------------------------------------------------------------
// dedup
// ---------------------------------------------------------------------------

struct DedupGroup {
    count: usize,
    first_index: usize,
    key_vals: Vec<Value>,
    sample_rows: Vec<usize>,
}

pub(crate) fn cmd_dedup(workspace: &Path, args: &[String]) -> Result<String, u8> {
    let id = match flag_value(args, "--dataset-id") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --dataset-id required"),
    };
    let sample_limit: usize = flag_value(args, "--sample-limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(5)
        .clamp(1, 20);
    let key_columns: Option<Vec<String>> = flag_value(args, "--key-columns").map(parse_column_list);

    if let Some(keys) = &key_columns {
        if keys.is_empty() {
            return fail(2, "analysis/invalid-args: --key-columns must list at least one column");
        }
        if keys.len() > 16 {
            return fail(2, "analysis/invalid-args: --key-columns supports at most 16 columns");
        }
        let mut uniq = HashSet::new();
        for k in keys {
            if !uniq.insert(k.clone()) {
                return fail(2, &format!("analysis/invalid-args: duplicate key column {k}"));
            }
        }
    }

    let (meta, table) = load_or_fail(workspace, &id)?;
    let row_count = meta.row_count.max(table.rows.len());

    let mut key_idx: Vec<usize> = Vec::new();
    if let Some(keys) = &key_columns {
        for name in keys {
            let idx = table
                .columns
                .iter()
                .position(|c| &c.name == name)
                .ok_or_else(|| {
                    eprintln!("analysis/invalid-args: unknown column {name}");
                    2u8
                })
                .map_err(|e| e)?;
            key_idx.push(idx);
        }
    }

    let mut groups: HashMap<Vec<Value>, DedupGroup> = HashMap::new();
    let rows_per_group_cap = 3usize;
    for (ri, row) in table.rows.iter().enumerate() {
        let key: Vec<Value> = if key_idx.is_empty() {
            row.iter().cloned().collect()
        } else {
            key_idx
                .iter()
                .map(|&ci| row.get(ci).cloned().unwrap_or(Value::Null))
                .collect()
        };
        let entry = groups.entry(key.clone());
        match entry {
            std::collections::hash_map::Entry::Occupied(mut o) => {
                let g = o.get_mut();
                g.count += 1;
                if g.sample_rows.len() < rows_per_group_cap {
                    g.sample_rows.push(ri);
                }
            }
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(DedupGroup {
                    count: 1,
                    first_index: ri,
                    key_vals: key,
                    sample_rows: vec![ri],
                });
            }
        }
    }

    let distinct = groups.len();
    let duplicate_row_count = row_count.saturating_sub(distinct);
    let mut max_multiplicity = 1usize;
    let mut duplicate_key_count = 0usize;
    let mut dup_groups: Vec<&DedupGroup> = Vec::new();
    for g in groups.values() {
        if g.count > max_multiplicity {
            max_multiplicity = g.count;
        }
        if g.count >= 2 {
            duplicate_key_count += 1;
            dup_groups.push(g);
        }
    }
    dup_groups.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.first_index.cmp(&b.first_index))
    });
    let total_dup_groups = dup_groups.len();
    dup_groups.truncate(sample_limit);
    let samples_truncated = total_dup_groups > sample_limit;

    let samples_json = dup_groups
        .iter()
        .map(|g| {
            let rows_json = g
                .sample_rows
                .iter()
                .map(|&ri| {
                    let cells = table.rows[ri]
                        .iter()
                        .map(|v| v.to_json())
                        .collect::<Vec<_>>()
                        .join(",");
                    format!("[{cells}]")
                })
                .collect::<Vec<_>>()
                .join(",");
            let key_json = if key_idx.is_empty() {
                "null".to_string()
            } else {
                let cells = g
                    .key_vals
                    .iter()
                    .map(|v| v.to_json())
                    .collect::<Vec<_>>()
                    .join(",");
                format!("[{cells}]")
            };
            format!(
                "{{\"key\":{},\"count\":{},\"rows\":[{}]}}",
                key_json, g.count, rows_json
            )
        })
        .collect::<Vec<_>>()
        .join(",");

    let key_columns_json = match &key_columns {
        None => "null".to_string(),
        Some(ks) => {
            let cells = ks.iter().map(|k| json_string(k)).collect::<Vec<_>>().join(",");
            format!("[{cells}]")
        }
    };

    if key_idx.is_empty() {
        Ok(format!(
            "{{\"ok\":true,\"datasetId\":{},\"rowCount\":{},\"mode\":\"exact\",\"keyColumns\":null,\"duplicateRowCount\":{},\"duplicateRowRate\":{},\"distinctRowCount\":{},\"duplicateRowCountGroups\":{},\"maxMultiplicity\":{},\"samples\":[{}],\"samplesTruncated\":{}}}",
            json_string(&id),
            row_count,
            duplicate_row_count,
            json_rate(duplicate_row_count, row_count),
            distinct,
            total_dup_groups,
            max_multiplicity,
            samples_json,
            samples_truncated
        ))
    } else {
        Ok(format!(
            "{{\"ok\":true,\"datasetId\":{},\"rowCount\":{},\"mode\":\"keyed\",\"keyColumns\":{},\"duplicateRowCount\":{},\"duplicateRowRate\":{},\"distinctKeyCount\":{},\"duplicateKeyCount\":{},\"maxMultiplicity\":{},\"samples\":[{}],\"samplesTruncated\":{}}}",
            json_string(&id),
            row_count,
            key_columns_json,
            duplicate_row_count,
            json_rate(duplicate_row_count, row_count),
            distinct,
            duplicate_key_count,
            max_multiplicity,
            samples_json,
            samples_truncated
        ))
    }
}

// ---------------------------------------------------------------------------
// outliers
// ---------------------------------------------------------------------------

pub(crate) fn cmd_outliers(workspace: &Path, args: &[String]) -> Result<String, u8> {
    let id = match flag_value(args, "--dataset-id") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --dataset-id required"),
    };
    let columns_raw = match flag_value(args, "--columns") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --columns required"),
    };
    let columns = parse_column_list(&columns_raw);
    if columns.is_empty() {
        return fail(2, "analysis/invalid-args: --columns must list at least one column");
    }
    if columns.len() > 32 {
        return fail(2, "analysis/invalid-args: --columns supports at most 32 columns");
    }
    let method = flag_value(args, "--method")
        .map(|s| s.to_string())
        .unwrap_or_else(|| "iqr".into());
    if method != "iqr" && method != "zscore" && method != "mad" {
        return fail(2, "analysis/invalid-args: --method must be iqr|zscore|mad");
    }
    let default_k = match method.as_str() {
        "iqr" => 1.5,
        _ => 3.0,
    };
    let k: f64 = match flag_value(args, "--k") {
        Some(raw) => match raw.parse::<f64>() {
            Ok(v) if v.is_finite() && v > 0.0 => v,
            _ => return fail(2, "analysis/invalid-args: --k must be a finite number > 0"),
        },
        None => default_k,
    };
    let sample_limit: usize = flag_value(args, "--sample-limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(10)
        .clamp(1, 20);
    let out_label = flag_value(args, "--out").map(|s| s.to_string());
    let max_rows: usize =
        env_u64("DSH_ANALYZE_MAX_ROWS", "INV_ANALYSIS_MAX_ROWS", 5_000_000) as usize;

    let (meta, table) = load_or_fail(workspace, &id)?;
    let row_count = meta.row_count.max(table.rows.len());

    let mut col_idxs = Vec::with_capacity(columns.len());
    for name in &columns {
        let idx = table
            .columns
            .iter()
            .position(|c| &c.name == name)
            .ok_or_else(|| {
                eprintln!("analysis/invalid-args: unknown column {name}");
                2u8
            })
            .map_err(|e| e)?;
        col_idxs.push(idx);
    }

    // Collect numeric series + outlier row indices per column
    struct ColResult {
        name: String,
        dtype: String,
        numeric_count: usize,
        null_count: usize,
        lower: Option<f64>,
        upper: Option<f64>,
        outlier_count: usize,
        degenerate_iqr: bool,
        degenerate_std: bool,
        degenerate_mad: bool,
        sample_row_idxs: Vec<usize>,
        sample_values: Vec<f64>,
    }

    let mut results: Vec<ColResult> = Vec::new();
    let mut any_outlier_rows: HashSet<usize> = HashSet::new();

    for (&ci, name) in col_idxs.iter().zip(columns.iter()) {
        let dtype = table.columns[ci].dtype.clone();
        let mut null_count = 0usize;
        let mut series: Vec<(usize, f64)> = Vec::new();
        for (ri, row) in table.rows.iter().enumerate() {
            match row.get(ci) {
                None | Some(Value::Null) => null_count += 1,
                Some(v) => match v.as_f64() {
                    Some(n) if n.is_finite() => series.push((ri, n)),
                    _ => {
                        // non-numeric non-null: not counted in numeric; not null either
                        // Spec: excluded from bound computation; nullCount tracks Null only.
                    }
                },
            }
        }
        let numeric_count = series.len();
        let mut nums: Vec<f64> = series.iter().map(|(_, n)| *n).collect();
        nums.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let mut lower = None;
        let mut upper = None;
        let mut outlier_flags: Vec<(usize, f64)> = Vec::new();
        let mut degenerate_iqr = false;
        let mut degenerate_std = false;
        let mut degenerate_mad = false;

        if numeric_count >= 2 {
            match method.as_str() {
                "iqr" => {
                    let q1 = percentile(&nums, 0.25);
                    let q3 = percentile(&nums, 0.75);
                    let iqr = q3 - q1;
                    if iqr == 0.0 {
                        degenerate_iqr = true;
                        // Spec: no outliers when IQR==0; bounds set to Q50 conceptually but we report q1==q3
                        lower = Some(q1);
                        upper = Some(q3);
                    } else {
                        lower = Some(q1 - k * iqr);
                        upper = Some(q3 + k * iqr);
                        let lo = lower.unwrap();
                        let hi = upper.unwrap();
                        for &(ri, n) in &series {
                            if n < lo || n > hi {
                                outlier_flags.push((ri, n));
                            }
                        }
                    }
                }
                "zscore" => {
                    let mean = nums.iter().sum::<f64>() / nums.len() as f64;
                    let var = nums.iter().map(|x| (x - mean).powi(2)).sum::<f64>()
                        / (nums.len() - 1) as f64;
                    let std = var.sqrt();
                    if std == 0.0 || !std.is_finite() {
                        degenerate_std = true;
                        lower = Some(mean);
                        upper = Some(mean);
                    } else {
                        lower = Some(mean - k * std);
                        upper = Some(mean + k * std);
                        let lo = lower.unwrap();
                        let hi = upper.unwrap();
                        for &(ri, n) in &series {
                            if (n - mean).abs() / std > k {
                                outlier_flags.push((ri, n));
                            }
                        }
                        let _ = (lo, hi);
                    }
                }
                _ => {
                    // mad
                    let med = percentile(&nums, 0.5);
                    let mut devs: Vec<f64> = nums.iter().map(|x| (x - med).abs()).collect();
                    devs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                    let mad = percentile(&devs, 0.5);
                    let scale = 1.4826 * mad;
                    if mad == 0.0 || scale == 0.0 || !scale.is_finite() {
                        degenerate_mad = true;
                        lower = Some(med);
                        upper = Some(med);
                    } else {
                        lower = Some(med - k * scale);
                        upper = Some(med + k * scale);
                        let hi = upper.unwrap();
                        let lo = lower.unwrap();
                        for &(ri, n) in &series {
                            if (n - med).abs() / scale > k {
                                outlier_flags.push((ri, n));
                                let _ = (lo, hi);
                            }
                        }
                    }
                }
            }
        }

        outlier_flags.sort_by_key(|(ri, _)| *ri);
        let outlier_count = outlier_flags.len();
        for (ri, _) in outlier_flags.iter() {
            any_outlier_rows.insert(*ri);
        }
        let sample_row_idxs: Vec<usize> = outlier_flags.iter().take(sample_limit).map(|(ri, _)| *ri).collect();
        let sample_values: Vec<f64> = outlier_flags
            .iter()
            .take(sample_limit)
            .map(|(_, n)| *n)
            .collect();

        results.push(ColResult {
            name: name.clone(),
            dtype,
            numeric_count,
            null_count,
            lower,
            upper,
            outlier_count,
            degenerate_iqr,
            degenerate_std,
            degenerate_mad,
            sample_row_idxs,
            sample_values,
        });
    }

    // Optional --out dataset
    let mut out_dataset_id = String::from("null");
    let mut out_row_count_json = String::new();
    if out_label.is_some() {
        let mut out_rows: Vec<Vec<Value>> = Vec::new();
        let mut idxs: Vec<usize> = any_outlier_rows.into_iter().collect();
        idxs.sort_unstable();
        for ri in idxs {
            out_rows.push(table.rows[ri].clone());
            if out_rows.len() > max_rows {
                return fail(3, "analysis/limit: outlier dataset too large");
            }
        }
        let new_id = new_dataset_id();
        let result = Table {
            columns: table.columns.clone(),
            rows: out_rows,
        };
        write_table_parquet_placeholder(workspace, &new_id, &result).map_err(|e| {
            eprintln!("{e}");
            1u8
        })?;
        let meta_out = Meta {
            dataset_id: new_id.clone(),
            row_count: result.rows.len(),
            columns: result.columns.clone(),
            source_kind: "outliers".into(),
            source_path: format!("outliers:{id}"),
            label: out_label.clone(),
            produced_by: "outliers".into(),
            parent_dataset_id: Some(id.clone()),
        };
        save_meta(workspace, &meta_out).map_err(|e| {
            eprintln!("{e}");
            1u8
        })?;
        out_row_count_json = format!(",\"outRowCount\":{}", result.rows.len());
        out_dataset_id = new_id;
    }

    let columns_json = results
        .iter()
        .map(|r| {
            let samples: String = r
                .sample_row_idxs
                .iter()
                .zip(r.sample_values.iter())
                .map(|(&ri, &val)| {
                    let row_cells = table.rows[ri]
                        .iter()
                        .map(|v| v.to_json())
                        .collect::<Vec<_>>()
                        .join(",");
                    format!(
                        "{{\"rowIndex\":{},\"value\":{},\"row\":[{}]}}",
                        ri,
                        json_f64(val),
                        row_cells
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            let lower_s = r.lower.map(json_f64).unwrap_or_else(|| "null".into());
            let upper_s = r.upper.map(json_f64).unwrap_or_else(|| "null".into());
            format!(
                concat!(
                    "{{\"name\":{},\"dtype\":{},\"numericCount\":{},\"nullCount\":{},",
                    "\"lowerBound\":{},\"upperBound\":{},\"outlierCount\":{},\"outlierRate\":{},",
                    "\"degenerateIqr\":{},\"degenerateStd\":{},\"degenerateMad\":{},",
                    "\"samples\":[{}]}}"
                ),
                json_string(&r.name),
                json_string(&r.dtype),
                r.numeric_count,
                r.null_count,
                lower_s,
                upper_s,
                r.outlier_count,
                json_rate(r.outlier_count, r.numeric_count),
                r.degenerate_iqr,
                r.degenerate_std,
                r.degenerate_mad,
                samples
            )
        })
        .collect::<Vec<_>>()
        .join(",");

    Ok(format!(
        "{{\"ok\":true,\"datasetId\":{},\"rowCount\":{},\"method\":{},\"k\":{},\"outDatasetId\":{}{},\"columns\":[{}]}}",
        json_string(&id),
        row_count,
        json_string(&method),
        json_f64(k),
        out_dataset_id,
        out_row_count_json,
        columns_json
    ))
}

/// Same nearest-rank percentile as describe: idx = round((n-1)*p) clamped.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}
