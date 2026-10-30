//! Dataset helpers: workspace-confined paths + CSV tables.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub dtype: String,
}

#[derive(Debug, Clone)]
pub struct Table {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Value>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    Str(String),
}

impl Eq for Value {}

impl std::hash::Hash for Value {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match self {
            Value::Null => 0u8.hash(state),
            Value::Bool(b) => {
                1u8.hash(state);
                b.hash(state);
            }
            Value::I64(i) => {
                2u8.hash(state);
                i.hash(state);
            }
            Value::F64(f) => {
                3u8.hash(state);
                f.to_bits().hash(state);
            }
            Value::Str(s) => {
                4u8.hash(state);
                s.hash(state);
            }
        }
    }
}

impl Value {
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::I64(v) => Some(*v as f64),
            Value::F64(v) => Some(*v),
            Value::Str(s) => s.trim().parse::<f64>().ok(),
            _ => None,
        }
    }

    pub fn to_json(&self) -> String {
        match self {
            Value::Null => "null".to_string(),
            Value::Bool(b) => if *b { "true".into() } else { "false".into() },
            Value::I64(v) => v.to_string(),
            Value::F64(v) => {
                if v.is_finite() {
                    format!("{v}")
                } else {
                    "null".into()
                }
            }
            Value::Str(s) => json_string(s),
        }
    }
}

pub fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn ensure_within_workspace(workspace: &Path, candidate: &Path) -> Result<PathBuf, String> {
    let ws = fs::canonicalize(workspace).map_err(|e| format!("workspace unavailable: {e}"))?;
    let abs = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        ws.join(candidate)
    };
    // canonicalize parent if file does not exist yet
    let norm = if abs.exists() {
        fs::canonicalize(&abs).map_err(|e| format!("invalid-path: {e}"))?
    } else {
        let parent = abs
            .parent()
            .ok_or_else(|| "invalid-path: no parent".to_string())?;
        let parent_c = fs::canonicalize(parent).map_err(|e| format!("invalid-path: {e}"))?;
        parent_c.join(abs.file_name().ok_or("invalid-path: no file name")?)
    };
    if !norm.starts_with(&ws) {
        return Err("analysis/invalid-path: path escapes workspace".into());
    }
    Ok(norm)
}

pub fn analysis_dir(workspace: &Path) -> PathBuf {
    workspace.join(".dsh-analyze")
}

pub fn datasets_dir(workspace: &Path) -> PathBuf {
    analysis_dir(workspace).join("datasets")
}

pub fn outputs_dir(workspace: &Path) -> PathBuf {
    analysis_dir(workspace).join("outputs")
}

pub fn new_dataset_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    )
    .unwrap_or(0);
    // Mix timestamp with process-local entropy to avoid pure-nanos collisions.
    let entropy = std::process::id() as u64 ^ nanos.rotate_left(9);
    let mixed = nanos
        .rotate_left(17)
        .wrapping_mul(0x9e37_79b9_7f4a_7c15)
        .wrapping_add(entropy.wrapping_mul(0xbf58_476d_1ce4_e5b9));
    let a = (mixed >> 32) as u32;
    let b = mixed as u32;
    format!("ds_{a:08x}{b:08x}")
}

fn infer_dtype(values: &[Value]) -> String {
    let mut seen_bool = false;
    let mut seen_i64 = false;
    let mut seen_f64 = false;
    let mut seen_str = false;
    let mut any = false;
    for v in values {
        match v {
            Value::Null => {}
            Value::Bool(_) => {
                seen_bool = true;
                any = true;
            }
            Value::I64(_) => {
                seen_i64 = true;
                any = true;
            }
            Value::F64(_) => {
                seen_f64 = true;
                any = true;
            }
            Value::Str(_) => {
                seen_str = true;
                any = true;
            }
        }
    }
    if !any {
        return "utf8".into();
    }
    if seen_str {
        return "utf8".into();
    }
    if seen_bool && !seen_i64 && !seen_f64 {
        return "bool".into();
    }
    if seen_f64 {
        return "f64".into();
    }
    if seen_i64 {
        return "i64".into();
    }
    "utf8".into()
}

fn parse_cell(raw: &str) -> Value {
    let t = raw.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("null") || t == "NA" {
        return Value::Null;
    }
    if t.eq_ignore_ascii_case("true") {
        return Value::Bool(true);
    }
    if t.eq_ignore_ascii_case("false") {
        return Value::Bool(false);
    }
    if let Ok(i) = t.parse::<i64>() {
        return Value::I64(i);
    }
    if let Ok(f) = t.parse::<f64>() {
        return Value::F64(f);
    }
    Value::Str(raw.to_string())
}

pub fn parse_csv_path(path: &Path) -> Result<Table, String> {
    let file = fs::File::open(path).map_err(|e| format!("analysis/cli-failed: open: {e}"))?;
    let mut reader = BufReader::new(file);
    let mut header_line = String::new();
    let n = reader
        .read_line(&mut header_line)
        .map_err(|e| format!("analysis/cli-failed: {e}"))?;
    if n == 0 {
        return Err("analysis/cli-failed: empty csv".into());
    }
    // Strip UTF-8 BOM so the first column name stays clean.
    let header_line = header_line.strip_prefix('\u{feff}').unwrap_or(&header_line);
    let headers: Vec<String> = split_csv_line(header_line.trim_end_matches(['\n', '\r']))
        .into_iter()
        .map(|s| s.trim().to_string())
        .collect();
    let mut rows: Vec<Vec<Value>> = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .map_err(|e| format!("analysis/cli-failed: {e}"))?;
        if read == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            continue;
        }
        let cells = split_csv_line(trimmed);
        let mut row = Vec::with_capacity(headers.len());
        for i in 0..headers.len() {
            let cell = cells.get(i).map(|s| s.as_str()).unwrap_or("");
            row.push(parse_cell(cell));
        }
        rows.push(row);
    }
    finish_table(headers, rows)
}

pub fn parse_jsonl_path(path: &Path) -> Result<Table, String> {
    let file = fs::File::open(path).map_err(|e| format!("analysis/cli-failed: open: {e}"))?;
    let reader = BufReader::new(file);
    let mut rows_map: Vec<BTreeMap<String, Value>> = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|e| format!("analysis/cli-failed: {e}"))?;
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        rows_map.push(parse_json_object(t)?);
    }
    let mut headers: Vec<String> = Vec::new();
    for row in &rows_map {
        for k in row.keys() {
            if !headers.iter().any(|h| h == k) {
                headers.push(k.clone());
            }
        }
    }
    headers.sort();
    let mut rows = Vec::with_capacity(rows_map.len());
    for row in &rows_map {
        rows.push(
            headers
                .iter()
                .map(|h| row.get(h).cloned().unwrap_or(Value::Null))
                .collect(),
        );
    }
    finish_table(headers, rows)
}

fn finish_table(headers: Vec<String>, rows: Vec<Vec<Value>>) -> Result<Table, String> {
    if headers.is_empty() {
        return Err("analysis/cli-failed: no columns".into());
    }
    let columns = headers
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let col_values: Vec<Value> = rows.iter().filter_map(|r| r.get(i).cloned()).collect();
            Column {
                name: name.clone(),
                dtype: infer_dtype(&col_values),
            }
        })
        .collect();
    Ok(Table { columns, rows })
}

/// Minimal JSON object parser for flat objects with scalar values.
/// T0 only; nested objects become JSON strings.
pub fn parse_json_object(line: &str) -> Result<BTreeMap<String, Value>, String> {
    let mut map = BTreeMap::new();
    let s = line.trim();
    if !s.starts_with('{') || !s.ends_with('}') {
        return Err("analysis/cli-failed: jsonl row must be object".into());
    }
    let inner = &s[1..s.len() - 1];
    let mut i = 0;
    let bytes: Vec<char> = inner.chars().collect();
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        if bytes[i] == ',' {
            i += 1;
            continue;
        }
        if bytes[i] != '"' {
            return Err("analysis/cli-failed: expected key".into());
        }
        let (key, ni) = read_json_string(&bytes, i)?;
        i = ni;
        while i < bytes.len() && bytes[i].is_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != ':' {
            return Err("analysis/cli-failed: expected ':'".into());
        }
        i += 1;
        while i < bytes.len() && bytes[i].is_whitespace() {
            i += 1;
        }
        let (value, ni) = read_json_scalar(&bytes, i)?;
        i = ni;
        map.insert(key, value);
    }
    Ok(map)
}

fn read_json_string(chars: &[char], start: usize) -> Result<(String, usize), String> {
    let mut i = start + 1;
    let mut out = String::new();
    while i < chars.len() {
        match chars[i] {
            '"' => return Ok((out, i + 1)),
            '\\' => {
                i += 1;
                let esc = *chars.get(i).ok_or("analysis/cli-failed: bad escape")?;
                out.push(match esc {
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    other => other,
                });
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Err("analysis/cli-failed: unterminated string".into())
}

fn read_json_scalar(chars: &[char], start: usize) -> Result<(Value, usize), String> {
    let mut i = start;
    if i >= chars.len() {
        return Err("analysis/cli-failed: expected value".into());
    }
    match chars[i] {
        '"' => {
            let (s, n) = read_json_string(chars, i)?;
            Ok((Value::Str(s), n))
        }
        't' => {
            expect_lit(chars, i, "true")?;
            Ok((Value::Bool(true), i + 4))
        }
        'f' => {
            expect_lit(chars, i, "false")?;
            Ok((Value::Bool(false), i + 5))
        }
        'n' => {
            expect_lit(chars, i, "null")?;
            Ok((Value::Null, i + 4))
        }
        _ => {
            let mut j = i;
            while j < chars.len()
                && (chars[j].is_ascii_digit()
                    || matches!(chars[j], '-' | '+' | '.' | 'e' | 'E'))
            {
                j += 1;
            }
            let tok: String = chars[i..j].iter().collect();
            if let Ok(v) = tok.parse::<i64>() {
                return Ok((Value::I64(v), j));
            }
            if let Ok(v) = tok.parse::<f64>() {
                return Ok((Value::F64(v), j));
            }
            Err("analysis/cli-failed: bad scalar".into())
        }
    }
}

fn expect_lit(chars: &[char], at: usize, lit: &str) -> Result<(), String> {
    for (k, ch) in lit.chars().enumerate() {
        if chars.get(at + k) != Some(&ch) {
            return Err(format!("analysis/cli-failed: expected {lit}"));
        }
    }
    Ok(())
}

pub fn split_csv_line(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    cur.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                cur.push(c);
            }
        } else if c == '"' {
            in_quotes = true;
        } else if c == ',' {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    out.push(cur);
    out
}

pub struct Meta {
    pub dataset_id: String,
    pub row_count: usize,
    pub columns: Vec<Column>,
    pub source_kind: String,
    pub source_path: String,
    pub label: Option<String>,
    pub produced_by: String,
    pub parent_dataset_id: Option<String>,
}

impl Meta {
    pub fn to_json(&self) -> String {
        let cols = self
            .columns
            .iter()
            .map(|c| format!("{{\"name\":{},\"dtype\":{}}}", json_string(&c.name), json_string(&c.dtype)))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "{{\"version\":1,\"datasetId\":{},\"rowCount\":{},\"columnCount\":{},\"columns\":[{}],\"sourceKind\":{},\"sourcePath\":{},\"label\":{},\"producedBy\":{},\"parentDatasetId\":{}}}",
            json_string(&self.dataset_id),
            self.row_count,
            self.columns.len(),
            cols,
            json_string(&self.source_kind),
            json_string(&self.source_path),
            self.label
                .as_deref()
                .map(json_string)
                .unwrap_or_else(|| "null".into()),
            json_string(&self.produced_by),
            self.parent_dataset_id
                .as_deref()
                .map(json_string)
                .unwrap_or_else(|| "null".into()),
        )
    }

    pub fn columns_json(&self) -> String {
        self.columns
            .iter()
            .map(|c| format!("{{\"name\":{},\"dtype\":{}}}", json_string(&c.name), json_string(&c.dtype)))
            .collect::<Vec<_>>()
            .join(",")
    }
}

pub fn write_table_parquet_placeholder(workspace: &Path, id: &str, table: &Table) -> Result<(u64, PathBuf), String> {
    // T0 stores a simple CSV next to meta; format name "csv" until Parquet lands.
    let dir = datasets_dir(workspace);
    fs::create_dir_all(&dir).map_err(|e| format!("analysis/cli-failed: {e}"))?;
    let path = dir.join(format!("{id}.csv"));
    let mut f = fs::File::create(&path).map_err(|e| format!("analysis/cli-failed: {e}"))?;
    let header = table
        .columns
        .iter()
        .map(|c| csv_escape(&c.name))
        .collect::<Vec<_>>()
        .join(",");
    writeln!(f, "{header}").map_err(|e| format!("analysis/cli-failed: {e}"))?;
    for row in &table.rows {
        let line = row
            .iter()
            .map(|v| match v {
                Value::Null => String::new(),
                Value::Str(s) => csv_escape(s),
                other => other.to_json(),
            })
            .collect::<Vec<_>>()
            .join(",");
        writeln!(f, "{line}").map_err(|e| format!("analysis/cli-failed: {e}"))?;
    }
    let bytes = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    Ok((bytes, path))
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

pub fn save_meta(workspace: &Path, meta: &Meta) -> Result<(), String> {
    let dir = datasets_dir(workspace);
    fs::create_dir_all(&dir).map_err(|e| format!("analysis/cli-failed: {e}"))?;
    let path = dir.join(format!("{}.meta.json", meta.dataset_id));
    fs::write(path, format!("{}\n", meta.to_json()))
        .map_err(|e| format!("analysis/cli-failed: {e}"))
}

pub fn load_dataset(workspace: &Path, id: &str) -> Result<(Meta, Table), String> {
    validate_dataset_id(id)?;
    let meta_path = datasets_dir(workspace).join(format!("{id}.meta.json"));
    let csv_path = datasets_dir(workspace).join(format!("{id}.csv"));
    if !meta_path.exists() || !csv_path.exists() {
        return Err("analysis/unknown-dataset".into());
    }
    let meta_raw = fs::read_to_string(&meta_path).map_err(|e| format!("analysis/cli-failed: {e}"))?;
    let row_count = extract_u64(&meta_raw, "rowCount").unwrap_or(0) as usize;
    let source_kind = extract_string(&meta_raw, "sourceKind").unwrap_or_else(|| "csv".into());
    let source_path = extract_string(&meta_raw, "sourcePath").unwrap_or_default();
    let label = extract_string(&meta_raw, "label");
    let produced_by = extract_string(&meta_raw, "producedBy").unwrap_or_else(|| "ingest".into());
    let parent = extract_string(&meta_raw, "parentDatasetId");
    let table = parse_csv_path(&csv_path)?;
    let meta = Meta {
        dataset_id: id.to_string(),
        row_count: row_count.max(table.rows.len()),
        columns: table.columns.clone(),
        source_kind,
        source_path,
        label,
        produced_by,
        parent_dataset_id: parent,
    };
    Ok((meta, table))
}

pub fn validate_dataset_id(id: &str) -> Result<(), String> {
    if !id.starts_with("ds_") || id.len() < 6 || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err("analysis/invalid-args: bad datasetId".into());
    }
    Ok(())
}

/// Summarize every `*.meta.json` under the datasets dir (sorted by datasetId).
/// Returns one JSON object string per dataset.
pub fn list_dataset_summaries(workspace: &Path) -> Result<Vec<String>, String> {
    let dir = datasets_dir(workspace);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut names: Vec<String> = fs::read_dir(&dir)
        .map_err(|e| format!("analysis/cli-failed: {e}"))?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if name.ends_with(".meta.json") {
                Some(name)
            } else {
                None
            }
        })
        .collect();
    names.sort();
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let raw = fs::read_to_string(dir.join(&name))
            .map_err(|e| format!("analysis/cli-failed: {e}"))?;
        let fallback_id = name.trim_end_matches(".meta.json").to_string();
        let dataset_id = extract_string(&raw, "datasetId").unwrap_or(fallback_id);
        let row_count = extract_u64(&raw, "rowCount").unwrap_or(0);
        let column_count = extract_u64(&raw, "columnCount").unwrap_or(0);
        let label = extract_string(&raw, "label");
        let produced_by = extract_string(&raw, "producedBy").unwrap_or_else(|| "ingest".into());
        let parent = extract_string(&raw, "parentDatasetId");
        let source_kind = extract_string(&raw, "sourceKind").unwrap_or_else(|| "csv".into());
        out.push(format!(
            "{{\"datasetId\":{},\"rowCount\":{},\"columnCount\":{},\"label\":{},\"producedBy\":{},\"parentDatasetId\":{},\"sourceKind\":{}}}",
            json_string(&dataset_id),
            row_count,
            column_count,
            label.as_deref().map(json_string).unwrap_or_else(|| "null".into()),
            json_string(&produced_by),
            parent.as_deref().map(json_string).unwrap_or_else(|| "null".into()),
            json_string(&source_kind)
        ));
    }
    Ok(out)
}

fn extract_u64(raw: &str, key: &str) -> Option<u64> {
    let needle = format!("\"{key}\":");
    let idx = raw.find(&needle)?;
    let rest = raw[idx + needle.len()..].trim_start();
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

fn extract_string(raw: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":");
    let idx = raw.find(&needle)?;
    let rest = &raw[idx + needle.len()..];
    let rest = rest.trim_start();
    if rest.starts_with("null") {
        return None;
    }
    if !rest.starts_with('"') {
        return None;
    }
    let mut out = String::new();
    let mut chars = rest[1..].chars();
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
    Some(out)
}
