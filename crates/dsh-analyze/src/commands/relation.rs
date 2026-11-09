//! Relation commands: corr / crosstab (v0.1.4).

use std::collections::HashMap;
use std::path::Path;

use crate::cli::{fail, flag_value};
use crate::commands::{json_f64, load_or_fail, parse_column_list};
use crate::dataset::{json_string, Value};

/// JSON label for a crosstab category. Null prints as the contract token
/// `__null__`; a data string `"__null__"` stays a distinct map key and the
/// same JSON label — counts never merge.
fn category_json(v: &Value) -> String {
    match v {
        Value::Null => json_string("__null__"),
        other => other.to_json(),
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
    p_value: Option<f64>,
    degenerate: bool,
}

/// Two-sided p-value for Pearson/Spearman r via Student-t approximation.
/// df = n - 2; t = r * sqrt(df / (1 - r^2)). |r|==1 → p=0 when n>=3;
/// r null or df < 1 → null.
fn p_value_from_r(r: Option<f64>, n: usize) -> Option<f64> {
    let r = r?;
    if n < 3 {
        return None;
    }
    let df = (n - 2) as f64;
    if r.abs() >= 1.0 {
        return Some(0.0);
    }
    let denom = 1.0 - r * r;
    if denom <= 0.0 {
        return Some(0.0);
    }
    let t = r.abs() * (df / denom).sqrt();
    if !t.is_finite() {
        return None;
    }
    // Two-sided p = I_{df/(df+t^2)}(df/2, 1/2)  (regularized incomplete beta).
    let x = df / (df + t * t);
    let p = regularized_beta(x, df / 2.0, 0.5);
    Some(p.clamp(0.0, 1.0))
}

/// Regularized incomplete beta I_x(a,b) via Lentz continued fraction.
fn regularized_beta(x: f64, a: f64, b: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    let front = (a * x.ln() + b * (1.0 - x).ln()
        - (ln_gamma(a) + ln_gamma(b) - ln_gamma(a + b)))
        .exp();
    // Use the standard NR-style branch for numerical stability.
    if x < (a + 1.0) / (a + b + 2.0) {
        front * betacf(a, b, x) / a
    } else {
        1.0 - front * betacf(b, a, 1.0 - x) / b
    }
    .clamp(0.0, 1.0)
}

/// Continued fraction for incomplete beta (Lentz's method).
fn betacf(a: f64, b: f64, x: f64) -> f64 {
    const MAXIT: u32 = 300;
    const EPS: f64 = 3e-14;
    const FPMIN: f64 = 1e-300;
    let qab = a + b;
    let qap = a + 1.0;
    let qam = a - 1.0;
    let mut c = 1.0f64;
    let mut d = 1.0 - qab * x / qap;
    if d.abs() < FPMIN {
        d = FPMIN;
    }
    d = 1.0 / d;
    let mut h = d;
    for m in 1..=MAXIT {
        let m = m as f64;
        // even step
        let m2 = 2.0 * m;
        let aa = m * (b - m) * x / ((qam + m2) * (a + m2));
        d = 1.0 + aa * d;
        if d.abs() < FPMIN {
            d = FPMIN;
        }
        c = 1.0 + aa / c;
        if c.abs() < FPMIN {
            c = FPMIN;
        }
        d = 1.0 / d;
        h *= d * c;
        // odd step
        let aa = -(a + m) * (qab + m) * x / ((a + m2) * (qap + m2));
        d = 1.0 + aa * d;
        if d.abs() < FPMIN {
            d = FPMIN;
        }
        c = 1.0 + aa / c;
        if c.abs() < FPMIN {
            c = FPMIN;
        }
        d = 1.0 / d;
        let del = d * c;
        h *= del;
        if (del - 1.0).abs() < EPS {
            break;
        }
    }
    h
}

/// Lanczos approximation of ln Γ(z) for z > 0.
fn ln_gamma(z: f64) -> f64 {
    const G: [f64; 9] = [
        0.99999999999980993,
        676.5203681218851,
        -1259.1392167224028,
        771.32342877765313,
        -176.61502916214059,
        12.507343278686905,
        -0.13857109526572012,
        9.9843695780195716e-6,
        1.5056327351493116e-7,
    ];
    if z < 0.5 {
        // reflection formula
        return (std::f64::consts::PI / (std::f64::consts::PI * z).sin()).ln() - ln_gamma(1.0 - z);
    }
    let z = z - 1.0;
    let mut x = G[0];
    for (i, g) in G.iter().enumerate().skip(1) {
        x += g / (z + i as f64);
    }
    let t = z + 7.5;
    0.5 * (2.0 * std::f64::consts::PI).ln() + (z + 0.5) * t.ln() - t + x.ln()
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
    // R-34: optional grouping columns for per-group pairwise corr.
    let by_names: Vec<String> = match flag_value(args, "--by") {
        Some(raw) => {
            let names = parse_column_list(&raw);
            if names.is_empty() || names.len() > 4 {
                return fail(2, "analysis/invalid-args: --by must list 1..4 columns");
            }
            names
        }
        None => Vec::new(),
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

    let mut by_idxs = Vec::with_capacity(by_names.len());
    for name in &by_names {
        let idx = table
            .columns
            .iter()
            .position(|c| &c.name == name)
            .ok_or_else(|| {
                eprintln!("analysis/invalid-args: unknown column {name}");
                2u8
            })?;
        by_idxs.push(idx);
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

    let maps: Vec<HashMap<usize, f64>> = series
        .iter()
        .map(|s| s.iter().copied().collect())
        .collect();

    let total_pairs_per_set = columns.len() * (columns.len() - 1) / 2;

    // Group row indices by --by key (first-seen order). Without --by, one group = all rows.
    let mut group_keys: Vec<Vec<Value>> = Vec::new();
    let mut group_rows: Vec<Vec<usize>> = Vec::new();
    if by_idxs.is_empty() {
        let all: Vec<usize> = if let Some(mask) = &listwise_rows {
            let mut rows: Vec<usize> = mask.iter().copied().collect();
            rows.sort_unstable();
            rows
        } else {
            (0..table.rows.len()).collect()
        };
        group_keys.push(Vec::new());
        group_rows.push(all);
    } else {
        let mut key_to_gi: HashMap<Vec<Value>, usize> = HashMap::new();
        for ri in 0..table.rows.len() {
            if let Some(mask) = &listwise_rows {
                if !mask.contains(&ri) {
                    continue;
                }
            }
            let row = &table.rows[ri];
            let key: Vec<Value> = by_idxs
                .iter()
                .map(|&ci| row.get(ci).cloned().unwrap_or(Value::Null).category_key())
                .collect();
            let gi = *key_to_gi.entry(key.clone()).or_insert_with(|| {
                group_keys.push(key.clone());
                group_rows.push(Vec::new());
                group_keys.len() - 1
            });
            group_rows[gi].push(ri);
        }
    }

    // Caps (R-34): group count and total emitted pairs.
    const MAX_BY_GROUPS: usize = 50;
    const MAX_BY_PAIRS: usize = 100;
    let groups_total = group_rows.len();
    let groups_truncated = by_idxs.is_empty() == false && groups_total > MAX_BY_GROUPS;
    let groups_kept = if groups_truncated {
        MAX_BY_GROUPS
    } else {
        groups_total
    };

    #[allow(clippy::type_complexity)]
    let mut all_out: Vec<(usize, Vec<Value>, PairOut)> = Vec::new();
    for (gi, rows) in group_rows.iter().take(groups_kept).enumerate() {
        let row_set: std::collections::HashSet<usize> = rows.iter().copied().collect();
        let mut results: Vec<PairOut> = Vec::with_capacity(total_pairs_per_set);
        for i in 0..columns.len() {
            for j in (i + 1)..columns.len() {
                let mut xs: Vec<f64> = Vec::new();
                let mut ys: Vec<f64> = Vec::new();
                // Stable original-index order within the group.
                let mut ordered: Vec<usize> = rows.clone();
                ordered.sort_unstable();
                for &ri in &ordered {
                    if !row_set.contains(&ri) {
                        continue;
                    }
                    if let (Some(&x), Some(&y)) = (maps[i].get(&ri), maps[j].get(&ri)) {
                        xs.push(x);
                        ys.push(y);
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
                let p_value = p_value_from_r(r, n);
                results.push(PairOut {
                    col_i: i,
                    col_j: j,
                    n,
                    r,
                    p_value,
                    degenerate,
                });
            }
        }
        // Sort within group: |r| desc (nulls last), then n desc, then col order.
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
        let gkey = group_keys.get(gi).cloned().unwrap_or_default();
        for p in results {
            all_out.push((gi, gkey.clone(), p));
        }
    }

    // Without --by: keep the historical single-set cap of 50 pairs.
    // With --by: cap total pairs at MAX_BY_PAIRS (group order stable).
    let pair_budget = if by_idxs.is_empty() { 50 } else { MAX_BY_PAIRS };
    let pairs_total = all_out.len();
    let truncated = groups_truncated || pairs_total > pair_budget;
    all_out.truncate(pair_budget);

    let cols_json = columns
        .iter()
        .map(|c| json_string(c))
        .collect::<Vec<_>>()
        .join(",");

    let by_json = if by_names.is_empty() {
        None
    } else {
        Some(
            by_names
                .iter()
                .map(|c| json_string(c))
                .collect::<Vec<_>>()
                .join(","),
        )
    };

    let pairs_json = all_out
        .iter()
        .map(|(_gi, gkey, p)| {
            let r_s = p.r.map(json_f64).unwrap_or_else(|| "null".into());
            let p_s = p.p_value.map(json_f64).unwrap_or_else(|| "null".into());
            let by_field = if by_names.is_empty() {
                String::new()
            } else {
                let vals = gkey
                    .iter()
                    .map(|v| match v {
                        Value::Null => "null".into(),
                        other => other.to_json(),
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                format!(",\"by\":[{vals}]")
            };
            format!(
                "{{\"columns\":[{},{}],\"n\":{},\"df\":{},\"r\":{},\"pValue\":{},\"degenerate\":{}{by_field}}}",
                json_string(&columns[p.col_i]),
                json_string(&columns[p.col_j]),
                p.n,
                p.n.saturating_sub(2),
                r_s,
                p_s,
                p.degenerate
            )
        })
        .collect::<Vec<_>>()
        .join(",");

    let mut extra = String::new();
    if let Some(by_j) = &by_json {
        extra.push_str(&format!(",\"by\":[{by_j}]"));
        extra.push_str(&format!(",\"groupCount\":{}", groups_total));
        extra.push_str(&format!(",\"groupsEmitted\":{}", groups_kept));
    }
    if groups_truncated {
        extra.push_str(&format!(
            ",\"warnings\":[{{\"code\":\"corr-groups-truncated\",\"emitted\":{groups_kept},\"total\":{groups_total}}}]"
        ));
    } else if pairs_total > pair_budget && by_json.is_some() {
        extra.push_str(&format!(
            ",\"warnings\":[{{\"code\":\"corr-pairs-truncated\",\"emitted\":{},\"total\":{pairs_total}}}]",
            all_out.len()
        ));
    }

    let pair_count_field = if by_idxs.is_empty() {
        total_pairs_per_set
    } else {
        pairs_total.min(pair_budget)
    };

    Ok(format!(
        "{{\"ok\":true,\"datasetId\":{},\"rowCount\":{},\"method\":{},\"pairwise\":{},\"minNumeric\":{},\"columns\":[{}],\"pairCount\":{},\"pairs\":[{}],\"truncated\":{}{extra}}}",
        json_string(&id),
        row_count,
        json_string(&method),
        json_string(&pairwise),
        min_numeric,
        cols_json,
        pair_count_field,
        pairs_json,
        truncated
    ))
}
// ---------------------------------------------------------------------------
// crosstab
// ---------------------------------------------------------------------------

/// Build one 2-D contingency slice over selected rows of `table`.
#[allow(clippy::too_many_arguments)]
fn crosstab_slice(
    table: &crate::dataset::Table,
    ri: usize,
    ci: usize,
    keep: &[usize],
    top: usize,
    min_count: usize,
    drop_null: bool,
) -> (
    Vec<Value>,
    Vec<Value>,
    Vec<Vec<usize>>,
    Vec<usize>,
    Vec<usize>,
    usize,
    usize,
    bool,
    usize,
    Vec<(usize, usize, usize)>,
) {
    let mut cell_counts: HashMap<(Value, Value), usize> = HashMap::new();
    let mut row_marg: HashMap<Value, usize> = HashMap::new();
    let mut col_marg: HashMap<Value, usize> = HashMap::new();
    let mut row_first: HashMap<Value, usize> = HashMap::new();
    let mut col_first: HashMap<Value, usize> = HashMap::new();
    let mut null_row_count = 0usize;
    let mut seq = 0usize;

    for &ri_row in keep {
        let row = &table.rows[ri_row];
        let rv = row.get(ri).cloned().unwrap_or(Value::Null);
        let cv = row.get(ci).cloned().unwrap_or(Value::Null);
        let r_is_null = matches!(rv, Value::Null);
        let c_is_null = matches!(cv, Value::Null);
        if drop_null && (r_is_null || c_is_null) {
            null_row_count += 1;
            continue;
        }
        let r_key = rv.category_key();
        let c_key = cv.category_key();
        *cell_counts.entry((r_key.clone(), c_key.clone())).or_insert(0) += 1;
        *row_marg.entry(r_key.clone()).or_insert(0) += 1;
        *col_marg.entry(c_key.clone()).or_insert(0) += 1;
        row_first.entry(r_key).or_insert(seq);
        col_first.entry(c_key).or_insert(seq);
        seq += 1;
    }

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

    let mut cells: Vec<(usize, usize, usize)> = Vec::new();
    for (i, row) in matrix.iter().enumerate() {
        for (j, &n) in row.iter().enumerate() {
            if n >= min_count && n > 0 {
                cells.push((i, j, n));
            }
        }
    }
    cells.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)).then_with(|| a.1.cmp(&b.1)));
    cells.truncate(50);

    (
        row_cates,
        col_cates,
        matrix,
        row_totals,
        col_totals,
        cell_count,
        null_row_count,
        truncated,
        0,
        cells,
    )
}

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
    let third_name = flag_value(args, "--third").map(|s| s.trim().to_string());
    if let Some(t) = &third_name {
        if t.is_empty() {
            return fail(2, "analysis/invalid-args: --third required");
        }
        if t == rows_name || t == cols_name {
            return fail(
                2,
                "analysis/invalid-args: --third must differ from --rows and --cols",
            );
        }
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

    let resolve = |name: &str| -> Result<usize, u8> {
        table
            .columns
            .iter()
            .position(|c| &c.name == name)
            .ok_or_else(|| {
                eprintln!("analysis/invalid-args: unknown column {name}");
                2u8
            })
    };
    let ri = resolve(rows_name)?;
    let ci = resolve(cols_name)?;
    let ti = match &third_name {
        Some(t) => Some(resolve(t)?),
        None => None,
    };

    let all: Vec<usize> = (0..table.rows.len()).collect();

    // --- No third dimension: legacy 2-D response (v0.1.7 shape) ---
    if ti.is_none() {
        let (
            row_cates,
            col_cates,
            matrix,
            row_totals,
            col_totals,
            cell_count,
            null_row_count,
            truncated,
            _,
            cells,
        ) = crosstab_slice(&table, ri, ci, &all, top, min_count, drop_null);
        let grand_total: usize = row_totals.iter().sum();
        let row_cates_json = row_cates.iter().map(category_json).collect::<Vec<_>>().join(",");
        let col_cates_json = col_cates.iter().map(category_json).collect::<Vec<_>>().join(",");
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
                    category_json(&row_cates[i]),
                    category_json(&col_cates[j]),
                    n
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        return Ok(format!(
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
        ));
    }

    // --- Third dimension: tables[] keyed by third category ---
    let ti = ti.expect("third index");
    // Group row indices by third value (first-seen order of categories by margin).
    let mut third_marg: HashMap<Value, usize> = HashMap::new();
    let mut third_first: HashMap<Value, usize> = HashMap::new();
    let mut third_groups: HashMap<Value, Vec<usize>> = HashMap::new();
    let mut null_third_count = 0usize;
    let mut seq = 0usize;
    for (i, row) in table.rows.iter().enumerate() {
        let tv = row.get(ti).cloned().unwrap_or(Value::Null);
        if drop_null && matches!(tv, Value::Null) {
            null_third_count += 1;
            continue;
        }
        *third_marg.entry(tv.clone()).or_insert(0) += 1;
        third_first.entry(tv.clone()).or_insert(seq);
        third_groups.entry(tv).or_default().push(i);
        seq += 1;
    }
    let mut third_cates: Vec<Value> = third_marg.keys().cloned().collect();
    third_cates.sort_by(|a, b| {
        let ca = third_marg.get(a).copied().unwrap_or(0);
        let cb = third_marg.get(b).copied().unwrap_or(0);
        cb.cmp(&ca).then_with(|| {
            third_first
                .get(a)
                .copied()
                .unwrap_or(usize::MAX)
                .cmp(&third_first.get(b).copied().unwrap_or(usize::MAX))
        })
    });
    let distinct_third = third_cates.len();
    third_cates.truncate(top);
    let third_truncated = distinct_third > top;

    let mut tables_json_parts: Vec<String> = Vec::new();
    for tv in &third_cates {
        let members = third_groups.get(tv).cloned().unwrap_or_default();
        let (
            row_cates,
            col_cates,
            matrix,
            row_totals,
            col_totals,
            cell_count,
            null_row_count,
            truncated,
            _,
            cells,
        ) = crosstab_slice(&table, ri, ci, &members, top, min_count, drop_null);
        let grand_total: usize = row_totals.iter().sum();
        let row_cates_json = row_cates.iter().map(category_json).collect::<Vec<_>>().join(",");
        let col_cates_json = col_cates.iter().map(category_json).collect::<Vec<_>>().join(",");
        let matrix_json = matrix
            .iter()
            .map(|row| {
                let cs = row.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(",");
                format!("[{cs}]")
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
                    category_json(&row_cates[i]),
                    category_json(&col_cates[j]),
                    n
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        tables_json_parts.push(format!(
            "{{\"third\":{},\"rowCategories\":[{}],\"colCategories\":[{}],\"matrix\":[{}],\"rowTotals\":[{}],\"colTotals\":[{}],\"grandTotal\":{},\"cellCount\":{},\"truncated\":{},\"nullRowCount\":{},\"topCells\":[{}]}}",
            category_json(tv),
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
        ));
    }
    let third_cates_json = third_cates.iter().map(category_json).collect::<Vec<_>>().join(",");
    Ok(format!(
        "{{\"ok\":true,\"datasetId\":{},\"rowCount\":{},\"rowColumn\":{},\"colColumn\":{},\"thirdColumn\":{},\"thirdCategories\":[{}],\"tables\":[{}],\"thirdTruncated\":{},\"nullThirdCount\":{}}}",
        json_string(&id),
        row_count,
        json_string(rows_name),
        json_string(cols_name),
        json_string(third_name.as_deref().unwrap_or("")),
        third_cates_json,
        tables_json_parts.join(","),
        third_truncated,
        null_third_count
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// K1: numeric helpers ship with ground-truth assertions, not range checks.

    #[test]
    fn ln_gamma_matches_known_values() {
        // Independent reference values (math.lgamma, double precision).
        for (z, want) in [
            (0.5, 0.5723649429247001),  // ln(sqrt(pi))
            (1.5, -0.1207822376352452), // ln(sqrt(pi)/2)
            (2.0, 0.0),
            (3.5, 1.2009736023470738),
            (10.0, 12.801827480081469),
        ] {
            let got = ln_gamma(z);
            assert!((got - want).abs() < 1e-12, "ln_gamma({z}) = {got}, want {want}");
        }
    }

    #[test]
    fn p_value_df1_closed_form() {
        // For df = 1 (n = 3) the two-sided p-value is exactly (2/pi)*atan(1/|t|),
        // t = r/sqrt(1-r^2). Validates the whole beta pipeline independently.
        for r in [0.1f64, 0.3, 0.5, 0.8, 0.9, 0.99, -0.5, -0.9] {
            let t = r.abs() / (1.0 - r * r).sqrt();
            let want = 2.0 / std::f64::consts::PI * (1.0 / t).atan();
            let got = p_value_from_r(Some(r), 3).expect("p");
            assert!((got - want).abs() < 1e-12, "r={r}: p={got}, want {want}");
        }
    }

    #[test]
    fn p_value_reference_points() {
        // Independent numeric-integration reference (two-sided t tail, df=3).
        let got = p_value_from_r(Some(0.9), 5).expect("p");
        assert!((got - 0.03738571).abs() < 1e-6, "r=0.9 n=5: p={got}");

        let got = p_value_from_r(Some(0.9), 50).expect("p");
        assert!(got < 1e-15, "r=0.9 n=50: p={got}");
    }

    #[test]
    fn p_value_boundaries() {
        assert_eq!(p_value_from_r(None, 10), None);
        assert_eq!(p_value_from_r(Some(0.5), 2), None); // n < 3
        assert_eq!(p_value_from_r(Some(1.0), 5), Some(0.0));
        assert_eq!(p_value_from_r(Some(-1.0), 5), Some(0.0));
        let p0 = p_value_from_r(Some(0.0), 5).expect("p");
        assert!((p0 - 1.0).abs() < 1e-12, "r=0 -> p=1, got {p0}");
        // symmetric in sign(r)
        let a = p_value_from_r(Some(0.7), 12).expect("p");
        let b = p_value_from_r(Some(-0.7), 12).expect("p");
        assert!((a - b).abs() < 1e-15);
    }
}
