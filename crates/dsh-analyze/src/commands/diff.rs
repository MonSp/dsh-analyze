//! Two-dataset comparison: onlyLeft / onlyRight / changed with samples (R-8b).

use std::collections::HashMap;
use std::path::Path;

use crate::cli::{env_u64, fail, flag_value};
use crate::commands::{clamp_warning_json, load_or_fail, parse_on_pairs};
use crate::dataset::{
    json_string, new_dataset_id, save_meta, write_table_parquet_placeholder, Column, Meta, Table,
    Value,
};

pub(crate) fn cmd_diff(workspace: &Path, args: &[String]) -> Result<String, u8> {
    let left_id = match flag_value(args, "--left") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --left required"),
    };
    let right_id = match flag_value(args, "--right") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --right required"),
    };
    let on_raw = flag_value(args, "--on").map(|s| s.to_string());
    let (requested_sample, sample_limit, warnings) = match flag_value(args, "--sample-limit") {
        Some(raw) => {
            let n: u64 = match raw.parse::<u64>() {
                Ok(n) if n >= 1 => n,
                _ => {
                    return fail(
                        2,
                        "analysis/invalid-args: --sample-limit must be integer 1..=20",
                    )
                }
            };
            if n > 20 {
                (
                    n,
                    20usize,
                    Some(clamp_warning_json("sampleLimit", n, 20)),
                )
            } else {
                (n, n as usize, None)
            }
        }
        None => (5u64, 5usize, None),
    };
    let _ = requested_sample;
    let max_rows: usize =
        env_u64("DSH_ANALYZE_MAX_ROWS", "INV_ANALYSIS_MAX_ROWS", 5_000_000) as usize;

    let (_left_meta, left) = load_or_fail(workspace, &left_id)?;
    let (_right_meta, right) = load_or_fail(workspace, &right_id)?;

    let shared: Vec<String> = left
        .columns
        .iter()
        .map(|c| c.name.clone())
        .filter(|n| right.columns.iter().any(|c| &c.name == n))
        .collect();

    let row_obj = |table: &Table, row: &[Value]| -> String {
        let cells = table
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{}:{}", json_string(&c.name), row.get(i).map(|v| v.to_json()).unwrap_or_else(|| "null".into())))
            .collect::<Vec<_>>()
            .join(",");
        format!("{{{cells}}}")
    };

    let mut only_left = 0u64;
    let mut only_right = 0u64;
    let mut changed = 0u64;
    let mut s_left: Vec<String> = Vec::new();
    let mut s_right: Vec<String> = Vec::new();
    let mut s_changed: Vec<String> = Vec::new();
    // Difference rows for the output dataset: (left row, right row) pairs;
    // a None side means the row exists only on the other side.
    let mut diff_rows: Vec<(Option<usize>, Option<usize>)> = Vec::new();

    if let Some(on_raw) = &on_raw {
        let entries: Vec<&str> = on_raw
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        if entries.is_empty() || entries.len() > 8 {
            return fail(2, "analysis/invalid-args: --on must list 1..8 key entries");
        }
        let pairs = parse_on_pairs(on_raw).map_err(|e| {
            eprintln!("{e}");
            2u8
        })?;
        let mut left_key_idx = Vec::with_capacity(pairs.len());
        let mut right_key_idx = Vec::with_capacity(pairs.len());
        for (ln, rn) in &pairs {
            let li = left.columns.iter().position(|c| &c.name == ln).ok_or_else(|| {
                eprintln!("analysis/invalid-args: unknown column {ln} on left");
                2u8
            })?;
            let ri = right.columns.iter().position(|c| &c.name == rn).ok_or_else(|| {
                eprintln!("analysis/invalid-args: unknown column {rn} on right");
                2u8
            })?;
            left_key_idx.push(li);
            right_key_idx.push(ri);
        }
        let key_left_names: Vec<String> = pairs.iter().map(|(ln, _)| ln.clone()).collect();
        let key_right_names: Vec<String> = pairs.iter().map(|(_, rn)| rn.clone()).collect();
        let non_key_shared: Vec<(String, usize, usize)> = shared
            .iter()
            .filter(|n| !key_left_names.contains(n) && !key_right_names.contains(n))
            .map(|n| {
                let li = left.columns.iter().position(|c| &c.name == n).unwrap();
                let ri = right.columns.iter().position(|c| &c.name == n).unwrap();
                (n.clone(), li, ri)
            })
            .collect();

        // Key tuple = JSON cells; null keys never match (join semantics).
        let key_of = |row: &[Value], idxs: &[usize]| -> Option<Vec<String>> {
            let mut out = Vec::with_capacity(idxs.len());
            for &i in idxs {
                let v = row.get(i).cloned().unwrap_or(Value::Null);
                if matches!(v, Value::Null) {
                    return None;
                }
                out.push(v.to_json());
            }
            Some(out)
        };

        let mut right_map: HashMap<Vec<String>, Vec<usize>> = HashMap::new();
        for (ri, row) in right.rows.iter().enumerate() {
            if let Some(k) = key_of(row, &right_key_idx) {
                right_map.entry(k).or_default().push(ri);
            }
        }
        let mut consumed = vec![false; right.rows.len()];
        for (li, lrow) in left.rows.iter().enumerate() {
            let key = key_of(lrow, &left_key_idx);
            let matched = key.and_then(|k| {
                right_map
                    .get_mut(&k)
                    .and_then(|q| if q.is_empty() { None } else { Some(q.remove(0)) })
            });
            let Some(ri) = matched else {
                only_left += 1;
                diff_rows.push((Some(li), None));
                if s_left.len() < sample_limit {
                    s_left.push(row_obj(&left, lrow));
                }
                continue;
            };
            consumed[ri] = true;
            let rrow = &right.rows[ri];
            let mut diff_cols: Vec<String> = Vec::new();
            for (name, lci, rci) in &non_key_shared {
                let lv = lrow.get(*lci).cloned().unwrap_or(Value::Null);
                let rv = rrow.get(*rci).cloned().unwrap_or(Value::Null);
                if lv.to_json() != rv.to_json() {
                    diff_cols.push(name.clone());
                }
            }
            if !diff_cols.is_empty() {
                changed += 1;
                diff_rows.push((Some(li), Some(ri)));
                if s_changed.len() < sample_limit {
                    let key_json = {
                        let cells = pairs
                            .iter()
                            .zip(&left_key_idx)
                            .map(|((ln, _), &ci)| {
                                format!(
                                    "{}:{}",
                                    json_string(ln),
                                    lrow.get(ci).map(|v| v.to_json()).unwrap_or_else(|| "null".into())
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(",");
                        format!("{{{cells}}}")
                    };
                    s_changed.push(format!(
                        "{{\"key\":{},\"diffColumns\":[{}],\"left\":{},\"right\":{}}}",
                        key_json,
                        diff_cols
                            .iter()
                            .map(|c| json_string(c))
                            .collect::<Vec<_>>()
                            .join(","),
                        row_obj(&left, lrow),
                        row_obj(&right, rrow)
                    ));
                }
            }
        }
        for (ri, rrow) in right.rows.iter().enumerate() {
            if consumed[ri] {
                continue;
            }
            only_right += 1;
            diff_rows.push((None, Some(ri)));
            if s_right.len() < sample_limit {
                s_right.push(row_obj(&right, rrow));
            }
        }
    } else {
        // Multiset compare over shared columns; `changed` stays 0.
        let shared_left_idx: Vec<usize> = shared
            .iter()
            .map(|n| left.columns.iter().position(|c| &c.name == n).unwrap())
            .collect();
        let shared_right_idx: Vec<usize> = shared
            .iter()
            .map(|n| right.columns.iter().position(|c| &c.name == n).unwrap())
            .collect();
        let key_of = |row: &[Value], idxs: &[usize]| -> Vec<String> {
            idxs.iter()
                .map(|&i| row.get(i).cloned().unwrap_or(Value::Null).to_json())
                .collect()
        };
        let mut right_map: HashMap<Vec<String>, Vec<usize>> = HashMap::new();
        for (ri, row) in right.rows.iter().enumerate() {
            right_map
                .entry(key_of(row, &shared_right_idx))
                .or_default()
                .push(ri);
        }
        let mut consumed = vec![false; right.rows.len()];
        for (li, lrow) in left.rows.iter().enumerate() {
            let k = key_of(lrow, &shared_left_idx);
            let matched = right_map
                .get_mut(&k)
                .and_then(|q| if q.is_empty() { None } else { Some(q.remove(0)) });
            match matched {
                Some(ri) => consumed[ri] = true,
                None => {
                    only_left += 1;
                    diff_rows.push((Some(li), None));
                    if s_left.len() < sample_limit {
                        s_left.push(row_obj(&left, lrow));
                    }
                }
            }
        }
        for (ri, rrow) in right.rows.iter().enumerate() {
            if consumed[ri] {
                continue;
            }
            only_right += 1;
            diff_rows.push((None, Some(ri)));
            if s_right.len() < sample_limit {
                s_right.push(row_obj(&right, rrow));
            }
        }
    }

    // Output dataset: full-outer-join expansion of every differing row
    // (left columns first, right columns appended, `_r` on name collision).
    let mut out_cols: Vec<Column> = left.columns.clone();
    let mut names: Vec<String> = out_cols.iter().map(|c| c.name.clone()).collect();
    let mut right_slot: Vec<usize> = Vec::new();
    for (i, c) in right.columns.iter().enumerate() {
        let mut name = c.name.clone();
        while names.contains(&name) {
            name = format!("{name}_r");
        }
        names.push(name.clone());
        out_cols.push(Column {
            name,
            dtype: c.dtype.clone(),
        });
        right_slot.push(i);
    }
    if diff_rows.len() > max_rows {
        return fail(3, "analysis/limit: diff result too large");
    }
    let mut out_rows: Vec<Vec<Value>> = Vec::with_capacity(diff_rows.len());
    for (li, ri) in &diff_rows {
        let mut row: Vec<Value> = (0..left.columns.len())
            .map(|ci| {
                li.and_then(|l| left.rows[l].get(ci).cloned())
                    .unwrap_or(Value::Null)
            })
            .collect();
        for &rsi in &right_slot {
            row.push(
                ri.and_then(|r| right.rows[r].get(rsi).cloned())
                    .unwrap_or(Value::Null),
            );
        }
        out_rows.push(row);
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
    let meta = Meta {
        dataset_id: new_id.clone(),
        row_count: result.rows.len(),
        columns: result.columns.clone(),
        source_kind: "diff".into(),
        source_path: format!("diff:{left_id}+{right_id}"),
        label: None,
        produced_by: "diff".into(),
        parent_dataset_id: Some(left_id.clone()),
    };
    save_meta(workspace, &meta).map_err(|e| {
        eprintln!("{e}");
        1u8
    })?;

    let warnings_json = warnings
        .map(|w| format!(",\"warnings\":[{w}]"))
        .unwrap_or_default();
    Ok(format!(
        "{{\"ok\":true,\"datasetId\":{},\"onlyLeft\":{},\"onlyRight\":{},\"changed\":{},\"samples\":{{\"onlyLeft\":[{}],\"onlyRight\":[{}],\"changed\":[{}]}}{warnings_json}}}",
        json_string(&new_id),
        only_left,
        only_right,
        changed,
        s_left.join(","),
        s_right.join(","),
        s_changed.join(",")
    ))
}
