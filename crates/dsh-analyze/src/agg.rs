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
                "in" => match &f.value {
                    Value::Str(_) => values_eq(&cell, &f.value),
                    _ => false,
                },
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
    }
    if let Some(arr) = extract_array(s, "groupBy")? {
        spec.group_by = parse_group_by(&arr)?;
    } else if find_key(s, "\"groupBy\"").is_some() {
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
    } else if find_key(s, "\"mutate\"").is_some() {
        return Err("analysis/invalid-args: mutate must be an array".into());
    }
    if let Some(n) = extract_number(s, "limit")? {
        spec.limit = Some(n as usize);
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
            let value = if op == "is_null" || op == "not_null" {
                Value::Null
            } else if let Some(v) = extract_number(&obj, "value")? {
                Value::F64(v)
            } else if let Some(v) = extract_bool(&obj, "value")? {
                Value::Bool(v)
            } else if let Some(v) = extract_string_field(&obj, "value")? {
                Value::Str(v)
            } else if let Some(arr) = extract_array(&obj, "value")? {
                let items = parse_string_array(&arr)?;
                Value::Str(items.first().cloned().unwrap_or_default())
            } else {
                Value::Null
            };
            spec.filter.push(Filter { op, column, value });
        }
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
    }
    if let Some(arr) = extract_array(s, "orderBy")? {
        for obj in split_top_objects(&arr)? {
            let column = extract_string_field(&obj, "column")?
                .ok_or_else(|| "analysis/invalid-args: orderBy column required".to_string())?;
            let desc = extract_bool(&obj, "desc")?.unwrap_or(false);
            spec.order_by.push(OrderBy { column, desc });
        }
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
    let mut depth = 0;
    for (i, ch) in after.char_indices() {
        match ch {
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

fn find_key(json: &str, quoted_key: &str) -> Option<usize> {
    json.find(quoted_key)
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
