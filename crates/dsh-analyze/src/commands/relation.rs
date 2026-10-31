//! Relation commands: corr / crosstab (v0.1.4).

use std::collections::HashMap;
use std::path::Path;

use crate::cli::{fail, flag_value};
use crate::dataset::{json_string, load_dataset, Value};

fn load_or_fail(workspace: &Path, id: &str) -> Result<(crate::dataset::Meta, crate::dataset::Table), u8> {
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
        format!("{v}")
    } else {
        "null".into()
    }
}

/// Pearson correlation on paired samples; (r, degenerate).
fn pearson(xs: &[f64], ys: &[f64]) -> (Option<f64>, bool) {
    debug_assert_eq!(xs.len(), ys.len());
    let n = xs.len();
    if n < 2 {
        return (None, false);
    }
    let mean_x = xs.iter().sum::<f64>() / n as f64;
    let mean_y = ys.iter().sum::<f64>() / n as f64;
    let mut sxx = 0.0f64;
    let mut syy = 0.0f64;
    let mut sxy = 0.0f64;
    for i in 0..n {
        let dx = xs[i] - mean_x;
        let dy = ys[i] - mean_y;
        sxx += dx * dx;
        syy += dy * dy;
        sxy += dx * dy;
    }
    if sxx == 0.0 || syy == 0.0 {
        return (None, true);
    }
    let denom = (sxx * syy).sqrt();
    if denom == 0.0 || !denom.is_finite() {
        return (None, true);
    }
    let r = sxy / denom;
    if !r.is_finite() {
        return (None, true);
    }
    // Clamp numerical noise into [-1, 1].
    let r = r.clamp(-1.0, 1.0);
    (Some(r), false)
}

/// Mid-ranks for values (ties take average of the ranks they span).
fn mid_ranks(values: &[f64]) -> Vec<f64> {
    let n = values.len();
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by(|&a, &b| {
        values[a]
            .partial_cmp(&values[b])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut ranks = vec![0.0f64; n];
    let mut i = 0usize;
    while i < n {
        let mut j = i;
        while j + 1 < n && values[idx[j + 1]] == values[idx[i]] {
            j += 1;
        }
        // ranks are 1-based for i..=j
        let avg = ((i + 1) + (j + 1)) as f64 / 2.0;
        for k in i..=j {
            ranks[idx[k]] = avg;
        }
        i = j + 1;
    }
    ranks
}

struct PairOut {
    col_i: usize,
    col_j: usize,
    n: usize,
    r: Option<f64>,
    degenerate: bool,
}

pub(crate) fn cmd_corr(workspace: &Path, args: &[String]) -> Result<String, u8> {
    let id = match flag_value(args, "--dataset-id") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --dataset-id required"),
    };
    let columns_raw = match flag_value(args, "--columns") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --columns required"),
    };
    let columns = parse_column_list(&columns_raw);
    if columns.len() < 2 || columns.len() > 16 {
        return fail(2, "analysis/invalid-args: --columns must list 2..16 columns");
    }
    {
        let mut seen = std::collections::HashSet::new();
        for c in &columns {
            if !seen.insert(c.clone()) {
                return fail(2, &format!("analysis/invalid-args: duplicate column {c}"));
            }
        }
    }
    let method = flag_value(args, "--method")
        .map(|s| s.to_string())
        .unwrap_or_else(|| "pearson".into());
    if method != "pearson" && method != "spearman" {
        return fail(2, "analysis/invalid-args: --method must be pearson|spearman");
    }
    let pairwise = flag_value(args, "--pairwise")
        .map(|s| s.to_string())
        .unwrap_or_else(|| "complete".into());
    if pairwise != "complete" && pairwise != "omit" {
        return fail(2, "analysis/invalid-args: --pairwise must be complete|omit");
    }
    let min_numeric: usize = match flag_value(args, "--min-numeric") {
        Some(raw) => match raw.parse::<usize>() {
            Ok(n) if n >= 2 => n,
            _ => return fail(2, "analysis/invalid-args: --min-numeric must be integer >= 2"),
        },
        None => 2,
    };

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
            })?;
        col_idxs.push(idx);
    }

    // Extract numeric series (row_index → f64) per column; validate numeric-capable.
    let mut series: Vec<Vec<(usize, f64)>> = Vec::with_capacity(col_idxs.len());
    for (&ci, name) in col_idxs.iter().zip(columns.iter()) {
        let mut s: Vec<(usize, f64)> = Vec::new();
        for (ri, row) in table.rows.iter().enumerate() {
            match row.get(ci) {
                None | Some(Value::Null) => {}
                Some(v) => {
                    if let Some(n) = v.as_f64() {
                        if n.is_finite() {
                            s.push((ri, n));
                        }
                    }
                }
            }
        }
        if s.is_empty() {
            return fail(
                2,
                &format!("analysis/invalid-args: column {name} is not numeric-capable"),
            );
        }
        series.push(s);
    }

    // Optional listwise mask (--pairwise omit): keep only rows present in all columns.
    let listwise_rows: Option<std::collections::HashSet<usize>> = if pairwise == "omit" {
        let mut mask: Option<std::collections::HashSet<usize>> = None;
        for s in &series {
            let set: std::collections::HashSet<usize> = s.iter().map(|(ri, _)| *ri).collect();
            mask = Some(match mask {
                None => set,
                Some(m) => m.intersection(&set).copied().collect(),
            });
        }
        mask
    } else {
        None
    };

    let total_pairs = columns.len() * (columns.len() - 1) / 2;
    // row → numeric value per column (pairs ≤ 50 after cap; maps once).
    let maps: Vec<HashMap<usize, f64>> = series
        .iter()
        .map(|s| s.iter().copied().collect())
        .collect();
    let mut results: Vec<PairOut> = Vec::with_capacity(total_pairs);
    for i in 0..columns.len() {
        for j in (i + 1)..columns.len() {
            let mut xs: Vec<f64> = Vec::new();
            let mut ys: Vec<f64> = Vec::new();
            if let Some(mask) = &listwise_rows {
                // stable row order by original index
                let mut rows: Vec<usize> = mask.iter().copied().collect();
                rows.sort_unstable();
                for &ri in &rows {
                    if let (Some(&x), Some(&y)) = (maps[i].get(&ri), maps[j].get(&ri)) {
                        xs.push(x);
                        ys.push(y);
                    }
                }
            } else {
                // pairwise: iterate the smaller column's row indices
                let smaller = if maps[i].len() <= maps[j].len() {
                    &maps[i]
                } else {
                    &maps[j]
                };
                let mut keys: Vec<usize> = smaller.keys().copied().collect();
                keys.sort_unstable();
                for &ri in &keys {
                    if let (Some(&x), Some(&y)) = (maps[i].get(&ri), maps[j].get(&ri)) {
                        xs.push(x);
                        ys.push(y);
                    }
                }
            }
            let n = xs.len();
            let (r, degenerate) = if n < min_numeric {
                (None, false)
            } else if method == "spearman" {
                let rx = mid_ranks(&xs);
                let ry = mid_ranks(&ys);
                pearson(&rx, &ry)
            } else {
                pearson(&xs, &ys)
            };
            results.push(PairOut {
                col_i: i,
                col_j: j,
                n,
                r,
                degenerate,
            });
        }
    }

    // Sort: |r| desc (nulls last), then n desc, then original (col_i, col_j).
    results.sort_by(|a, b| {
        let ra = a.r.map(|v| v.abs());
        let rb = b.r.map(|v| v.abs());
        match (ra, rb) {
            (Some(x), Some(y)) => y
                .partial_cmp(&x)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.n.cmp(&a.n))
                .then_with(|| a.col_i.cmp(&b.col_i))
                .then_with(|| a.col_j.cmp(&b.col_j)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => b
                .n
                .cmp(&a.n)
                .then_with(|| a.col_i.cmp(&b.col_i))
                .then_with(|| a.col_j.cmp(&b.col_j)),
        }
    });

    let truncated = results.len() > 50;
    results.truncate(50);

    let pairs_json = results
        .iter()
        .map(|p| {
            let r_s = p.r.map(json_f64).unwrap_or_else(|| "null".into());
            format!(
                "{{\"columns\":[{},{}],\"n\":{},\"r\":{},\"degenerate\":{}}}",
                json_string(&columns[p.col_i]),
                json_string(&columns[p.col_j]),
                p.n,
                r_s,
                p.degenerate
            )
        })
        .collect::<Vec<_>>()
        .join(",");

    let cols_json = columns
        .iter()
        .map(|c| json_string(c))
        .collect::<Vec<_>>()
        .join(",");

    Ok(format!(
        "{{\"ok\":true,\"datasetId\":{},\"rowCount\":{},\"method\":{},\"pairwise\":{},\"minNumeric\":{},\"columns\":[{}],\"pairCount\":{},\"pairs\":[{}],\"truncated\":{}}}",
        json_string(&id),
        row_count,
        json_string(&method),
        json_string(&pairwise),
        min_numeric,
        cols_json,
        total_pairs,
        pairs_json,
        truncated
    ))
}

// ---------------------------------------------------------------------------
// crosstab
// ---------------------------------------------------------------------------

pub(crate) fn cmd_crosstab(workspace: &Path, args: &[String]) -> Result<String, u8> {
    let id = match flag_value(args, "--dataset-id") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --dataset-id required"),
    };
    let rows_raw = match flag_value(args, "--rows") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --rows required"),
    };
    let cols_raw = match flag_value(args, "--cols") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --cols required"),
    };
    let rows_name = rows_raw.trim();
    let cols_name = cols_raw.trim();
    if rows_name.is_empty() {
        return fail(2, "analysis/invalid-args: --rows required");
    }
    if cols_name.is_empty() {
        return fail(2, "analysis/invalid-args: --cols required");
    }
    if rows_name == cols_name {
        return fail(
            2,
            "analysis/invalid-args: --rows and --cols must be different columns",
        );
    }
    let top: usize = match flag_value(args, "--top") {
        Some(raw) => match raw.parse::<usize>() {
            Ok(n) if n >= 1 && n <= 50 => n,
            _ => return fail(2, "analysis/invalid-args: --top must be integer 1..=50"),
        },
        None => 20,
    };
    let min_count: usize = match flag_value(args, "--min-count") {
        Some(raw) => match raw.parse::<usize>() {
            Ok(n) if n >= 1 => n,
            _ => return fail(2, "analysis/invalid-args: --min-count must be integer >= 1"),
        },
        None => 1,
    };
    let drop_null = match flag_value(args, "--drop-null") {
        Some("true") | Some("True") | Some("1") => true,
        Some("false") | Some("False") | Some("0") => false,
        Some(_) => return fail(2, "analysis/invalid-args: --drop-null must be true|false"),
        None => true,
    };

    let (meta, table) = load_or_fail(workspace, &id)?;
    let row_count = meta.row_count.max(table.rows.len());

    let ri = table
        .columns
        .iter()
        .position(|c| &c.name == rows_name)
        .ok_or_else(|| {
            eprintln!("analysis/invalid-args: unknown column {rows_name}");
            2u8
        })?;
    let ci = table
        .columns
        .iter()
        .position(|c| &c.name == cols_name)
        .ok_or_else(|| {
            eprintln!("analysis/invalid-args: unknown column {cols_name}");
            2u8
        })?;

    // One pass: count cells + track first-seen order.
    let mut cell_counts: HashMap<(Value, Value), usize> = HashMap::new();
    let mut row_marg: HashMap<Value, usize> = HashMap::new();
    let mut col_marg: HashMap<Value, usize> = HashMap::new();
    let mut row_first: HashMap<Value, usize> = HashMap::new();
    let mut col_first: HashMap<Value, usize> = HashMap::new();
    let mut null_row_count = 0usize;
    let mut seq = 0usize;

    for row in &table.rows {
        let rv = row.get(ri).cloned().unwrap_or(Value::Null);
        let cv = row.get(ci).cloned().unwrap_or(Value::Null);
        let r_is_null = matches!(rv, Value::Null);
        let c_is_null = matches!(cv, Value::Null);
        if drop_null && (r_is_null || c_is_null) {
            null_row_count += 1;
            continue;
        }
        // When drop_null=false, null becomes category `__null__`.
        let r_key = if r_is_null {
            Value::Str("__null__".into())
        } else {
            rv
        };
        let c_key = if c_is_null {
            Value::Str("__null__".into())
        } else {
            cv
        };
        *cell_counts.entry((r_key.clone(), c_key.clone())).or_insert(0) += 1;
        *row_marg.entry(r_key.clone()).or_insert(0) += 1;
        *col_marg.entry(c_key.clone()).or_insert(0) += 1;
        row_first.entry(r_key).or_insert(seq);
        col_first.entry(c_key).or_insert(seq);
        seq += 1;
    }

    // Select top categories per dimension.
    let mut row_cates: Vec<Value> = row_marg.keys().cloned().collect();
    row_cates.sort_by(|a, b| {
        let ca = row_marg.get(a).copied().unwrap_or(0);
        let cb = row_marg.get(b).copied().unwrap_or(0);
        cb.cmp(&ca).then_with(|| {
            row_first
                .get(a)
                .copied()
                .unwrap_or(usize::MAX)
                .cmp(&row_first.get(b).copied().unwrap_or(usize::MAX))
        })
    });
    let distinct_rows = row_cates.len();
    row_cates.truncate(top);

    let mut col_cates: Vec<Value> = col_marg.keys().cloned().collect();
    col_cates.sort_by(|a, b| {
        let ca = col_marg.get(a).copied().unwrap_or(0);
        let cb = col_marg.get(b).copied().unwrap_or(0);
        cb.cmp(&ca).then_with(|| {
            col_first
                .get(a)
                .copied()
                .unwrap_or(usize::MAX)
                .cmp(&col_first.get(b).copied().unwrap_or(usize::MAX))
        })
    });
    let distinct_cols = col_cates.len();
    col_cates.truncate(top);

    let mut truncated = distinct_rows > top || distinct_cols > top;

    // Build matrix over selected categories only.
    let mut matrix: Vec<Vec<usize>> = vec![vec![0usize; col_cates.len()]; row_cates.len()];
    let mut cell_count = 0usize;
    for ((r, c), n) in &cell_counts {
        let (Some(ri2), Some(ci2)) = (
            row_cates.iter().position(|x| x == r),
            col_cates.iter().position(|x| x == c),
        ) else {
            truncated = true;
            continue;
        };
        matrix[ri2][ci2] += n;
        cell_count += n;
    }

    let row_totals: Vec<usize> = matrix.iter().map(|row| row.iter().sum()).collect();
    let col_totals: Vec<usize> = (0..col_cates.len())
        .map(|j| matrix.iter().map(|row| row[j]).sum())
        .collect();
    let grand_total: usize = row_totals.iter().sum();

    // topCells from matrix cells with count >= min_count, cap 50.
    let mut cells: Vec<(usize, usize, usize)> = Vec::new();
    for (i, row) in matrix.iter().enumerate() {
        for (j, &n) in row.iter().enumerate() {
            if n >= min_count && n > 0 {
                cells.push((i, j, n));
            }
        }
    }
    // count desc, then first-seen (i then j preserves scan order for ties after stable sort by count)
    cells.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)).then_with(|| a.1.cmp(&b.1)));
    let top_cells_total = cells.len();
    cells.truncate(50);

    let row_cates_json = row_cates
        .iter()
        .map(|v| v.to_json())
        .collect::<Vec<_>>()
        .join(",");
    let col_cates_json = col_cates
        .iter()
        .map(|v| v.to_json())
        .collect::<Vec<_>>()
        .join(",");
    let matrix_json = matrix
        .iter()
        .map(|row| {
            let cells = row.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(",");
            format!("[{cells}]")
        })
        .collect::<Vec<_>>()
        .join(",");
    let row_totals_json = row_totals
        .iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let col_totals_json = col_totals
        .iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let top_cells_json = cells
        .iter()
        .map(|&(i, j, n)| {
            format!(
                "{{\"row\":{},\"col\":{},\"count\":{}}}",
                row_cates[i].to_json(),
                col_cates[j].to_json(),
                n
            )
        })
        .collect::<Vec<_>>()
        .join(",");

    // topCells cap note: if more cells existed before truncate, still truncated via matrix flags.
    // Surface cell-list truncation as truncated=true only if we want — matrix truncation is primary.
    // Spec: truncated covers category/grid drop. Cell list is a sample of matrix — no extra flag.
    let _ = top_cells_total;

    Ok(format!(
        "{{\"ok\":true,\"datasetId\":{},\"rowCount\":{},\"rowColumn\":{},\"colColumn\":{},\"rowCategories\":[{}],\"colCategories\":[{}],\"matrix\":[{}],\"rowTotals\":[{}],\"colTotals\":[{}],\"grandTotal\":{},\"cellCount\":{},\"truncated\":{},\"nullRowCount\":{},\"topCells\":[{}]}}",
        json_string(&id),
        row_count,
        json_string(rows_name),
        json_string(cols_name),
        row_cates_json,
        col_cates_json,
        matrix_json,
        row_totals_json,
        col_totals_json,
        grand_total,
        cell_count,
        truncated,
        null_row_count,
        top_cells_json
    ))
}
