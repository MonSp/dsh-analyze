//! Declarative agg/filter/select/mutate/time-bucket for v0.2.

use std::collections::HashMap;

use crate::dataset::{Column, Table, Value};
use crate::expr::{self, Expr};
use crate::time::{bucket_key, parse_timestamp_utc_date, Period};

#[derive(Debug, Clone)]
pub struct Filter {
    pub op: String,
    pub column: String,
    pub value: Value,
    /// Set members for `op == "in"` (possibly empty); empty for every other op.
    pub members: Vec<Value>,
}

#[derive(Debug, Clone)]
pub struct Agg {
    pub r#fn: String,
    pub column: Option<String>,
    pub r#as: Option<String>,
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
pub enum GroupKeySpec {
    Column(String),
    Period {
        date_column: String,
        period: Period,
    },
}

#[derive(Debug, Clone)]
pub struct Spec {
    pub select: Option<Vec<String>>,
    pub filter: Vec<Filter>,
    pub mutate: Vec<Mutate>,
    pub group_by: Vec<GroupKeySpec>,
    pub aggs: Vec<Agg>,
    pub order_by: Vec<OrderBy>,
    pub limit: Option<usize>,
}

#[derive(Debug)]
pub struct AggOutcome {
    pub table: Table,
    /// Non-null unparseable time cells across all period group columns.
    pub parse_fail_count: u64,
}

pub fn validate_op(op: &str) -> bool {
    matches!(
        op,
        "eq" | "ne" | "gt" | "gte" | "lt" | "lte" | "in" | "contains" | "is_null" | "not_null"
    )
}

pub fn validate_fn(f: &str) -> bool {
    matches!(f, "count" | "sum" | "mean" | "min" | "max" | "median" | "std")
}

fn period_output_name(date_column: &str, period: &Period) -> String {
    format!("{date_column}_{}", period.as_str())
}

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

    // filter
    let mut keep: Vec<usize> = Vec::new();
    for (i, row) in table.rows.iter().enumerate() {
        let mut ok = true;
        for f in &spec.filter {
            let ci = idx(&f.column)?;
            let cell = row.get(ci).cloned().unwrap_or(Value::Null);
            ok = match f.op.as_str() {
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
            };
            if !ok {
                break;
            }
        }
        if ok {
            keep.push(i);
        }
    }

    // Working table after filter (clone rows we need for mutate + group).
    let mut work_cols: Vec<Column> = table.columns.clone();
    let mut work_rows: Vec<Vec<Value>> = keep
        .iter()
        .map(|&i| table.rows[i].clone())
        .collect();

    // mutate (derived columns, sequential visibility)
    let mut name_set: Vec<String> = work_cols.iter().map(|c| c.name.clone()).collect();
    for m in &spec.mutate {
        if name_set.contains(&m.r#as) {
            return Err(format!(
                "analysis/invalid-args: mutate column collides: {}",
                m.r#as
            ));
        }
        let col_index: Vec<(String, usize)> = work_cols
            .iter()
            .enumerate()
            .map(|(i, c)| (c.name.clone(), i))
            .collect();
        // Validate identifiers against current column set
        let available: Vec<String> = work_cols.iter().map(|c| c.name.clone()).collect();
        expr::check_idents(&m.expr, &available)?;
        let mut out_vals = Vec::with_capacity(work_rows.len());
        for row in &work_rows {
            out_vals.push(expr::eval_expr(&m.expr, row, &col_index)?);
        }
        let dtype = crate::agg::infer_from_values(&out_vals);
        work_cols.push(Column {
            name: m.r#as.clone(),
            dtype,
        });
        name_set.push(m.r#as.clone());
        for (row, v) in work_rows.iter_mut().zip(out_vals) {
            row.push(v);
        }
    }

    // No group / no aggs → project
    if spec.aggs.is_empty() && spec.group_by.is_empty() {
        let work = Table {
            columns: work_cols,
            rows: work_rows,
        };
        let t = project_rows(&work, spec, limit)?;
        return Ok(AggOutcome {
            table: t,
            parse_fail_count: 0,
        });
    }

    // Resolve group keys against work_cols
    let mut group_labels: Vec<String> = Vec::new();
    let mut group_resolvers: Vec<GroupResolver> = Vec::new();
    let mut parse_fail_count: u64 = 0;
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
                                        Value::Null
                                    }
                                }
                            }
                        }
                        other => match other.to_json() {
                            // non-string non-null: try json string form as text
                            j => {
                                let s = j.trim_matches('"').to_string();
                                match parse_timestamp_utc_date(&s) {
                                    Some(p) => Value::Str(bucket_key(p, period)),
                                    None => {
                                        parse_fail_count += 1;
                                        Value::Null
                                    }
                                }
                            }
                        },
                    };
                    key.push(bucket);
                }
            }
        }
        groups.entry(key).or_default().push(row_i);
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

    if !spec.order_by.is_empty() {
        let order_idxs: Vec<(usize, bool)> = spec
            .order_by
            .iter()
            .map(|o| {
                out_names
                    .iter()
                    .position(|n| *n == o.column)
                    .ok_or_else(|| {
                        format!("analysis/invalid-args: unknown order column {}", o.column)
                    })
                    .map(|i| (i, o.desc))
            })
            .collect::<Result<_, String>>()?;
        out_rows.sort_by(|a, b| {
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

    out_rows.truncate(limit);

    // optional select on aggregated columns
    let (final_names, final_rows) = if let Some(select) = &spec.select {
        let mut idxs = Vec::new();
        for name in select {
            let i = out_names.iter().position(|n| n == name).ok_or_else(|| {
                format!("analysis/invalid-args: unknown column {name}")
            })?;
            idxs.push(i);
        }
        let names: Vec<String> = idxs.iter().map(|&i| out_names[i].clone()).collect();
        let rows: Vec<Vec<Value>> = out_rows
            .iter()
            .map(|r| idxs.iter().map(|&i| r.get(i).cloned().unwrap_or(Value::Null)).collect())
            .collect();
        (names, rows)
    } else {
        (out_names, out_rows)
    };

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

    Ok(AggOutcome {
        table: Table {
            columns: cols,
            rows: final_rows,
        },
        parse_fail_count,
    })
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

fn project_rows(work: &Table, spec: &Spec, limit: usize) -> Result<Table, String> {
    let all: Vec<String> = work.columns.iter().map(|c| c.name.clone()).collect();
    let select: Vec<String> = spec.select.clone().unwrap_or(all);
    let idxs: Vec<usize> = select
        .iter()
        .map(|name| {
            work
                .columns
                .iter()
                .position(|c| &c.name == name)
                .ok_or_else(|| format!("analysis/invalid-args: unknown column {name}"))
        })
        .collect::<Result<_, String>>()?;
    let mut rows = Vec::new();
    for row in work.rows.iter().take(limit) {
        rows.push(
            idxs.iter()
                .map(|&ci| row.get(ci).cloned().unwrap_or(Value::Null))
                .collect(),
        );
    }
    let columns = idxs.iter().map(|&ci| work.columns[ci].clone()).collect();
    Ok(Table { columns, rows })
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
    let col = a.column.clone().ok_or_else(|| {
        "analysis/invalid-args: agg column required".to_string()
    })?;
    let ci = idx(&col)?;
    let mut nums: Vec<f64> = Vec::new();
    for &m in members {
        let v = table.rows[m].get(ci).cloned().unwrap_or(Value::Null);
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
        other => Err(format!("analysis/invalid-args: bad agg fn {other}")),
    }
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
            "select" | "filter" | "mutate" | "groupBy" | "aggs" | "orderBy" | "limit"
        ) {
            return Err(format!("analysis/invalid-args: unknown spec key {key}"));
        }
    }
    let has = |k: &str| keys.iter().any(|x| x.as_str() == k);
    let mut spec = Spec {
        select: None,
        filter: Vec::new(),
        mutate: Vec::new(),
        group_by: Vec::new(),
        aggs: Vec::new(),
        order_by: Vec::new(),
        limit: None,
    };

    if let Some(arr) = extract_array(s, "select")? {
        spec.select = Some(parse_string_array(&arr)?);
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
    if let Some(arr) = extract_array(s, "filter")? {
        for obj in split_top_objects(&arr)? {
            let op = extract_string_field(&obj, "op")?
                .ok_or_else(|| "analysis/invalid-args: filter op required".to_string())?;
            let column = extract_string_field(&obj, "column")?
                .ok_or_else(|| "analysis/invalid-args: filter column required".to_string())?;
            if !validate_op(&op) {
                return Err(format!("analysis/invalid-args: bad filter op {op}"));
            }
            let mut in_members: Option<Vec<Value>> = None;
            let value = if op == "is_null" || op == "not_null" {
                Value::Null
            } else if let Some(v) = extract_number(&obj, "value")? {
                Value::F64(v)
            } else if let Some(v) = extract_bool(&obj, "value")? {
                Value::Bool(v)
            } else if let Some(v) = extract_string_field(&obj, "value")? {
                Value::Str(v)
            } else if let Some(arr) = extract_array(&obj, "value")? {
                if op != "in" {
                    return Err(format!(
                        "analysis/invalid-args: filter value must not be an array for op {op}"
                    ));
                }
                in_members = Some(parse_value_array(&arr)?);
                Value::Null
            } else if find_key(&obj, "\"value\"").is_some() {
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
            spec.filter.push(Filter { op, column, value, members });
        }
    } else if has("filter") {
        return Err("analysis/invalid-args: filter must be an array".into());
    }
    if let Some(arr) = extract_array(s, "aggs")? {
        for obj in split_top_objects(&arr)? {
            let r#fn = extract_string_field(&obj, "fn")?
                .ok_or_else(|| "analysis/invalid-args: agg fn required".to_string())?;
            if !validate_fn(&r#fn) {
                return Err(format!("analysis/invalid-args: bad agg fn {}", r#fn));
            }
            let column = extract_string_field(&obj, "column")?;
            let r#as = extract_string_field(&obj, "as")?;
            spec.aggs.push(Agg {
                r#fn,
                column,
                r#as,
            });
        }
    } else if has("aggs") {
        return Err("analysis/invalid-args: aggs must be an array".into());
    }
    if let Some(arr) = extract_array(s, "orderBy")? {
        for obj in split_top_objects(&arr)? {
            let column = extract_string_field(&obj, "column")?
                .ok_or_else(|| "analysis/invalid-args: orderBy column required".to_string())?;
            let desc = extract_bool(&obj, "desc")?.unwrap_or(false);
            spec.order_by.push(OrderBy { column, desc });
        }
    } else if has("orderBy") {
        return Err("analysis/invalid-args: orderBy must be an array".into());
    }
    Ok(spec)
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
    let needle = format!("\"{key}\"");
    let Some(idx) = find_key(json, &needle) else {
        return Ok(None);
    };
    let after = &json[idx + needle.len()..];
    let after = after.trim_start();
    if !after.starts_with(':') {
        return Ok(None);
    }
    let after = after[1..].trim_start();
    if !after.starts_with('[') {
        return Ok(None);
    }
    // String-aware bracket matching: `[` / `]` inside string elements must not
    // truncate the array (e.g. select ["A]B"]).
    let mut depth = 0;
    let mut in_str = false;
    let mut esc = false;
    for (i, ch) in after.char_indices() {
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
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(Some(after[..=i].to_string()));
                }
            }
            _ => {}
        }
    }
    Err("analysis/invalid-args: unterminated array".into())
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
    let digits: String = after
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect();
    if digits.is_empty() {
        return Ok(None);
    }
    digits
        .parse()
        .map(Some)
        .map_err(|_| "analysis/invalid-args: bad number".into())
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
        let tok: String = rest
            .chars()
            .take_while(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e' | 'E'))
            .collect();
        if tok.is_empty() {
            return Err("analysis/invalid-args: bad filter value".into());
        }
        let n: f64 = tok
            .parse()
            .map_err(|_| format!("analysis/invalid-args: bad filter value {tok}"))?;
        out.push(Value::F64(n));
        rest = &rest[tok.len()..];
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
    for ch in inner.chars() {
        if in_str {
            cur.push(ch);
            if ch == '"' {
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
        assert_eq!(spec.filter.len(), 1);
        assert_eq!(spec.filter[0].op, "eq");
        assert_eq!(spec.filter[0].column, "op");
    }

    /// filter must be an array; object/string/empty-object shapes must error, never pass through.
    #[test]
    fn filter_must_be_array() {
        let err =
            parse_spec_json(r#"{"filter":{"column":"amount","op":"gt","value":3000}}"#).unwrap_err();
        assert!(err.contains("filter must be an array"));
        assert!(parse_spec_json(r#"{"filter":"amount > 3000"}"#).is_err());
        assert!(parse_spec_json(r#"{"filter":{}}"#).is_err());
        assert!(parse_spec_json(r#"{"filter":[{"op":"eq","column":"a","value":1}]}"#).is_ok());
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
        assert_eq!(parsed.filter.len(), 1);
        assert_eq!(parsed.filter[0].column, "region");
        // nested `select` inside a mutate item is not the spec select
        let parsed = parse_spec_json(
            r#"{"mutate":[{"as":"n","expr":"hits + total","select":[]}],"select":["region"]}"#,
        )
        .unwrap();
        assert_eq!(parsed.select.as_deref(), Some(&["region".to_string()][..]));
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
        assert_eq!(
            spec.select.as_deref(),
            Some(&["A]B".to_string(), "C".to_string()][..])
        );
        let spec =
            parse_spec_json(r#"{"filter":[{"op":"in","column":"a","value":["x]y","z"]}]}"#)
                .unwrap();
        assert_eq!(spec.filter[0].members, vec![Value::Str("x]y".into()), Value::Str("z".into())]);
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
        // null bucket + one day bucket
        assert_eq!(out.table.rows.len(), 2);
        assert!(out.table.columns.iter().any(|c| c.name == "created_at_day"));
    }
}
