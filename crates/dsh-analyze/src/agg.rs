//! Declarative agg/filter/select for T0.

use std::collections::HashMap;

use crate::dataset::{Table, Value};

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
pub struct Spec {
    pub select: Option<Vec<String>>,
    pub filter: Vec<Filter>,
    pub group_by: Vec<String>,
    pub aggs: Vec<Agg>,
    pub order_by: Vec<OrderBy>,
    pub limit: Option<usize>,
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

pub fn run_agg(table: &Table, spec: &Spec, default_limit: usize, max_limit: usize) -> Result<Table, String> {
    let limit = spec.limit.unwrap_or(default_limit).min(max_limit).min(5000);

    // column index helper
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

    // group or project
    if spec.aggs.is_empty() && spec.group_by.is_empty() {
        return project_rows(table, &keep, spec, limit);
    }

    // group keys
    let mut groups: HashMap<Vec<Value>, Vec<usize>> = HashMap::new();
    let group_idxs: Vec<usize> = spec.group_by.iter().map(|g| idx(g)).collect::<Result<_, _>>()?;
    for i in keep {
        let key: Vec<Value> = group_idxs
            .iter()
            .map(|&ci| table.rows[i].get(ci).cloned().unwrap_or(Value::Null))
            .collect();
        groups.entry(key).or_default().push(i);
    }

    let out_names: Vec<String> = if spec.aggs.is_empty() {
        spec.group_by.clone()
    } else {
        let mut names = spec.group_by.clone();
        for a in &spec.aggs {
            let name = a
                .r#as
                .clone()
                .unwrap_or_else(|| default_agg_name(a));
            names.push(name);
        }
        names
    };

    let mut out_rows: Vec<Vec<Value>> = Vec::new();
    for (key, members) in groups {
        let mut row: Vec<Value> = key.clone();
        for a in &spec.aggs {
            let v = compute_agg(table, &members, a, &idx)?;
            row.push(v);
        }
        out_rows.push(row);
    }

    // order
    if !spec.order_by.is_empty() {
        let order_idxs: Vec<(usize, bool)> = spec
            .order_by
            .iter()
            .map(|o| Ok((out_names.iter().position(|n| *n == o.column).ok_or_else(|| format!("analysis/invalid-args: unknown order column {}", o.column))?, o.desc)))
            .collect::<Result<_, String>>()?;
        out_rows.sort_by(|a, b| {
            for (i, desc) in &order_idxs {
                let ord = cmp_values(a.get(*i).unwrap_or(&Value::Null), b.get(*i).unwrap_or(&Value::Null));
                let ord = if *desc { ord.reverse() } else { ord };
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            std::cmp::Ordering::Equal
        });
    }

    out_rows.truncate(limit);

    let columns = out_names
        .iter()
        .map(|n| crate::dataset::Column {
            name: n.clone(),
            dtype: "utf8".into(), // refined below
        })
        .collect::<Vec<_>>();
    let mut table_out = Table {
        columns,
        rows: out_rows,
    };
    // infer dtypes
    let mut cols = Vec::new();
    for (i, name) in out_names.iter().enumerate() {
        let col_values: Vec<Value> = table_out.rows.iter().filter_map(|r| r.get(i).cloned()).collect();
        let dtype = infer_from_values(&col_values);
        cols.push(crate::dataset::Column {
            name: name.clone(),
            dtype,
        });
    }
    table_out.columns = cols;
    Ok(table_out)
}

fn default_agg_name(a: &Agg) -> String {
    match &a.column {
        Some(c) => format!("{c}_{}", a.r#fn),
        None => a.r#fn.clone(),
    }
}

fn project_rows(table: &Table, keep: &[usize], spec: &Spec, limit: usize) -> Result<Table, String> {
    let all: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();
    let select: Vec<String> = spec.select.clone().unwrap_or(all.clone());
    let idxs: Vec<usize> = select
        .iter()
        .map(|name| {
            table
                .columns
                .iter()
                .position(|c| &c.name == name)
                .ok_or_else(|| format!("analysis/invalid-args: unknown column {name}"))
        })
        .collect::<Result<_, String>>()?;
    let mut rows = Vec::new();
    for &i in keep.iter().take(limit) {
        let row = &table.rows[i];
        rows.push(
            idxs.iter()
                .map(|&ci| row.get(ci).cloned().unwrap_or(Value::Null))
                .collect(),
        );
    }
    let columns = idxs
        .iter()
        .map(|&ci| table.columns[ci].clone())
        .collect();
    Ok(Table { columns, rows })
}

fn compute_agg(table: &Table, members: &[usize], a: &Agg, idx: &dyn Fn(&str) -> Result<usize, String>) -> Result<Value, String> {
    if a.r#fn == "count" {
        return Ok(Value::I64(members.len() as i64));
    }
    let col = a
        .column
        .clone()
        .ok_or_else(|| "analysis/invalid-args: agg column required".to_string())?;
    let ci = idx(&col)?;
    let mut nums: Vec<f64> = Vec::new();
    let mut vals: Vec<Value> = Vec::new();
    for &m in members {
        let v = table.rows[m].get(ci).cloned().unwrap_or(Value::Null);
        vals.push(v.clone());
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
            let var = nums.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (nums.len() - 1) as f64;
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

/// Tiny recursive-ish object extractors for the flat-ish JSON we accept in T0.
pub fn parse_spec_json(input: &str) -> Result<Spec, String> {
    // Reuse a pragmatic approach: strip outer braces and scan keys we care about.
    let s = input.trim();
    if !s.starts_with('{') || !s.ends_with('}') {
        return Err("analysis/invalid-args: spec must be object".into());
    }
    let mut spec = Spec {
        select: None,
        filter: Vec::new(),
        group_by: Vec::new(),
        aggs: Vec::new(),
        order_by: Vec::new(),
        limit: None,
    };

    if let Some(arr) = extract_array(s, "select")? {
        spec.select = Some(parse_string_array(&arr)?);
    }
    if let Some(arr) = extract_array(s, "groupBy")? {
        spec.group_by = parse_string_array(&arr)?;
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
                // in: join string list as first value only for T0 — reject multi without proper type
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
    digits.parse().map(Some).map_err(|_| "analysis/invalid-args: bad number".into())
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
    // naive: find "key" that is not inside a string value — T0 ok
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
            // skip non-string tokens
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
