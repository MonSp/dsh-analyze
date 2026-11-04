//! Declarative agg/filter/select/mutate/time-bucket for v0.2.
//! Pipeline: filter → topKPerGroup → mutate → group/agg → having → select → orderBy → limit.

use std::collections::HashMap;

use crate::dataset::{Column, Table, Value};
use crate::expr::{self, EvalCounters, Expr, ExprType};
use crate::time::{bucket_key, parse_timestamp_utc_date, Period};

#[derive(Debug, Clone)]
pub struct Filter {
    pub op: String,
    pub column: String,
    pub value: Value,
    /// Set members for `op == "in"` (possibly empty); empty for every other op.
    pub members: Vec<Value>,
}

/// filter / having tree: AND-array legacy plus `{all|{any|{not}` groups (R-5).
#[derive(Debug, Clone)]
pub enum FilterNode {
    Pred(Filter),
    All(Vec<FilterNode>),
    Any(Vec<FilterNode>),
    Not(Box<FilterNode>),
}

#[derive(Debug, Clone)]
pub struct Agg {
    pub r#fn: String,
    pub column: Option<String>,
    pub r#as: Option<String>,
    pub p: Option<f64>,
    pub where_clause: Option<FilterNode>,
}

#[derive(Debug, Clone)]
pub struct OrderBy {
    pub column: String,
    pub desc: bool,
}

#[derive(Debug, Clone)]
pub struct Mutate {
    pub r#as: String,
    pub expr: Expr,
}

#[derive(Debug, Clone)]
pub enum SelectEntry {
    Name(String),
    Rename { column: String, as_name: String },
}

impl SelectEntry {
    pub fn source(&self) -> &str {
        match self {
            SelectEntry::Name(n) => n,
            SelectEntry::Rename { column, .. } => column,
        }
    }
    pub fn output(&self) -> &str {
        match self {
            SelectEntry::Name(n) => n,
            SelectEntry::Rename { as_name, .. } => as_name,
        }
    }
}

#[derive(Debug, Clone)]
pub enum GroupKeySpec {
    Column(String),
    Period {
        date_column: String,
        period: Period,
    },
}

#[derive(Debug, Clone)]
pub struct TopKPerGroup {
    pub by: Vec<String>,
    pub k: usize,
    pub order_by: Option<OrderBy>,
}

#[derive(Debug, Clone)]
pub struct Spec {
    pub select: Option<Vec<SelectEntry>>,
    pub filter: Option<FilterNode>,
    pub having: Option<FilterNode>,
    pub mutate: Vec<Mutate>,
    pub group_by: Vec<GroupKeySpec>,
    pub aggs: Vec<Agg>,
    pub order_by: Vec<OrderBy>,
    pub limit: Option<usize>,
    pub top_k_per_group: Option<TopKPerGroup>,
}

/// One `warnings[]` entry (R-20); `message` is omitted in the agg response.
#[derive(Debug, Clone)]
pub struct AggWarning {
    pub code: &'static str,
    pub column: Option<String>,
    pub count: u64,
}

#[derive(Debug)]
pub struct AggOutcome {
    pub table: Table,
    /// Non-null unparseable time cells across all period group columns.
    pub parse_fail_count: u64,
    /// ≤10 distinct unparseable samples in first-seen order.
    pub parse_fail_sample: Vec<String>,
    pub warnings: Vec<AggWarning>,
}

pub fn validate_op(op: &str) -> bool {
    matches!(
        op,
        "eq" | "ne" | "gt" | "gte" | "lt" | "lte" | "in" | "contains" | "is_null" | "not_null"
    )
}

pub fn validate_fn(f: &str) -> bool {
    matches!(
        f,
        "count"
            | "sum"
            | "mean"
            | "min"
            | "max"
            | "median"
            | "std"
            | "count_distinct"
            | "percentile"
            | "first"
            | "last"
            | "count_if"
    )
}

fn period_output_name(date_column: &str, period: &Period) -> String {
    format!("{date_column}_{}", period.as_str())
}

const MAX_FILTER_DEPTH: usize = 8;
const MAX_TOPK_BY: usize = 8;
const MAX_TOPK_K: usize = 1000;
const MAX_PARSE_FAIL_SAMPLES: usize = 10;

pub fn run_agg(
    table: &Table,
    spec: &Spec,
    default_limit: usize,
    max_limit: usize,
) -> Result<AggOutcome, String> {
    let limit = spec.limit.unwrap_or(default_limit).min(max_limit).min(5000);

    let idx = |name: &str| -> Result<usize, String> {
        table
            .columns
            .iter()
            .position(|c| c.name == name)
            .ok_or_else(|| format!("analysis/invalid-args: unknown column {name}"))
    };

    // filter (R-5 tree; array = AND)
    let mut keep: Vec<usize> = Vec::new();
    if let Some(node) = &spec.filter {
        validate_filter_columns(node, &|n| {
            table
                .columns
                .iter()
                .position(|c| c.name == n)
                .map(|_| ())
                .ok_or_else(|| format!("analysis/invalid-args: unknown column {n}"))
        })?;
    }
    for (i, row) in table.rows.iter().enumerate() {
        let ok = match &spec.filter {
            Some(node) => eval_filter(node, row, &|name| idx(name))?,
            None => true,
        };
        if ok {
            keep.push(i);
        }
    }

    // topKPerGroup (R-15): row stage between filter and mutate
    if let Some(tk) = &spec.top_k_per_group {
        for c in &tk.by {
            idx(c)?;
        }
        if let Some(ob) = &tk.order_by {
            idx(&ob.column)?;
        }
        keep = top_k_rows(table, &keep, tk);
    }

    // Working table after filter (clone rows we need for mutate + group).
    let mut work_cols: Vec<Column> = table.columns.clone();
    let mut work_rows: Vec<Vec<Value>> = keep
        .iter()
        .map(|&i| table.rows[i].clone())
        .collect();

    // mutate (derived columns, sequential visibility); static type-check first (R-1)
    let mut name_set: Vec<String> = work_cols.iter().map(|c| c.name.clone()).collect();
    let mut mutate_warnings: Vec<AggWarning> = Vec::new();
    for m in &spec.mutate {
        if name_set.contains(&m.r#as) {
            return Err(format!(
                "analysis/invalid-args: mutate column collides: {}",
                m.r#as
            ));
        }
        let col_types: Vec<(String, String)> = work_cols
            .iter()
            .map(|c| (c.name.clone(), c.dtype.clone()))
            .collect();
        let out_type = expr::check_types(&m.expr, &col_types, &m.r#as)?;
        let col_index: Vec<(String, usize)> = work_cols
            .iter()
            .enumerate()
            .map(|(i, c)| (c.name.clone(), i))
            .collect();
        let mut counters = EvalCounters::default();
        let mut out_vals = Vec::with_capacity(work_rows.len());
        for row in &work_rows {
            out_vals.push(expr::eval_expr(&m.expr, row, &col_index, &mut counters)?);
        }
        if counters.arith_null > 0 {
            mutate_warnings.push(AggWarning {
                code: "mutate-null",
                column: Some(m.r#as.clone()),
                count: counters.arith_null,
            });
        }
        if counters.cast_null > 0 {
            mutate_warnings.push(AggWarning {
                code: "cast-null",
                column: Some(m.r#as.clone()),
                count: counters.cast_null,
            });
        }
        let dtype = match out_type {
            ExprType::Date => "date".to_string(),
            ExprType::String => "utf8".to_string(),
            ExprType::Boolean => "bool".to_string(),
            ExprType::Number => {
                let d = crate::agg::infer_from_values(&out_vals);
                if d == "utf8" {
                    "f64".into()
                } else {
                    d
                }
            }
        };
        work_cols.push(Column {
            name: m.r#as.clone(),
            dtype,
        });
        name_set.push(m.r#as.clone());
        for (row, v) in work_rows.iter_mut().zip(out_vals) {
            row.push(v);
        }
    }

    // No group / no aggs → project (having → select → orderBy → limit)
    if spec.aggs.is_empty() && spec.group_by.is_empty() {
        let work = Table {
            columns: work_cols,
            rows: work_rows,
        };
        let t = project_rows(&work, spec, limit)?;
        return Ok(AggOutcome {
            table: t,
            parse_fail_count: 0,
            parse_fail_sample: Vec::new(),
            warnings: mutate_warnings,
        });
    }

    // Resolve group keys against work_cols
    let mut group_labels: Vec<String> = Vec::new();
    let mut group_resolvers: Vec<GroupResolver> = Vec::new();
    let mut parse_fail_count: u64 = 0;
    let mut parse_fail_sample: Vec<String> = Vec::new();
    for gk in &spec.group_by {
        match gk {
            GroupKeySpec::Column(name) => {
                let ci = work_cols
                    .iter()
                    .position(|c| &c.name == name)
                    .ok_or_else(|| format!("analysis/invalid-args: unknown column {name}"))?;
                group_labels.push(name.clone());
                group_resolvers.push(GroupResolver::Col(ci));
            }
            GroupKeySpec::Period {
                date_column,
                period,
            } => {
                let ci = work_cols
                    .iter()
                    .position(|c| &c.name == date_column)
                    .ok_or_else(|| {
                        format!("analysis/invalid-args: unknown column {date_column}")
                    })?;
                group_labels.push(period_output_name(date_column, period));
                group_resolvers.push(GroupResolver::Time {
                    col: ci,
                    period: period.clone(),
                });
            }
        }
    }

    let mut groups: HashMap<Vec<Value>, Vec<usize>> = HashMap::new();
    for (row_i, row) in work_rows.iter().enumerate() {
        let mut key = Vec::with_capacity(group_resolvers.len());
        for r in &group_resolvers {
            match r {
                GroupResolver::Col(ci) => {
                    key.push(row.get(*ci).cloned().unwrap_or(Value::Null));
                }
                GroupResolver::Time { col, period } => {
                    let cell = row.get(*col).cloned().unwrap_or(Value::Null);
                    let bucket = match &cell {
                        Value::Null => Value::Null,
                        Value::Str(s) => {
                            let t = s.trim();
                            if t.is_empty() {
                                Value::Null
                            } else {
                                match parse_timestamp_utc_date(t) {
                                    Some(p) => Value::Str(bucket_key(p, period)),
                                    None => {
                                        parse_fail_count += 1;
                                        push_parse_sample(&mut parse_fail_sample, t);
                                        Value::Null
                                    }
                                }
                            }
                        }
                        other => {
                            let j = other.to_json();
                            let s = j.trim_matches('"');
                            match parse_timestamp_utc_date(s) {
                                Some(p) => Value::Str(bucket_key(p, period)),
                                None => {
                                    parse_fail_count += 1;
                                    push_parse_sample(&mut parse_fail_sample, s);
                                    Value::Null
                                }
                            }
                        }
                    };
                    key.push(bucket);
                }
            }
        }
        groups.entry(key).or_default().push(row_i);
    }
    // With aggs and no group keys there is exactly one (possibly empty) group.
    if group_resolvers.is_empty() && !spec.aggs.is_empty() {
        groups.entry(Vec::new()).or_default();
    }

    let work = Table {
        columns: work_cols,
        rows: work_rows,
    };

    // Column index helper on work table for aggs
    let widx = |name: &str| -> Result<usize, String> {
        work
            .columns
            .iter()
            .position(|c| c.name == name)
            .ok_or_else(|| format!("analysis/invalid-args: unknown column {name}"))
    };

    let out_names: Vec<String> = if spec.aggs.is_empty() {
        group_labels.clone()
    } else {
        let mut names = group_labels.clone();
        for a in &spec.aggs {
            let name = a.r#as.clone().unwrap_or_else(|| default_agg_name(a));
            names.push(name);
        }
        names
    };

    let mut out_rows: Vec<Vec<Value>> = Vec::new();
    for (key, members) in groups {
        let mut row: Vec<Value> = key.clone();
        for a in &spec.aggs {
            let v = compute_agg(&work, &members, a, &widx)?;
            row.push(v);
        }
        out_rows.push(row);
    }

    // having (R-7): filter result rows (group keys + agg `as`) before select
    if let Some(node) = &spec.having {
        validate_filter_columns(node, &|n| {
            if out_names.iter().any(|c| c == n) {
                Ok(())
            } else {
                Err(format!("analysis/invalid-args: unknown column {n}"))
            }
        })?;
        let mut kept = Vec::with_capacity(out_rows.len());
        for row in out_rows {
            let ok = eval_filter(node, &row, &|name| {
                out_names
                    .iter()
                    .position(|n| n == name)
                    .ok_or_else(|| format!("analysis/invalid-args: unknown column {name}"))
            })?;
            if ok {
                kept.push(row);
            }
        }
        out_rows = kept;
    }

    let (final_names, mut final_rows) = apply_select(&out_names, &out_rows, spec)?;
    apply_order_limit(&final_names, &mut final_rows, spec, limit);

    let mut cols = Vec::new();
    for (i, name) in final_names.iter().enumerate() {
        let col_values: Vec<Value> = final_rows
            .iter()
            .filter_map(|r| r.get(i).cloned())
            .collect();
        let dtype = infer_from_values(&col_values);
        cols.push(Column {
            name: name.clone(),
            dtype,
        });
    }

    let mut warnings = mutate_warnings;
    if parse_fail_count > 0 {
        warnings.push(AggWarning {
            code: "date-parse-fail",
            column: None,
            count: parse_fail_count,
        });
    }

    Ok(AggOutcome {
        table: Table {
            columns: cols,
            rows: final_rows,
        },
        parse_fail_count,
        parse_fail_sample,
        warnings,
    })
}

fn push_parse_sample(samples: &mut Vec<String>, raw: &str) {
    if samples.len() >= MAX_PARSE_FAIL_SAMPLES {
        return;
    }
    let s = raw.to_string();
    if !samples.contains(&s) {
        samples.push(s);
    }
}

/// Keep ≤k rows per distinct `by` combo (Null its own group), ordered by
/// `orderBy` (stable input order when absent). Output preserves input order.
fn top_k_rows(table: &Table, keep: &[usize], tk: &TopKPerGroup) -> Vec<usize> {
    let by_idxs: Vec<usize> = tk
        .by
        .iter()
        .map(|c| {
            table
                .columns
                .iter()
                .position(|col| &col.name == c)
                .unwrap_or(0)
        })
        .collect();
    let ob_idx = tk.order_by.as_ref().map(|ob| {
        table
            .columns
            .iter()
            .position(|col| col.name == ob.column)
            .unwrap_or(0)
    });
    let mut groups: HashMap<Vec<Value>, Vec<usize>> = HashMap::new();
    for &i in keep {
        let key: Vec<Value> = by_idxs
            .iter()
            .map(|&ci| table.rows[i].get(ci).cloned().unwrap_or(Value::Null))
            .collect();
        groups.entry(key).or_default().push(i);
    }
    let mut selected: Vec<usize> = Vec::with_capacity(keep.len());
    for members in groups.values_mut() {
        if let (Some(oi), Some(ob)) = (ob_idx, &tk.order_by) {
            members.sort_by(|&a, &b| {
                let va = table.rows[a].get(oi).unwrap_or(&Value::Null);
                let vb = table.rows[b].get(oi).unwrap_or(&Value::Null);
                let ord = cmp_values(va, vb);
                if ob.desc {
                    ord.reverse()
                } else {
                    ord
                }
            });
        }
        selected.extend(members.iter().take(tk.k));
    }
    selected.sort_unstable();
    selected
}

enum GroupResolver {
    Col(usize),
    Time { col: usize, period: Period },
}

fn default_agg_name(a: &Agg) -> String {
    match &a.column {
        Some(c) => format!("{c}_{}", a.r#fn),
        None => a.r#fn.clone(),
    }
}

/// having → select → orderBy → limit on a non-aggregated work table.
fn project_rows(work: &Table, spec: &Spec, limit: usize) -> Result<Table, String> {
    let all: Vec<String> = work.columns.iter().map(|c| c.name.clone()).collect();
    let mut rows: Vec<Vec<Value>> = work.rows.clone();
    if let Some(node) = &spec.having {
        validate_filter_columns(node, &|n| {
            if all.iter().any(|c| c == n) {
                Ok(())
            } else {
                Err(format!("analysis/invalid-args: unknown column {n}"))
            }
        })?;
        let mut kept = Vec::with_capacity(rows.len());
        for row in rows {
            let ok = eval_filter(node, &row, &|name| {
                work.columns
                    .iter()
                    .position(|c| c.name == name)
                    .ok_or_else(|| format!("analysis/invalid-args: unknown column {name}"))
            })?;
            if ok {
                kept.push(row);
            }
        }
        rows = kept;
    }
    let out_names_owned: Vec<String> = all;
    let (final_names, mut final_rows) = apply_select_on(
        &out_names_owned,
        &rows,
        spec.select.as_deref(),
        &|name| {
            work.columns
                .iter()
                .position(|c| c.name == name)
                .map(|_| ())
                .ok_or_else(|| format!("analysis/invalid-args: unknown column {name}"))
        },
    )?;
    apply_order_limit(&final_names, &mut final_rows, spec, limit);
    let columns = final_names
        .iter()
        .map(|name| {
            work.columns
                .iter()
                .find(|c| &c.name == name)
                .cloned()
                .unwrap_or_else(|| Column {
                    name: name.clone(),
                    dtype: "utf8".into(),
                })
        })
        .collect();
    Ok(Table {
        columns,
        rows: final_rows,
    })
}

fn apply_select(
    out_names: &[String],
    out_rows: &[Vec<Value>],
    spec: &Spec,
) -> Result<(Vec<String>, Vec<Vec<Value>>), String> {
    apply_select_on(out_names, out_rows, spec.select.as_deref(), &|name| {
        out_names
            .iter()
            .position(|n| n == name)
            .map(|_| ())
            .ok_or_else(|| format!("analysis/invalid-args: unknown column {name}"))
    })
}

fn apply_select_on(
    out_names: &[String],
    out_rows: &[Vec<Value>],
    select: Option<&[SelectEntry]>,
    src_ok: &dyn Fn(&str) -> Result<(), String>,
) -> Result<(Vec<String>, Vec<Vec<Value>>), String> {
    let Some(select) = select else {
        return Ok((out_names.to_vec(), out_rows.to_vec()));
    };
    let mut idxs = Vec::new();
    let mut names = Vec::new();
    for entry in select {
        let src = entry.source();
        let i = out_names.iter().position(|n| n == src).ok_or_else(|| {
            format!("analysis/invalid-args: unknown column {src}")
        })?;
        src_ok(src)?;
        let out = entry.output().to_string();
        if names.contains(&out) {
            return Err(format!(
                "analysis/invalid-args: duplicate select name {out}"
            ));
        }
        idxs.push(i);
        names.push(out);
    }
    let rows: Vec<Vec<Value>> = out_rows
        .iter()
        .map(|r| {
            idxs.iter()
                .map(|&i| r.get(i).cloned().unwrap_or(Value::Null))
                .collect()
        })
        .collect();
    Ok((names, rows))
}

fn apply_order_limit(
    final_names: &[String],
    rows: &mut Vec<Vec<Value>>,
    spec: &Spec,
    limit: usize,
) {
    if !spec.order_by.is_empty() {
        let order_idxs: Vec<(usize, bool)> = spec
            .order_by
            .iter()
            .filter_map(|o| {
                final_names
                    .iter()
                    .position(|n| *n == o.column)
                    .map(|i| (i, o.desc))
            })
            .collect();
        rows.sort_by(|a, b| {
            for (i, desc) in &order_idxs {
                let ord = cmp_values(
                    a.get(*i).unwrap_or(&Value::Null),
                    b.get(*i).unwrap_or(&Value::Null),
                );
                let ord = if *desc { ord.reverse() } else { ord };
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            std::cmp::Ordering::Equal
        });
    }
    rows.truncate(limit);
}

/// Walk a filter tree and error on any unknown column before evaluating rows.
fn validate_filter_columns(
    node: &FilterNode,
    check: &dyn Fn(&str) -> Result<(), String>,
) -> Result<(), String> {
    match node {
        FilterNode::Pred(f) => check(&f.column),
        FilterNode::All(items) | FilterNode::Any(items) => {
            for it in items {
                validate_filter_columns(it, check)?;
            }
            Ok(())
        }
        FilterNode::Not(inner) => validate_filter_columns(inner, check),
    }
}

fn eval_filter(
    node: &FilterNode,
    row: &[Value],
    col_of: &dyn Fn(&str) -> Result<usize, String>,
) -> Result<bool, String> {
    match node {
        FilterNode::All(items) => {
            for it in items {
                if !eval_filter(it, row, col_of)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        FilterNode::Any(items) => {
            for it in items {
                if eval_filter(it, row, col_of)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        FilterNode::Not(inner) => Ok(!eval_filter(inner, row, col_of)?),
        FilterNode::Pred(f) => {
            let ci = col_of(&f.column)?;
            let cell = row.get(ci).cloned().unwrap_or(Value::Null);
            Ok(match f.op.as_str() {
                "is_null" => matches!(cell, Value::Null),
                "not_null" => !matches!(cell, Value::Null),
                "eq" => values_eq(&cell, &f.value),
                "ne" => !values_eq(&cell, &f.value),
                "gt" | "gte" | "lt" | "lte" => {
                    let a = cell.as_f64();
                    let b = f.value.as_f64();
                    match (a, b) {
                        (Some(a), Some(b)) => match f.op.as_str() {
                            "gt" => a > b,
                            "gte" => a >= b,
                            "lt" => a < b,
                            _ => a <= b,
                        },
                        _ => false,
                    }
                }
                "in" => f.members.iter().any(|m| values_eq(&cell, m)),
                "contains" => match (&cell, &f.value) {
                    (Value::Str(a), Value::Str(b)) => a.contains(b.as_str()),
                    _ => false,
                },
                other => return Err(format!("analysis/invalid-args: bad op {other}")),
            })
        }
    }
}

fn compute_agg(
    table: &Table,
    members: &[usize],
    a: &Agg,
    idx: &dyn Fn(&str) -> Result<usize, String>,
) -> Result<Value, String> {
    if a.r#fn == "count" {
        return Ok(Value::I64(members.len() as i64));
    }
    if a.r#fn == "count_if" {
        let node = a.where_clause.as_ref().ok_or_else(|| {
            "analysis/invalid-args: count_if requires where".to_string()
        })?;
        validate_filter_columns(node, &|n| {
            idx(n).map(|_| ()).map_err(|_| {
                format!("analysis/invalid-args: unknown column {n}")
            })
        })?;
        let mut n = 0i64;
        for &m in members {
            let row = &table.rows[m];
            if eval_filter(node, row, idx)? {
                n += 1;
            }
        }
        return Ok(Value::I64(n));
    }
    let col = a.column.clone().ok_or_else(|| {
        "analysis/invalid-args: agg column required".to_string()
    })?;
    let ci = idx(&col)?;
    let mut nums: Vec<f64> = Vec::new();
    let mut non_null: Vec<Value> = Vec::new();
    for &m in members {
        let v = table.rows[m].get(ci).cloned().unwrap_or(Value::Null);
        if !matches!(v, Value::Null) {
            non_null.push(v.clone());
        }
        if let Some(n) = v.as_f64() {
            nums.push(n);
        }
    }
    match a.r#fn.as_str() {
        "sum" => Ok(Value::F64(nums.iter().sum())),
        "mean" => {
            if nums.is_empty() {
                Ok(Value::Null)
            } else {
                Ok(Value::F64(nums.iter().sum::<f64>() / nums.len() as f64))
            }
        }
        "min" => Ok(nums
            .iter()
            .copied()
            .fold(None::<f64>, |acc, x| Some(acc.map(|a| a.min(x)).unwrap_or(x)))
            .map(Value::F64)
            .unwrap_or(Value::Null)),
        "max" => Ok(nums
            .iter()
            .copied()
            .fold(None::<f64>, |acc, x| Some(acc.map(|a| a.max(x)).unwrap_or(x)))
            .map(Value::F64)
            .unwrap_or(Value::Null)),
        "median" => {
            if nums.is_empty() {
                return Ok(Value::Null);
            }
            let mut s = nums.clone();
            s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let mid = s.len() / 2;
            if s.len() % 2 == 0 {
                Ok(Value::F64((s[mid - 1] + s[mid]) / 2.0))
            } else {
                Ok(Value::F64(s[mid]))
            }
        }
        "std" => {
            if nums.len() < 2 {
                return Ok(Value::Null);
            }
            let mean = nums.iter().sum::<f64>() / nums.len() as f64;
            let var = nums
                .iter()
                .map(|x| (x - mean).powi(2))
                .sum::<f64>()
                / (nums.len() - 1) as f64;
            Ok(Value::F64(var.sqrt()))
        }
        "count_distinct" => {
            let mut seen: Vec<String> = Vec::new();
            for v in &non_null {
                let k = v.to_json();
                if !seen.contains(&k) {
                    seen.push(k);
                }
            }
            Ok(Value::I64(seen.len() as i64))
        }
        "percentile" => {
            let p = a.p.ok_or("analysis/invalid-args: percentile requires p in (0,1)")?;
            if !(0.0..1.0).contains(&p) {
                return Err("analysis/invalid-args: percentile requires p in (0,1)".into());
            }
            if nums.is_empty() {
                return Ok(Value::Null);
            }
            let mut s = nums.clone();
            s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            Ok(Value::F64(nearest_rank(&s, p)))
        }
        "first" => Ok(non_null.first().cloned().unwrap_or(Value::Null)),
        "last" => Ok(non_null.last().cloned().unwrap_or(Value::Null)),
        other => Err(format!("analysis/invalid-args: bad agg fn {other}")),
    }
}

/// Same nearest-rank percentile as describe: idx = round((n-1)*p) clamped.
fn nearest_rank(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn values_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::I64(x), Value::I64(y)) => x == y,
        (Value::F64(x), Value::F64(y)) => x == y,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Str(x), Value::Str(y)) => x == y,
        _ => match (a.as_f64(), b.as_f64()) {
            (Some(x), Some(y)) => x == y,
            _ => false,
        },
    }
}

fn cmp_values(a: &Value, b: &Value) -> std::cmp::Ordering {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
        _ => a.to_json().cmp(&b.to_json()),
    }
}

fn infer_from_values(values: &[Value]) -> String {
    let mut has_f = false;
    let mut has_i = false;
    let mut has_s = false;
    for v in values {
        match v {
            Value::F64(_) => has_f = true,
            Value::I64(_) => has_i = true,
            Value::Str(_) => has_s = true,
            _ => {}
        }
    }
    if has_s {
        "utf8".into()
    } else if has_f {
        "f64".into()
    } else if has_i {
        "i64".into()
    } else {
        "utf8".into()
    }
}

/// Parse the agg spec JSON (flat-ish object) for v0.2 including mutate + time groupBy.
pub fn parse_spec_json(input: &str) -> Result<Spec, String> {
    let s = input.trim();
    if !s.starts_with('{') || !s.ends_with('}') {
        return Err("analysis/invalid-args: spec must be object".into());
    }
    let keys = top_level_keys(s)?;
    for key in &keys {
        if !matches!(
            key.as_str(),
            "select"
                | "filter"
                | "having"
                | "mutate"
                | "groupBy"
                | "aggs"
                | "orderBy"
                | "limit"
                | "topKPerGroup"
        ) {
            return Err(format!("analysis/invalid-args: unknown spec key {key}"));
        }
    }
    let has = |k: &str| keys.iter().any(|x| x.as_str() == k);
    let mut spec = Spec {
        select: None,
        filter: None,
        having: None,
        mutate: Vec::new(),
        group_by: Vec::new(),
        aggs: Vec::new(),
        order_by: Vec::new(),
        limit: None,
        top_k_per_group: None,
    };

    if let Some(arr) = extract_array(s, "select")? {
        spec.select = Some(parse_select(&arr)?);
    } else if has("select") {
        return Err("analysis/invalid-args: select must be an array".into());
    }
    if let Some(arr) = extract_array(s, "groupBy")? {
        spec.group_by = parse_group_by(&arr)?;
    } else if has("groupBy") {
        return Err("analysis/invalid-args: groupBy must be an array".into());
    }
    if let Some(arr) = extract_array(s, "mutate")? {
        for obj in split_top_objects(&arr)? {
            let as_name = extract_string_field(&obj, "as")?
                .ok_or_else(|| "analysis/invalid-args: mutate as required".to_string())?;
            if !expr::validate_as_name(&as_name) {
                return Err(format!(
                    "analysis/invalid-args: invalid mutate name {as_name}"
                ));
            }
            let expr_src = extract_string_field(&obj, "expr")?
                .ok_or_else(|| "analysis/invalid-args: mutate expr required".to_string())?;
            let parsed = expr::parse_expr(&expr_src)?;
            spec.mutate.push(Mutate {
                r#as: as_name,
                expr: parsed,
            });
        }
    } else if has("mutate") {
        return Err("analysis/invalid-args: mutate must be an array".into());
    }
    if let Some(n) = extract_number(s, "limit")? {
        if n.fract() != 0.0 || !(1.0..=5000.0).contains(&n) {
            return Err("analysis/invalid-args: limit must be an integer 1..5000".into());
        }
        spec.limit = Some(n as usize);
    } else if has("limit") {
        return Err("analysis/invalid-args: limit must be a number".into());
    }
    if let Some(val) = extract_json_value(s, "filter")? {
        spec.filter = Some(parse_filter_node(&val, 0, "filter")?);
    } else if has("filter") {
        return Err(
            "analysis/invalid-args: filter must be an array or {all|any|not} group".into(),
        );
    }
    if let Some(val) = extract_json_value(s, "having")? {
        spec.having = Some(parse_filter_node(&val, 0, "having")?);
    } else if has("having") {
        return Err(
            "analysis/invalid-args: having must be an array or {all|any|not} group".into(),
        );
    }
    if let Some(arr) = extract_array(s, "aggs")? {
        for obj in split_top_objects(&arr)? {
            spec.aggs.push(parse_agg(&obj)?);
        }
    } else if has("aggs") {
        return Err("analysis/invalid-args: aggs must be an array".into());
    }
    if let Some(arr) = extract_array(s, "orderBy")? {
        for obj in split_top_objects(&arr)? {
            spec.order_by.push(parse_order_by(&obj)?);
        }
    } else if has("orderBy") {
        return Err("analysis/invalid-args: orderBy must be an array".into());
    }
    if let Some(val) = extract_json_value(s, "topKPerGroup")? {
        spec.top_k_per_group = Some(parse_top_k(&val)?);
    } else if has("topKPerGroup") {
        return Err("analysis/invalid-args: topKPerGroup must be an object".into());
    }
    Ok(spec)
}

fn parse_order_by(obj: &str) -> Result<OrderBy, String> {
    let column = extract_string_field(obj, "column")?
        .ok_or_else(|| "analysis/invalid-args: orderBy column required".to_string())?;
    let desc = if find_key(obj, "\"desc\"").is_some() {
        extract_bool(obj, "desc")?.ok_or_else(|| {
            "analysis/invalid-args: orderBy desc must be boolean".to_string()
        })?
    } else {
        false
    };
    Ok(OrderBy { column, desc })
}

fn parse_top_k(obj: &str) -> Result<TopKPerGroup, String> {
    let t = obj.trim();
    if !t.starts_with('{') || !t.ends_with('}') {
        return Err("analysis/invalid-args: topKPerGroup must be an object".into());
    }
    let by_arr = extract_array(t, "by")?
        .ok_or_else(|| "analysis/invalid-args: topKPerGroup by required".to_string())?;
    let by = parse_string_array(&by_arr)?;
    if by.is_empty() || by.len() > MAX_TOPK_BY {
        return Err(format!(
            "analysis/invalid-args: topKPerGroup by must list 1..={MAX_TOPK_BY} columns"
        ));
    }
    let k = extract_number(t, "k")?
        .ok_or_else(|| "analysis/invalid-args: topKPerGroup k required".to_string())?;
    if k.fract() != 0.0 || !(1.0..=MAX_TOPK_K as f64).contains(&k) {
        return Err(format!(
            "analysis/invalid-args: topKPerGroup k must be an integer 1..={MAX_TOPK_K}"
        ));
    }
    let order_by = match extract_json_value(t, "orderBy")? {
        Some(ob) => Some(parse_order_by(&ob)?),
        None => None,
    };
    Ok(TopKPerGroup {
        by,
        k: k as usize,
        order_by,
    })
}

fn parse_agg(obj: &str) -> Result<Agg, String> {
    let r#fn = extract_string_field(obj, "fn")?
        .ok_or_else(|| "analysis/invalid-args: agg fn required".to_string())?;
    if !validate_fn(&r#fn) {
        return Err(format!("analysis/invalid-args: bad agg fn {}", r#fn));
    }
    let column = extract_string_field(obj, "column")?;
    let r#as = extract_string_field(obj, "as")?;
    let mut p = None;
    let mut where_clause = None;
    match r#fn.as_str() {
        "percentile" => {
            let v = extract_number(obj, "p")?
                .ok_or("analysis/invalid-args: percentile requires p in (0,1)")?;
            if !(0.0..1.0).contains(&v) {
                return Err("analysis/invalid-args: percentile requires p in (0,1)".into());
            }
            p = Some(v);
        }
        "count_if" => {
            if column.is_some() {
                return Err("analysis/invalid-args: count_if must not take column".into());
            }
            let w = extract_json_value(obj, "where")?
                .ok_or_else(|| "analysis/invalid-args: count_if requires where".to_string())?;
            where_clause = Some(parse_filter_item(&w, 0, "where")?);
        }
        "count_distinct" | "first" | "last" => {
            if column.is_none() {
                return Err("analysis/invalid-args: agg column required".to_string());
            }
        }
        _ => {}
    }
    Ok(Agg {
        r#fn,
        column,
        r#as,
        p,
        where_clause,
    })
}

/// `select` entries: plain names or `{column, as}` renames (R-9).
fn parse_select(arr: &str) -> Result<Vec<SelectEntry>, String> {
    let mut out = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for item in split_top_objects(arr)? {
        let t = item.trim();
        let entry = if t.starts_with('"') {
            let mut items = parse_string_array(&format!("[{t}]"))?;
            let name = items
                .pop()
                .ok_or_else(|| "analysis/invalid-args: empty select entry".to_string())?;
            SelectEntry::Name(name)
        } else if t.starts_with('{') {
            let column = extract_string_field(t, "column")?.ok_or_else(|| {
                "analysis/invalid-args: select column required".to_string()
            })?;
            let as_name = extract_string_field(t, "as")?.ok_or_else(|| {
                "analysis/invalid-args: select as required".to_string()
            })?;
            if !expr::validate_as_name(&as_name) {
                return Err(format!(
                    "analysis/invalid-args: invalid select name {as_name}"
                ));
            }
            SelectEntry::Rename { column, as_name }
        } else {
            return Err("analysis/invalid-args: select entry must be string or object".into());
        };
        let out_name = entry.output().to_string();
        if seen.contains(&out_name) {
            return Err(format!(
                "analysis/invalid-args: duplicate select name {out_name}"
            ));
        }
        seen.push(out_name);
        out.push(entry);
    }
    Ok(out)
}

/// Parse a filter/having/count_if `where` value: AND-array or {all|any|not} group.
fn parse_filter_node(raw: &str, depth: usize, what: &str) -> Result<FilterNode, String> {
    let t = raw.trim();
    if t.starts_with('[') {
        let items = split_top_objects(t)?;
        let mut nodes = Vec::with_capacity(items.len());
        for item in &items {
            nodes.push(parse_filter_item(item, depth, what)?);
        }
        return Ok(FilterNode::All(nodes));
    }
    if t.starts_with('{') {
        return parse_filter_obj(t, depth, what, true);
    }
    Err(format!(
        "analysis/invalid-args: {what} must be an array or {{all|any|not}} group"
    ))
}

/// One item inside a filter array/group: predicate or nested group.
fn parse_filter_item(raw: &str, depth: usize, what: &str) -> Result<FilterNode, String> {
    let t = raw.trim();
    if t.starts_with('{') {
        return parse_filter_obj(t, depth, what, false);
    }
    Err(format!(
        "analysis/invalid-args: {what} item must be a predicate or group"
    ))
}

fn parse_filter_obj(
    t: &str,
    depth: usize,
    what: &str,
    top_level: bool,
) -> Result<FilterNode, String> {
    let has_all = find_key(t, "\"all\"").is_some();
    let has_any = find_key(t, "\"any\"").is_some();
    let has_not = find_key(t, "\"not\"").is_some();
    let n_group = [has_all, has_any, has_not].iter().filter(|x| **x).count();
    if n_group > 1 {
        return Err(format!(
            "analysis/invalid-args: {what} group must be exactly one of all/any/not"
        ));
    }
    if n_group == 1 {
        if depth + 1 > MAX_FILTER_DEPTH {
            return Err("analysis/invalid-args: filter nesting too deep".into());
        }
        if has_all {
            let arr = extract_array(t, "all")?.ok_or_else(|| {
                format!("analysis/invalid-args: {what} all must be an array")
            })?;
            let items = split_top_objects(&arr)?;
            let mut nodes = Vec::with_capacity(items.len());
            for item in &items {
                nodes.push(parse_filter_item(item, depth + 1, what)?);
            }
            return Ok(FilterNode::All(nodes));
        }
        if has_any {
            let arr = extract_array(t, "any")?.ok_or_else(|| {
                format!("analysis/invalid-args: {what} any must be an array")
            })?;
            let items = split_top_objects(&arr)?;
            let mut nodes = Vec::with_capacity(items.len());
            for item in &items {
                nodes.push(parse_filter_item(item, depth + 1, what)?);
            }
            return Ok(FilterNode::Any(nodes));
        }
        let inner = extract_json_value(t, "not")?.ok_or_else(|| {
            format!("analysis/invalid-args: {what} not requires an item")
        })?;
        return Ok(FilterNode::Not(Box::new(parse_filter_item(
            &inner,
            depth + 1,
            what,
        )?)));
    }
    // predicate shape is only valid as a group item; top level must be array or group
    if !top_level
        && extract_string_field(t, "op")?.is_some()
        && extract_string_field(t, "column")?.is_some()
    {
        return Ok(FilterNode::Pred(parse_predicate(t)?));
    }
    if top_level {
        Err(format!(
            "analysis/invalid-args: {what} must be an array or {{all|any|not}} group"
        ))
    } else {
        Err(format!(
            "analysis/invalid-args: {what} item must be a predicate or group"
        ))
    }
}

fn parse_predicate(obj: &str) -> Result<Filter, String> {
    let op = extract_string_field(obj, "op")?
        .ok_or_else(|| "analysis/invalid-args: filter op required".to_string())?;
    let column = extract_string_field(obj, "column")?
        .ok_or_else(|| "analysis/invalid-args: filter column required".to_string())?;
    if !validate_op(&op) {
        return Err(format!("analysis/invalid-args: bad filter op {op}"));
    }
    let mut in_members: Option<Vec<Value>> = None;
    let value = if op == "is_null" || op == "not_null" {
        Value::Null
    } else if let Some(v) = extract_number(obj, "value")? {
        Value::F64(v)
    } else if let Some(v) = extract_bool(obj, "value")? {
        Value::Bool(v)
    } else if let Some(v) = extract_string_field(obj, "value")? {
        Value::Str(v)
    } else if let Some(arr) = extract_array(obj, "value")? {
        if op != "in" {
            return Err(format!(
                "analysis/invalid-args: filter value must not be an array for op {op}"
            ));
        }
        in_members = Some(parse_value_array(&arr)?);
        Value::Null
    } else if find_key(obj, "\"value\"").is_some() {
        return Err("analysis/invalid-args: bad filter value".into());
    } else {
        return Err(format!(
            "analysis/invalid-args: filter value required for op {op}"
        ));
    };
    let members = match in_members {
        Some(m) => m,
        None if op == "in" => vec![value.clone()],
        None => Vec::new(),
    };
    Ok(Filter {
        op,
        column,
        value,
        members,
    })
}

fn parse_group_by(arr: &str) -> Result<Vec<GroupKeySpec>, String> {
    let mut out = Vec::new();
    for obj in split_top_objects(arr)? {
        let t = obj.trim();
        if t.starts_with('"') {
            let mut items = parse_string_array(&format!("[{t}]"))?;
            let name = items
                .pop()
                .ok_or_else(|| "analysis/invalid-args: empty groupBy entry".to_string())?;
            out.push(GroupKeySpec::Column(name));
        } else if t.starts_with('{') {
            let date_column = extract_string_field(t, "dateColumn")?.ok_or_else(|| {
                "analysis/invalid-args: groupBy dateColumn required".to_string()
            })?;
            let period_s = extract_string_field(t, "period")?
                .ok_or_else(|| "analysis/invalid-args: groupBy period required".to_string())?;
            let period = Period::parse(&period_s).ok_or_else(|| {
                format!("analysis/invalid-args: bad period {period_s} (day|week|month)")
            })?;
            out.push(GroupKeySpec::Period {
                date_column,
                period,
            });
        } else {
            return Err("analysis/invalid-args: groupBy entry must be string or object".into());
        }
    }
    Ok(out)
}

fn extract_array(json: &str, key: &str) -> Result<Option<String>, String> {
    let Some(val) = extract_json_value(json, key)? else {
        return Ok(None);
    };
    let t = val.trim_start();
    if t.starts_with('[') {
        Ok(Some(val))
    } else {
        Ok(None)
    }
}

/// Extract the raw JSON value (array / object / scalar) for `key` at top level.
fn extract_json_value(json: &str, key: &str) -> Result<Option<String>, String> {
    let needle = format!("\"{key}\"");
    let Some(idx) = find_key(json, &needle) else {
        return Ok(None);
    };
    let after = json[idx + needle.len()..].trim_start();
    if !after.starts_with(':') {
        return Ok(None);
    }
    let after = after[1..].trim_start();
    if after.starts_with('[') || after.starts_with('{') {
        return Ok(Some(match_brackets(after)?));
    }
    if after.starts_with('"') {
        let mut out = String::from("\"");
        let mut chars = after[1..].chars();
        while let Some(c) = chars.next() {
            out.push(c);
            if c == '\\' {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            } else if c == '"' {
                break;
            }
        }
        return Ok(Some(out));
    }
    let tok: String = after
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != ',' && *c != '}' && *c != ']')
        .collect();
    if tok.is_empty() {
        Ok(None)
    } else {
        Ok(Some(tok))
    }
}

/// String-aware bracket matching for a value starting at `[` or `{`.
fn match_brackets(s: &str) -> Result<String, String> {
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for (i, ch) in s.char_indices() {
        if in_str {
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
            '"' => in_str = true,
            '[' | '{' => depth += 1,
            ']' | '}' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(s[..=i].to_string());
                }
            }
            _ => {}
        }
    }
    Err("analysis/invalid-args: unterminated value".into())
}

/// Lex a JSON-style number at the start of `s` (optional sign, digits,
/// fraction, `e` / `E` exponent). Returns `Ok(None)` when `s` does not start
/// with a number at all, `Err` when a number token is malformed or followed
/// by junk (`2abc`, `2e`) — callers must never silently read a prefix like
/// `2` from `2e1`.
fn lex_number(s: &str) -> Result<Option<(f64, &str)>, String> {
    let tok: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e' | 'E'))
        .collect();
    if tok.is_empty() {
        return Ok(None);
    }
    let after = &s[tok.len()..];
    // The token must end at a value boundary: end of input, whitespace, or a
    // structural character that can follow a value.
    let boundary_ok = match after.chars().next() {
        None => true,
        Some(c) => c.is_whitespace() || matches!(c, ',' | '}' | ']'),
    };
    if !boundary_ok {
        return Err("analysis/invalid-args: bad number".into());
    }
    let n: f64 = tok
        .parse()
        .map_err(|_| format!("analysis/invalid-args: bad number {tok}"))?;
    Ok(Some((n, after)))
}

fn extract_number(json: &str, key: &str) -> Result<Option<f64>, String> {
    let needle = format!("\"{key}\"");
    let Some(idx) = find_key(json, &needle) else {
        return Ok(None);
    };
    let after = json[idx + needle.len()..].trim_start();
    if !after.starts_with(':') {
        return Ok(None);
    }
    let after = after[1..].trim_start();
    Ok(lex_number(after)?.map(|(n, _)| n))
}

fn extract_bool(json: &str, key: &str) -> Result<Option<bool>, String> {
    let needle = format!("\"{key}\"");
    let Some(idx) = find_key(json, &needle) else {
        return Ok(None);
    };
    let after = json[idx + needle.len()..].trim_start();
    if !after.starts_with(':') {
        return Ok(None);
    }
    let after = after[1..].trim_start();
    if after.starts_with("true") {
        return Ok(Some(true));
    }
    if after.starts_with("false") {
        return Ok(Some(false));
    }
    Ok(None)
}

fn extract_string_field(json: &str, key: &str) -> Result<Option<String>, String> {
    let needle = format!("\"{key}\"");
    let Some(idx) = find_key(json, &needle) else {
        return Ok(None);
    };
    let after = json[idx + needle.len()..].trim_start();
    if !after.starts_with(':') {
        return Ok(None);
    }
    let after = after[1..].trim_start();
    if !after.starts_with('"') {
        return Ok(None);
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
    Ok(Some(out))
}

/// Top-level keys of a JSON object; nested keys are ignored (string-aware scan).
fn top_level_keys(json: &str) -> Result<Vec<String>, String> {
    let s = json.trim();
    let Some(inner) = s.strip_prefix('{').and_then(|t| t.strip_suffix('}')) else {
        return Err("analysis/invalid-args: spec must be object".into());
    };
    let mut keys = Vec::new();
    let mut depth = 0usize;
    let mut in_str = false;
    let mut esc = false;
    let mut expect_key = true;
    let mut reading_key = false;
    let mut cur = String::new();
    for ch in inner.chars() {
        if in_str {
            if esc {
                esc = false;
                if reading_key {
                    cur.push(ch);
                }
                continue;
            }
            match ch {
                '\\' => esc = true,
                '"' => {
                    in_str = false;
                    if reading_key {
                        keys.push(std::mem::take(&mut cur));
                        reading_key = false;
                    }
                }
                _ => {
                    if reading_key {
                        cur.push(ch);
                    }
                }
            }
            continue;
        }
        match ch {
            '"' => {
                in_str = true;
                if depth == 0 && expect_key {
                    reading_key = true;
                }
            }
            '{' | '[' => depth += 1,
            '}' | ']' => depth = depth.saturating_sub(1),
            ':' => {
                if depth == 0 {
                    expect_key = false;
                }
            }
            ',' => {
                if depth == 0 {
                    expect_key = true;
                }
            }
            _ => {}
        }
    }
    if in_str {
        return Err("analysis/invalid-args: unterminated string".into());
    }
    Ok(keys)
}

/// String-aware scan of `json[..pos]`: is `pos` a key position directly inside
/// the outermost object (depth 1, not inside any string)?
fn is_top_key_pos(json: &str, pos: usize) -> bool {
    let mut depth = 0usize;
    let mut in_str = false;
    let mut esc = false;
    for ch in json[..pos].chars() {
        if in_str {
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
            '"' => in_str = true,
            '{' | '[' => depth += 1,
            '}' | ']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    !in_str && depth == 1
}

fn find_key(json: &str, quoted_key: &str) -> Option<usize> {
    // Only treat `"key"` as a field when it follows `{` or `,` (object-key
    // position) AND sits directly inside the outermost object, outside any
    // string. Substring matches inside string values (e.g. column name
    // "periods") are skipped, and nested same-name keys (e.g. `limit` inside a
    // filter item) never shadow the top-level field — key order must not
    // change results.
    let mut base = 0usize;
    let mut rest = json;
    while let Some(idx) = rest.find(quoted_key) {
        let abs = base + idx;
        let before = json[..abs].trim_end();
        if (before.ends_with('{') || before.ends_with(',')) && is_top_key_pos(json, abs) {
            return Some(abs);
        }
        base = abs + quoted_key.len();
        rest = &json[base..];
    }
    None
}

fn parse_string_array(arr: &str) -> Result<Vec<String>, String> {
    let inner = arr.trim();
    let inner = inner
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| "analysis/invalid-args: bad array".to_string())?;
    let mut out = Vec::new();
    let mut chars = inner.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() || c == ',' {
            chars.next();
            continue;
        }
        if c == '"' {
            chars.next();
            let mut s = String::new();
            while let Some(ch) = chars.next() {
                if ch == '\\' {
                    // Keep escaped characters (e.g. `\"` / `\\`) intact so a
                    // quote inside a string element does not end it early.
                    if let Some(n) = chars.next() {
                        s.push(n);
                    }
                    continue;
                }
                if ch == '"' {
                    break;
                }
                s.push(ch);
            }
            out.push(s);
        } else {
            while let Some(&ch) = chars.peek() {
                if ch == ',' {
                    break;
                }
                chars.next();
            }
        }
    }
    Ok(out)
}

/// Parse a JSON array of scalar filter values (string / number / bool) for
/// `op: "in"`. Non-scalar elements are rejected instead of silently dropped.
fn parse_value_array(arr: &str) -> Result<Vec<Value>, String> {
    let inner = arr
        .trim()
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| "analysis/invalid-args: bad array".to_string())?;
    let mut out = Vec::new();
    let mut rest = inner;
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        if let Some(t) = rest.strip_prefix(',') {
            rest = t;
            continue;
        }
        if rest.starts_with('"') {
            let mut s = String::new();
            let mut closed = false;
            let mut esc = false;
            let mut consumed = 1usize;
            for (i, ch) in rest[1..].char_indices() {
                let abs = 1 + i;
                if esc {
                    s.push(ch);
                    esc = false;
                    consumed = abs + ch.len_utf8();
                    continue;
                }
                match ch {
                    '\\' => {
                        esc = true;
                        consumed = abs + 1;
                    }
                    '"' => {
                        closed = true;
                        consumed = abs + 1;
                        break;
                    }
                    _ => {
                        s.push(ch);
                        consumed = abs + ch.len_utf8();
                    }
                }
            }
            if !closed {
                return Err("analysis/invalid-args: unterminated string".into());
            }
            out.push(Value::Str(s));
            rest = &rest[consumed..];
            continue;
        }
        if rest.starts_with("true") {
            out.push(Value::Bool(true));
            rest = &rest[4..];
            continue;
        }
        if rest.starts_with("false") {
            out.push(Value::Bool(false));
            rest = &rest[5..];
            continue;
        }
        match lex_number(rest)? {
            Some((n, after)) => {
                out.push(Value::F64(n));
                rest = after;
            }
            None => return Err("analysis/invalid-args: bad filter value".into()),
        }
    }
    Ok(out)
}

fn split_top_objects(arr: &str) -> Result<Vec<String>, String> {
    let inner = arr
        .trim()
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| "analysis/invalid-args: bad object array".to_string())?;
    let mut out = Vec::new();
    let mut depth = 0;
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
            '{' | '[' => {
                depth += 1;
                cur.push(ch);
            }
            '}' | ']' => {
                depth -= 1;
                cur.push(ch);
            }
            ',' if depth == 0 => {
                let t = cur.trim().to_string();
                if !t.is_empty() {
                    out.push(t);
                }
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    let t = cur.trim().to_string();
    if !t.is_empty() {
        out.push(t);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> Table {
        Table {
            columns: vec![
                Column { name: "hits".into(), dtype: "i64".into() },
                Column { name: "total".into(), dtype: "i64".into() },
                Column { name: "region".into(), dtype: "utf8".into() },
            ],
            rows: vec![
                vec![Value::I64(1), Value::I64(4), Value::Str("A".into())],
                vec![Value::I64(3), Value::I64(4), Value::Str("B".into())],
                vec![Value::I64(2), Value::I64(0), Value::Str("A".into())],
            ],
        }
    }

    fn filter_columns(node: &FilterNode, out: &mut Vec<String>) {
        match node {
            FilterNode::Pred(f) => out.push(f.column.clone()),
            FilterNode::All(items) | FilterNode::Any(items) => {
                for it in items {
                    filter_columns(it, out);
                }
            }
            FilterNode::Not(inner) => filter_columns(inner, out),
        }
    }

    #[test]
    fn mutate_then_group() {
        let spec = parse_spec_json(
            r#"{"mutate":[{"as":"rate","expr":"hits / total"}],"groupBy":["region"],"aggs":[{"fn":"mean","column":"rate","as":"avg_rate"}]}"#,
        )
        .unwrap();
        let out = run_agg(&table(), &spec, 1000, 5000).unwrap();
        assert_eq!(out.table.rows.len(), 2);
        assert!(out.table.columns.iter().any(|c| c.name == "avg_rate"));
    }

    #[test]
    fn bad_expr_rejected() {
        assert!(parse_spec_json(r#"{"mutate":[{"as":"x","expr":"hits + total()"}]}"#).is_err());
        assert!(parse_spec_json(r#"{"mutate":[{"as":"x","expr":"system(1)"}]}"#).is_err());
        let spec = parse_spec_json(r#"{"mutate":[{"as":"rate","expr":"missing / total"}]}"#).unwrap();
        assert!(run_agg(&table(), &spec, 1000, 5000).is_err());
    }

    /// Column / mutate names equal to field names must not shadow real keys.
    #[test]
    fn field_name_collision_in_spec_objects() {
        // mutate as named "expr" next to expr key — as must still be "expr".
        let spec = parse_spec_json(
            r#"{"mutate":[{"as":"expr","expr":"hits + total"}],"aggs":[{"fn":"sum","column":"expr","as":"s"}]}"#,
        )
        .unwrap();
        assert_eq!(spec.mutate.len(), 1);
        assert_eq!(spec.mutate[0].r#as, "expr");
        // filter column named "op" must not be read as the op field.
        let spec = parse_spec_json(r#"{"filter":[{"op":"eq","column":"op","value":1}]}"#).unwrap();
        let mut cols = Vec::new();
        filter_columns(spec.filter.as_ref().unwrap(), &mut cols);
        assert_eq!(cols, vec!["op".to_string()]);
    }

    /// filter accepts an AND-array or {all|any|not} groups; other shapes error.
    #[test]
    fn filter_shapes() {
        let err =
            parse_spec_json(r#"{"filter":{"column":"amount","op":"gt","value":3000}}"#).unwrap_err();
        assert!(
            err.contains("filter must be an array or {all|any|not} group"),
            "err={err}"
        );
        assert!(parse_spec_json(r#"{"filter":"amount > 3000"}"#).is_err());
        assert!(parse_spec_json(r#"{"filter":{}}"#).is_err());
        assert!(parse_spec_json(r#"{"filter":[{"op":"eq","column":"a","value":1}]}"#).is_ok());
        assert!(parse_spec_json(r#"{"filter":{"any":[{"op":"eq","column":"a","value":1}]}}"#).is_ok());
        assert!(parse_spec_json(r#"{"filter":{"all":[]}}"#).is_ok());
    }

    #[test]
    fn array_keys_reject_wrong_shape() {
        assert!(parse_spec_json(r#"{"select":"a,b"}"#).is_err());
        assert!(parse_spec_json(r#"{"aggs":{}}"#).is_err());
        assert!(parse_spec_json(r#"{"orderBy":"a"}"#).is_err());
        assert!(parse_spec_json(r#"{"orderBy":[{"column":"a"}]}"#).is_ok());
    }

    /// Unknown top-level keys must be rejected instead of silently dropped.
    #[test]
    fn unknown_spec_key_rejected() {
        let err = parse_spec_json(r#"{"filters":[]}"#).unwrap_err();
        assert!(err.contains("unknown spec key"));
        assert!(parse_spec_json(r#"{"select":["a"],"extra":1}"#).is_err());
        // nested non-key strings are not top-level keys
        assert!(
            parse_spec_json(r#"{"filter":[{"op":"eq","column":"unknown","value":1}]}"#).is_ok()
        );
    }

    #[test]
    fn limit_must_be_number() {
        assert!(parse_spec_json(r#"{"limit":100}"#).is_ok());
        assert!(parse_spec_json(r#"{"limit":5000}"#).is_ok());
        let err = parse_spec_json(r#"{"limit":"100"}"#).unwrap_err();
        assert!(err.contains("limit must be a number"));
    }

    /// limit outside 1..5000 (or fractional) is rejected at parse time,
    /// not silently clamped to the row count.
    #[test]
    fn limit_range_enforced() {
        for spec in [
            r#"{"limit":0}"#,
            r#"{"limit":-5}"#,
            r#"{"limit":5001}"#,
            r#"{"limit":1.5}"#,
        ] {
            let err = parse_spec_json(spec).unwrap_err();
            assert!(
                err.contains("limit must be an integer 1..5000"),
                "spec={spec} err={err}"
            );
        }
    }

    /// Nested same-name keys must never shadow top-level fields: key order in
    /// the spec JSON must not change the result.
    #[test]
    fn nested_keys_never_shadow_top_level() {
        for spec in [
            r#"{"filter":[{"op":"gt","column":"amount","value":0,"limit":2}],"limit":1}"#,
            r#"{"limit":1,"filter":[{"op":"gt","column":"amount","value":0,"limit":2}]}"#,
        ] {
            let parsed = parse_spec_json(spec).unwrap();
            assert_eq!(parsed.limit, Some(1), "spec={spec}");
        }
        // nested `filter` inside a mutate item is not the spec filter
        let parsed = parse_spec_json(
            r#"{"mutate":[{"as":"n","expr":"hits + total","filter":[]}],"filter":[{"op":"eq","column":"region","value":"A"}]}"#,
        )
        .unwrap();
        assert!(parsed.filter.is_some());
        let mut cols = Vec::new();
        filter_columns(parsed.filter.as_ref().unwrap(), &mut cols);
        assert_eq!(cols, vec!["region".to_string()]);
        // nested `select` inside a mutate item is not the spec select
        let parsed = parse_spec_json(
            r#"{"mutate":[{"as":"n","expr":"hits + total","select":[]}],"select":["region"]}"#,
        )
        .unwrap();
        assert_eq!(
            parsed.select.as_deref().map(|s| s[0].output()),
            Some("region")
        );
    }

    /// `in` matches every listed member (OR), not just the first (PR #6 review item).
    #[test]
    fn filter_in_matches_all_members() {
        // all three rows: regions A and B are both members
        let spec =
            parse_spec_json(r#"{"filter":[{"op":"in","column":"region","value":["A","B"]}]}"#)
                .unwrap();
        assert_eq!(run_agg(&table(), &spec, 1000, 5000).unwrap().table.rows.len(), 3);
        // numeric members: hits 1 and 3 match, hits 2 does not
        let spec = parse_spec_json(r#"{"filter":[{"op":"in","column":"hits","value":[1,3]}]}"#)
            .unwrap();
        assert_eq!(run_agg(&table(), &spec, 1000, 5000).unwrap().table.rows.len(), 2);
        // scalar form is a one-member set
        let spec =
            parse_spec_json(r#"{"filter":[{"op":"in","column":"region","value":"B"}]}"#).unwrap();
        assert_eq!(run_agg(&table(), &spec, 1000, 5000).unwrap().table.rows.len(), 1);
        // empty member set matches nothing (literal empty set, not a null match)
        let spec =
            parse_spec_json(r#"{"filter":[{"op":"in","column":"region","value":[]}]}"#).unwrap();
        assert_eq!(run_agg(&table(), &spec, 1000, 5000).unwrap().table.rows.len(), 0);
    }

    /// filter value shapes: arrays only for `in`, value required for the other
    /// ops, non-scalar values rejected — never a silent default.
    #[test]
    fn filter_value_shapes_enforced() {
        let err =
            parse_spec_json(r#"{"filter":[{"op":"eq","column":"a","value":[1,2]}]}"#).unwrap_err();
        assert!(err.contains("filter value must not be an array"));
        let err = parse_spec_json(r#"{"filter":[{"op":"eq","column":"a"}]}"#).unwrap_err();
        assert!(err.contains("filter value required for op eq"));
        let err =
            parse_spec_json(r#"{"filter":[{"op":"eq","column":"a","value":{"x":1}}]}"#).unwrap_err();
        assert!(err.contains("bad filter value"));
        let err =
            parse_spec_json(r#"{"filter":[{"op":"in","column":"a","value":[1,null]}]}"#)
                .unwrap_err();
        assert!(err.contains("bad filter value"));
        // is_null / not_null need no value
        assert!(parse_spec_json(r#"{"filter":[{"op":"is_null","column":"a"}]}"#).is_ok());
        assert!(parse_spec_json(r#"{"filter":[{"op":"not_null","column":"a"}]}"#).is_ok());
    }

    /// Bracket matching is string-aware: `[` / `]` inside string elements must
    /// not truncate the array (PR #6 review item).
    #[test]
    fn extract_array_string_brackets() {
        let spec = parse_spec_json(r#"{"select":["A]B","C"]}"#).unwrap();
        let sel = spec.select.unwrap();
        assert_eq!(sel.len(), 2);
        assert_eq!(sel[0].output(), "A]B");
        assert_eq!(sel[1].output(), "C");
        let spec =
            parse_spec_json(r#"{"filter":[{"op":"in","column":"a","value":["x]y","z"]}]}"#)
                .unwrap();
        let FilterNode::All(items) = spec.filter.as_ref().unwrap() else {
            panic!("and")
        };
        let FilterNode::Pred(pred) = &items[0] else {
            panic!("pred")
        };
        assert_eq!(pred.members, vec![Value::Str("x]y".into()), Value::Str("z".into())]);
    }

    /// One number lexer for every path: exponents parse as JSON numbers
    /// everywhere, trailing junk is rejected — never a silent prefix.
    #[test]
    fn number_lexing_consistent() {
        assert_eq!(parse_spec_json(r#"{"limit":2e1}"#).unwrap().limit, Some(20));
        assert_eq!(parse_spec_json(r#"{"limit":1e3}"#).unwrap().limit, Some(1000));
        let err = parse_spec_json(r#"{"limit":2abc}"#).unwrap_err();
        assert!(err.contains("bad number"), "err={err}");
        let err = parse_spec_json(r#"{"limit":2e}"#).unwrap_err();
        assert!(err.contains("bad number"), "err={err}");

        let spec =
            parse_spec_json(r#"{"filter":[{"op":"gt","column":"amount","value":3e3}]}"#).unwrap();
        let FilterNode::All(items) = spec.filter.as_ref().unwrap() else {
            panic!()
        };
        let FilterNode::Pred(p) = &items[0] else {
            panic!()
        };
        assert_eq!(p.value, Value::F64(3000.0));

        // scalar and array members of `in` read the same literal identically
        let spec =
            parse_spec_json(r#"{"filter":[{"op":"in","column":"amount","value":2e3}]}"#).unwrap();
        let FilterNode::All(items) = spec.filter.as_ref().unwrap() else {
            panic!()
        };
        let FilterNode::Pred(p) = &items[0] else {
            panic!()
        };
        assert_eq!(p.members, vec![Value::F64(2000.0)]);
        let spec =
            parse_spec_json(r#"{"filter":[{"op":"in","column":"amount","value":[2e3]}]}"#)
                .unwrap();
        let FilterNode::All(items) = spec.filter.as_ref().unwrap() else {
            panic!()
        };
        let FilterNode::Pred(p) = &items[0] else {
            panic!()
        };
        assert_eq!(p.members, vec![Value::F64(2000.0)]);
    }

    /// Escaped quotes inside item strings must not swallow later items or
    /// truncate string-array elements.
    #[test]
    fn escaped_quotes_in_item_strings() {
        let spec = parse_spec_json(
            r#"{"filter":[{"op":"ne","column":"amount","value":"x\"y"},{"op":"gt","column":"amount","value":2000}]}"#,
        )
        .unwrap();
        let FilterNode::All(items) = spec.filter.as_ref().unwrap() else {
            panic!("and")
        };
        assert_eq!(items.len(), 2);
        let FilterNode::Pred(p) = &items[0] else { panic!() };
        assert_eq!(p.value, Value::Str("x\"y".into()));
        let FilterNode::Pred(p) = &items[1] else { panic!() };
        assert_eq!(p.op, "gt");
        assert_eq!(p.value, Value::F64(2000.0));
        // select elements keep escaped quotes intact
        let spec = parse_spec_json(r#"{"select":["a\"b"]}"#).unwrap();
        assert_eq!(spec.select.as_deref().map(|s| s[0].output()), Some("a\"b"));
    }

    /// `desc` must be a boolean when present (same text as the tool layer).
    #[test]
    fn order_by_desc_strict() {
        assert!(parse_spec_json(r#"{"orderBy":[{"column":"a","desc":true}]}"#).is_ok());
        assert!(parse_spec_json(r#"{"orderBy":[{"column":"a"}]}"#).is_ok());
        let err =
            parse_spec_json(r#"{"orderBy":[{"column":"a","desc":"true"}]}"#).unwrap_err();
        assert!(err.contains("orderBy desc must be boolean"), "err={err}");
    }

    #[test]
    fn time_bucket_day() {
        let t = Table {
            columns: vec![
                Column { name: "created_at".into(), dtype: "utf8".into() },
                Column { name: "n".into(), dtype: "i64".into() },
            ],
            rows: vec![
                vec![Value::Str("2024-01-31".into()), Value::I64(1)],
                vec![Value::Str("2024-01-31T10:00:00".into()), Value::I64(2)],
                vec![Value::Str("not-a-date".into()), Value::I64(3)],
                vec![Value::Null, Value::I64(4)],
            ],
        };
        let spec = parse_spec_json(
            r#"{"groupBy":[{"dateColumn":"created_at","period":"day"}],"aggs":[{"fn":"count","as":"n"}]}"#,
        )
        .unwrap();
        let out = run_agg(&t, &spec, 1000, 5000).unwrap();
        assert_eq!(out.parse_fail_count, 1);
        assert_eq!(out.parse_fail_sample, vec!["not-a-date".to_string()]);
        // null bucket + one day bucket
        assert_eq!(out.table.rows.len(), 2);
        assert!(out.table.columns.iter().any(|c| c.name == "created_at_day"));
    }

    #[test]
    fn filter_groups_eval() {
        // OR: region A or hits=3 → all 3 rows
        let spec = parse_spec_json(
            r#"{"filter":{"any":[{"op":"eq","column":"region","value":"A"},{"op":"eq","column":"hits","value":3}]},"aggs":[{"fn":"count","as":"n"}]}"#,
        )
        .unwrap();
        let out = run_agg(&table(), &spec, 1000, 5000).unwrap();
        assert_eq!(out.table.rows[0][0], Value::I64(3));
        // NOT: not region B → 2 rows
        let spec = parse_spec_json(
            r#"{"filter":{"not":{"column":"region","op":"eq","value":"B"}},"aggs":[{"fn":"count","as":"n"}]}"#,
        )
        .unwrap();
        let out = run_agg(&table(), &spec, 1000, 5000).unwrap();
        assert_eq!(out.table.rows[0][0], Value::I64(2));
        // empty all = true, empty any = false
        let spec =
            parse_spec_json(r#"{"filter":{"all":[]},"aggs":[{"fn":"count","as":"n"}]}"#).unwrap();
        let out = run_agg(&table(), &spec, 1000, 5000).unwrap();
        assert_eq!(out.table.rows[0][0], Value::I64(3));
        let spec =
            parse_spec_json(r#"{"filter":{"any":[]},"aggs":[{"fn":"count","as":"n"}]}"#).unwrap();
        let out = run_agg(&table(), &spec, 1000, 5000).unwrap();
        assert_eq!(out.table.rows[0][0], Value::I64(0));
    }

    #[test]
    fn filter_nesting_depth() {
        // 8 levels of not/all is fine
        let mut spec = String::from(r#"{"filter":"#);
        let mut core = r#"{"column":"hits","op":"gt","value":0}"#.to_string();
        for _ in 0..7 {
            core = format!(r#"{{"not":{core}}}"#);
        }
        spec.push_str(&core);
        spec.push('}');
        assert!(parse_spec_json(&spec).is_ok());
        // 9 levels errors (depth ≤8 allowed)
        let mut spec = String::from(r#"{"filter":"#);
        let mut core = r#"{"column":"hits","op":"gt","value":0}"#.to_string();
        for _ in 0..9 {
            core = format!(r#"{{"not":{core}}}"#);
        }
        spec.push_str(&core);
        spec.push('}');
        let err = parse_spec_json(&spec).unwrap_err();
        assert!(err.contains("filter nesting too deep"), "err={err}");
    }

    #[test]
    fn having_filters_result_rows() {
        let spec = parse_spec_json(
            r#"{"groupBy":["region"],"aggs":[{"fn":"count","as":"n"}],"having":[{"column":"n","op":"gt","value":1}],"orderBy":[{"column":"region"}]}"#,
        )
        .unwrap();
        let out = run_agg(&table(), &spec, 1000, 5000).unwrap();
        assert_eq!(out.table.rows.len(), 1);
        assert_eq!(out.table.rows[0][0], Value::Str("A".into()));
        // unknown having column errors
        let spec = parse_spec_json(
            r#"{"groupBy":["region"],"aggs":[{"fn":"count","as":"n"}],"having":[{"column":"nope","op":"gt","value":1}]}"#,
        )
        .unwrap();
        let err = run_agg(&table(), &spec, 1000, 5000).unwrap_err();
        assert!(err.contains("unknown column nope"), "err={err}");
    }

    #[test]
    fn new_agg_fns() {
        let spec = parse_spec_json(
            r#"{"aggs":[{"fn":"count_distinct","column":"region","as":"nr"},{"fn":"first","column":"region","as":"f"},{"fn":"last","column":"region","as":"l"}]}"#,
        )
        .unwrap();
        let out = run_agg(&table(), &spec, 1000, 5000).unwrap();
        let row = &out.table.rows[0];
        assert_eq!(row[0], Value::I64(2)); // A,B
        assert_eq!(row[1], Value::Str("A".into()));
        assert_eq!(row[2], Value::Str("A".into())); // last non-null is A (row 3)

        let spec = parse_spec_json(
            r#"{"aggs":[{"fn":"percentile","column":"hits","p":0.5,"as":"p50"}]}"#,
        )
        .unwrap();
        let out = run_agg(&table(), &spec, 1000, 5000).unwrap();
        assert_eq!(out.table.rows[0][0], Value::F64(2.0)); // sorted 1,2,3 → idx 1

        let spec = parse_spec_json(
            r#"{"aggs":[{"fn":"count_if","where":{"column":"hits","op":"gt","value":1},"as":"big"}]}"#,
        )
        .unwrap();
        let out = run_agg(&table(), &spec, 1000, 5000).unwrap();
        assert_eq!(out.table.rows[0][0], Value::I64(2));

        // percentile without p rejected
        let err = parse_spec_json(r#"{"aggs":[{"fn":"percentile","column":"hits","as":"p"}]}"#)
            .unwrap_err();
        assert!(err.contains("percentile requires p in (0,1)"), "err={err}");
        // p out of range rejected
        let err =
            parse_spec_json(r#"{"aggs":[{"fn":"percentile","column":"hits","p":1,"as":"p"}]}"#)
                .unwrap_err();
        assert!(err.contains("percentile requires p in (0,1)"), "err={err}");
        // count_if must not take column
        let err = parse_spec_json(
            r#"{"aggs":[{"fn":"count_if","column":"hits","where":{"column":"hits","op":"gt","value":1},"as":"c"}]}"#,
        )
        .unwrap_err();
        assert!(err.contains("count_if must not take column"), "err={err}");
    }

    #[test]
    fn select_rename_and_duplicates() {
        let spec = parse_spec_json(
            r#"{"select":[{"column":"region","as":"r"}],"aggs":[{"fn":"count","as":"n"}],"groupBy":["region"]}"#,
        )
        .unwrap();
        let out = run_agg(&table(), &spec, 1000, 5000).unwrap();
        assert_eq!(out.table.columns[0].name, "r");
        let err = parse_spec_json(
            r#"{"select":[{"column":"region","as":"x"},{"column":"hits","as":"x"}]}"#,
        )
        .unwrap_err();
        assert!(err.contains("duplicate select name"), "err={err}");
    }

    #[test]
    fn top_k_per_group() {
        let spec = parse_spec_json(
            r#"{"topKPerGroup":{"by":["region"],"k":1,"orderBy":{"column":"hits","desc":true}},"select":["region","hits"],"orderBy":[{"column":"region"}]}"#,
        )
        .unwrap();
        let out = run_agg(&table(), &spec, 1000, 5000).unwrap();
        assert_eq!(out.table.rows.len(), 2);
        // per region keep the largest hits: A→2, B→3
        assert_eq!(out.table.rows[0], vec![Value::Str("A".into()), Value::I64(2)]);
        assert_eq!(out.table.rows[1], vec![Value::Str("B".into()), Value::I64(3)]);
        // k out of range
        let err =
            parse_spec_json(r#"{"topKPerGroup":{"by":["region"],"k":0}}"#).unwrap_err();
        assert!(err.contains("topKPerGroup k"), "err={err}");
        // empty by
        let err = parse_spec_json(r#"{"topKPerGroup":{"by":[],"k":1}}"#).unwrap_err();
        assert!(err.contains("topKPerGroup by"), "err={err}");
    }

    #[test]
    fn r1_type_error_before_eval() {
        let spec =
            parse_spec_json(r#"{"mutate":[{"as":"x","expr":"region + 1"}]}"#).unwrap();
        let err = run_agg(&table(), &spec, 1000, 5000).unwrap_err();
        assert!(
            err.contains("mutate x: cannot add number to string"),
            "err={err}"
        );
    }

    #[test]
    fn mutate_null_and_cast_null_warnings() {
        let t = Table {
            columns: vec![
                Column { name: "a".into(), dtype: "i64".into() },
                Column { name: "s".into(), dtype: "utf8".into() },
            ],
            rows: vec![
                vec![Value::I64(1), Value::Str("10".into())],
                vec![Value::I64(0), Value::Str("abc".into())],
            ],
        };
        let spec = parse_spec_json(r#"{"mutate":[{"as":"x","expr":"a / 0"}]}"#).unwrap();
        let out = run_agg(&t, &spec, 1000, 5000).unwrap();
        let w = out
            .warnings
            .iter()
            .find(|w| w.code == "mutate-null")
            .expect("mutate-null warning");
        assert_eq!(w.column.as_deref(), Some("x"));
        assert_eq!(w.count, 2);
        let spec = parse_spec_json(r#"{"mutate":[{"as":"y","expr":"to_int(s)"}]}"#).unwrap();
        let out = run_agg(&t, &spec, 1000, 5000).unwrap();
        let w = out
            .warnings
            .iter()
            .find(|w| w.code == "cast-null")
            .expect("cast-null warning");
        assert_eq!(w.count, 1);
        // clean run: no warnings
        let spec = parse_spec_json(r#"{"aggs":[{"fn":"count","as":"n"}]}"#).unwrap();
        let out = run_agg(&t, &spec, 1000, 5000).unwrap();
        assert!(out.warnings.is_empty());
    }
}
