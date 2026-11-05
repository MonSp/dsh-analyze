//! Dataset helpers: workspace-confined paths + CSV tables.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Cursor, Write};
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
                    // -0.0 serializes as "-0"; normalize so JSON never carries -0.
                    if *v == 0.0 {
                        "0".to_string()
                    } else {
                        format!("{v}")
                    }
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

/// Resolve `candidate` inside `workspace` without requiring the target or its
/// intermediate directories to exist. Errors never carry raw OS text.
pub fn ensure_within_workspace(workspace: &Path, candidate: &Path) -> Result<PathBuf, String> {
    let ws = fs::canonicalize(workspace)
        .map_err(|_| "analysis/invalid-path: workspace unavailable".to_string())?;
    let abs = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        ws.join(candidate)
    };
    let norm = normalize_missing(&abs)?;
    if !norm.starts_with(&ws) {
        return Err("analysis/invalid-path: path escapes workspace".into());
    }
    Ok(norm)
}

/// Canonicalize the deepest existing ancestor, then re-attach the missing tail
/// with lexical `.`/`..` resolution so nested not-yet-created parents work.
fn normalize_missing(abs: &Path) -> Result<PathBuf, String> {
    if abs.exists() {
        return fs::canonicalize(abs).map_err(|_| "analysis/invalid-path: path not found".into());
    }
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = abs.to_path_buf();
    loop {
        let parent = match cur.parent() {
            Some(p) if p != cur => p.to_path_buf(),
            _ => return Err("analysis/invalid-path: invalid path".into()),
        };
        tail.push(
            cur.file_name()
                .map(|s| s.to_os_string())
                .ok_or("analysis/invalid-path: invalid path")?,
        );
        cur = parent;
        if cur.exists() {
            break;
        }
    }
    let mut base =
        fs::canonicalize(&cur).map_err(|_| "analysis/invalid-path: path not found".to_string())?;
    for comp in tail.iter().rev() {
        if comp == ".." {
            if !base.pop() {
                return Err("analysis/invalid-path: path escapes workspace".into());
            }
        } else if comp != "." {
            base.push(comp);
        }
    }
    Ok(base)
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

/// Current UTC time as ISO-8601 `YYYY-MM-DDTHH:MM:SSZ` (std only).
pub fn now_iso_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Parse an ISO-8601 UTC timestamp (`YYYY-MM-DD[THH:MM:SS][.fff][Z]`) into a
/// comparable `(y, mo, d, h, mi, s)` tuple. Lenient on fractional seconds.
pub fn parse_iso_utc_ts(s: &str) -> Option<(i64, u32, u32, u32, u32, u32)> {
    let t = s.trim();
    if t.len() < 10 {
        return None;
    }
    let bytes = t.as_bytes();
    let num = |a: usize, b: usize| -> Option<u32> { t.get(a..b)?.parse().ok() };
    if bytes.get(4) != Some(&b'-') || bytes.get(7) != Some(&b'-') {
        return None;
    }
    let y: i64 = t.get(0..4)?.parse().ok()?;
    let mo = num(5, 7)?;
    let d = num(8, 10)?;
    let rest = t.get(10..)?;
    if rest.is_empty() {
        return Some((y, mo, d, 0, 0, 0));
    }
    if !rest.starts_with('T') && !rest.starts_with('t') && !rest.starts_with(' ') {
        return None;
    }
    let rest = &rest[1..];
    if rest.len() < 8 {
        return None;
    }
    if rest.as_bytes().get(2) != Some(&b':') || rest.as_bytes().get(5) != Some(&b':') {
        return None;
    }
    let h = rest.get(0..2)?.parse().ok()?;
    let mi = rest.get(3..5)?.parse().ok()?;
    let se = rest.get(6..8)?.parse().ok()?;
    if mo < 1 || mo > 12 || d < 1 || d > 31 || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    Some((y, mo, d, h, mi, se))
}

/// Parse `--since` (`YYYY-MM-DD[THH:MM:SS]`) to a comparable tuple.
pub fn parse_since_arg(s: &str) -> Option<(i64, u32, u32, u32, u32, u32)> {
    let t = s.trim();
    let ok_date = t.len() >= 10
        && t.as_bytes().get(4) == Some(&b'-')
        && t.as_bytes().get(7) == Some(&b'-')
        && t.as_bytes()[0..4].iter().all(|b| b.is_ascii_digit())
        && t.as_bytes()[5..7].iter().all(|b| b.is_ascii_digit())
        && t.as_bytes()[8..10].iter().all(|b| b.is_ascii_digit());
    if !ok_date {
        return None;
    }
    if t.len() == 10 {
        return parse_iso_utc_ts(t);
    }
    let sep = t.as_bytes().get(10).copied()?;
    if sep != b'T' && sep != b' ' {
        return None;
    }
    let hms = &t[11..];
    if hms.len() != 8 || !hms.bytes().all(|b| b.is_ascii_digit() || b == b':') {
        return None;
    }
    parse_iso_utc_ts(&format!("{}T{}", &t[0..10], hms))
}

pub fn infer_dtype(values: &[Value]) -> String {
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

/// Stable reason for a failed file read; never leaks OS error text.
pub fn io_read_reason(e: &std::io::Error) -> &'static str {
    match e.kind() {
        std::io::ErrorKind::NotFound => "file not found",
        std::io::ErrorKind::PermissionDenied => "file not readable",
        _ => "read failed",
    }
}

pub fn parse_csv_path(path: &Path) -> Result<Table, String> {
    let file = fs::File::open(path)
        .map_err(|e| format!("analysis/cli-failed: cannot read file: {}", io_read_reason(&e)))?;
    parse_csv_reader(BufReader::new(file))
}

/// Parse CSV bytes already decoded to UTF-8 (R-30 `--encoding` read path).
/// Shares the exact line/BOM/quote semantics of [`parse_csv_path`].
pub fn parse_csv_bytes(bytes: &[u8]) -> Result<Table, String> {
    parse_csv_reader(Cursor::new(bytes))
}

fn parse_csv_reader<R: BufRead>(mut reader: R) -> Result<Table, String> {
    let mut header_line = String::new();
    let n = reader
        .read_line(&mut header_line)
        .map_err(|e| format!("analysis/cli-failed: cannot read file: {}", io_read_reason(&e)))?;
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
            .map_err(|e| format!("analysis/cli-failed: cannot read file: {}", io_read_reason(&e)))?;
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

/// Decode whole-file CSV bytes per `--encoding` mode (R-30 / S2.5).
/// Never lossy: malformed bytes are rejected, never replaced with U+FFFD.
/// A leading UTF-8 BOM is an encoding marker, not data — stripped before
/// every mode so it can never surface as garbage.
/// Returns `(utf-8 text, used_gb18030_fallback)`; the fallback flag is only
/// ever true for `auto`.
pub fn decode_csv_bytes(bytes: &[u8], encoding: &str) -> Result<(String, bool), String> {
    const PLAN_B: &str = "analysis/invalid-args: file is not valid UTF-8 (re-save as CSV UTF-8, or pass --encoding gbk)";
    let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(bytes);
    match encoding {
        "gbk" | "gb18030" => decode_gb18030(body).map(|s| (s, false)),
        "auto" => {
            // Strict UTF-8 first; fatal GB18030 fallback (S2.5) either way —
            // a BOM never suppresses the fallback (its declaration failed).
            if let Ok(s) = std::str::from_utf8(body) {
                return Ok((s.to_string(), false));
            }
            decode_gb18030(body)
                .map(|s| (s, true))
                .map_err(|_| PLAN_B.to_string())
        }
        // Default `utf-8`: strict, as before.
        _ => std::str::from_utf8(body)
            .map(|s| (s.to_string(), false))
            .map_err(|_| PLAN_B.to_string()),
    }
}

/// Fatal (no replacement) GB18030 decode. The WHATWG GBK decoder also covers
/// GB18030 four-byte forms, so `--encoding gbk` and `gb18030` share this path.
fn decode_gb18030(bytes: &[u8]) -> Result<String, String> {
    encoding_rs::GBK
        .decode_without_bom_handling_and_without_replacement(bytes)
        .map(|cow| cow.into_owned())
        .ok_or_else(|| "analysis/invalid-args: file is not valid GB18030".to_string())
}

pub fn parse_jsonl_path(path: &Path) -> Result<Table, String> {
    let file = fs::File::open(path)
        .map_err(|e| format!("analysis/cli-failed: cannot read file: {}", io_read_reason(&e)))?;
    let reader = BufReader::new(file);
    let mut rows_map: Vec<BTreeMap<String, Value>> = Vec::new();
    for line in reader.lines() {
        let line = line
            .map_err(|e| format!("analysis/cli-failed: cannot read file: {}", io_read_reason(&e)))?;
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

/// Read a Parquet file into a Table (R-22). Nested list/struct/map are rejected.
/// Errors are stable `analysis/*` strings (never raw OS / parquet Display leakage
/// beyond our own mapping).
pub fn parse_parquet_path(path: &Path) -> Result<Table, String> {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    let file = fs::File::open(path)
        .map_err(|e| format!("analysis/cli-failed: cannot read file: {}", io_read_reason(&e)))?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(|_| "analysis/cli-failed: not a valid parquet file".to_string())?;
    let schema = builder.schema().clone();

    // Reject nested types up front (T0: no list/struct/map).
    for field in schema.fields() {
        if is_nested_parquet_type(field.data_type()) {
            return Err(format!(
                "analysis/invalid-args: parquet nested type not supported for column {}",
                field.name()
            ));
        }
    }

    // Map Arrow schema → our dtypes + physical/logical info for value conversion.
    let mut col_names: Vec<String> = Vec::new();
    let mut col_dtypes: Vec<String> = Vec::new();
    let mut col_kinds: Vec<ParquetColKind> = Vec::new();
    for field in schema.fields() {
        col_names.push(field.name().clone());
        let (dtype, kind) = parquet_field_map(field.data_type()).map_err(|_| {
            format!(
                "analysis/invalid-args: parquet type not supported for column {}",
                field.name()
            )
        })?;
        col_dtypes.push(dtype);
        col_kinds.push(kind);
    }

    let reader = builder
        .build()
        .map_err(|_| "analysis/cli-failed: not a valid parquet file".to_string())?;

    let mut rows: Vec<Vec<Value>> = Vec::new();
    for batch in reader {
        let batch = batch.map_err(|_| "analysis/cli-failed: not a valid parquet file".to_string())?;
        let n = batch.num_rows();
        for r in 0..n {
            let mut row = Vec::with_capacity(col_names.len());
            for (c, kind) in col_kinds.iter().enumerate() {
                let col = batch.column(c);
                row.push(parquet_value_at(col, r, kind));
            }
            rows.push(row);
        }
    }
    // Schema dtypes are re-inferred by finish_table from values (parity with csv).
    let _ = col_dtypes;
    finish_table(col_names, rows)
}

#[derive(Debug, Clone, Copy)]
enum ParquetColKind {
    Bool,
    I64,
    F64,
    Str,
    /// TIMESTAMP_* rendered as UTC ISO-8601 string.
    Timestamp,
    /// DATE as `YYYY-MM-DD`.
    Date,
}

fn is_nested_parquet_type(dt: &arrow_schema::DataType) -> bool {
    use arrow_schema::DataType;
    matches!(
        dt,
        DataType::List(_)
            | DataType::LargeList(_)
            | DataType::FixedSizeList(_, _)
            | DataType::Struct(_)
            | DataType::Map(_, _)
            | DataType::Union(_, _)
    )
}

/// Map an Arrow type to our dtype + value kind. Anything outside this
/// allowlist (Decimal, Time, Duration, Interval, Dictionary, Float16, …) is
/// rejected by the caller — never silently read as null.
fn parquet_field_map(dt: &arrow_schema::DataType) -> Result<(String, ParquetColKind), ()> {
    use arrow_schema::DataType;
    Ok(match dt {
        DataType::Boolean => ("bool".into(), ParquetColKind::Bool),
        DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64 => {
            ("i64".into(), ParquetColKind::I64)
        }
        DataType::UInt8 | DataType::UInt16 | DataType::UInt32 | DataType::UInt64 => {
            ("i64".into(), ParquetColKind::I64)
        }
        DataType::Float32 | DataType::Float64 => ("f64".into(), ParquetColKind::F64),
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Binary | DataType::LargeBinary => {
            ("utf8".into(), ParquetColKind::Str)
        }
        DataType::Timestamp(_, _) => ("utf8".into(), ParquetColKind::Timestamp),
        DataType::Date32 | DataType::Date64 => ("utf8".into(), ParquetColKind::Date),
        DataType::Null => ("utf8".into(), ParquetColKind::Str),
        _ => return Err(()),
    })
}

fn parquet_value_at(col: &arrow_array::ArrayRef, row: usize, kind: &ParquetColKind) -> Value {
    use arrow_array::*;
    if col.is_null(row) {
        return Value::Null;
    }
    match kind {
        ParquetColKind::Bool => {
            if let Some(a) = col.as_any().downcast_ref::<BooleanArray>() {
                Value::Bool(a.value(row))
            } else {
                Value::Null
            }
        }
        ParquetColKind::I64 => {
            macro_rules! as_i64 {
                ($t:ty) => {{
                    if let Some(a) = col.as_any().downcast_ref::<$t>() {
                        Some(a.value(row) as i64)
                    } else {
                        None
                    }
                }};
            }
            let v = as_i64!(Int64Array)
                .or_else(|| as_i64!(Int32Array))
                .or_else(|| as_i64!(Int16Array))
                .or_else(|| as_i64!(Int8Array))
                .or_else(|| as_i64!(UInt64Array))
                .or_else(|| as_i64!(UInt32Array))
                .or_else(|| as_i64!(UInt16Array))
                .or_else(|| as_i64!(UInt8Array));
            v.map(Value::I64).unwrap_or(Value::Null)
        }
        ParquetColKind::F64 => {
            macro_rules! as_f64 {
                ($t:ty) => {{
                    if let Some(a) = col.as_any().downcast_ref::<$t>() {
                        Some(a.value(row) as f64)
                    } else {
                        None
                    }
                }};
            }
            let v = as_f64!(Float64Array).or_else(|| as_f64!(Float32Array));
            v.map(Value::F64).unwrap_or(Value::Null)
        }
        ParquetColKind::Str => {
            if let Some(a) = col.as_any().downcast_ref::<StringArray>() {
                Value::Str(a.value(row).to_string())
            } else if let Some(a) = col.as_any().downcast_ref::<BinaryArray>() {
                Value::Str(String::from_utf8_lossy(a.value(row)).into_owned())
            } else if let Some(a) = col.as_any().downcast_ref::<LargeStringArray>() {
                Value::Str(a.value(row).to_string())
            } else if let Some(a) = col.as_any().downcast_ref::<LargeBinaryArray>() {
                Value::Str(String::from_utf8_lossy(a.value(row)).into_owned())
            } else {
                Value::Null
            }
        }
        ParquetColKind::Timestamp => {
            macro_rules! ts_str {
                ($t:ty, $unit:expr) => {{
                    if let Some(a) = col.as_any().downcast_ref::<$t>() {
                        Some(format_ts_micros(a.value(row), $unit))
                    } else {
                        None
                    }
                }};
            }
            let v = ts_str!(TimestampMicrosecondArray, "us")
                .or_else(|| ts_str!(TimestampMillisecondArray, "ms"))
                .or_else(|| ts_str!(TimestampNanosecondArray, "ns"))
                .or_else(|| ts_str!(TimestampSecondArray, "s"));
            if let Some(s) = v {
                Value::Str(s)
            } else if let Some(a) = col.as_any().downcast_ref::<Int64Array>() {
                Value::Str(format_ts_micros(a.value(row), "us"))
            } else {
                Value::Null
            }
        }
        ParquetColKind::Date => {
            if let Some(a) = col.as_any().downcast_ref::<Date32Array>() {
                Value::Str(format_date32(a.value(row)))
            } else if let Some(a) = col.as_any().downcast_ref::<Date64Array>() {
                Value::Str(format_date32((a.value(row) / 86_400_000) as i32))
            } else {
                Value::Null
            }
        }
    }
}

fn format_ts_micros(v: i64, unit: &str) -> String {
    let micros = match unit {
        "s" => v.saturating_mul(1_000_000),
        "ms" => v.saturating_mul(1_000),
        "ns" => v / 1_000,
        _ => v,
    };
    let secs = micros.div_euclid(1_000_000);
    let frac = micros.rem_euclid(1_000_000);
    let (y, mo, d, h, mi, s) = civil_from_unix(secs);
    if frac == 0 {
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
    } else {
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{frac:06}Z")
    }
}

fn format_date32(days: i32) -> String {
    let secs = i64::from(days) * 86_400;
    let (y, mo, d, _, _, _) = civil_from_unix(secs);
    format!("{y:04}-{mo:02}-{d:02}")
}

/// Days-from-civil algorithm (Howard Hinnant) → UTC calendar parts.
fn civil_from_unix(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let h = (rem / 3600) as u32;
    let mi = ((rem % 3600) / 60) as u32;
    let s = (rem % 60) as u32;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, h, mi, s)
}

/// Parse a `--format json` file: top-level array `[{…},…]` or `{"records":[…]}`.
/// Columns are the key union in first-seen order; dtype rules match jsonl.
pub fn parse_json_path(path: &Path) -> Result<Table, String> {
    let text = fs::read_to_string(path)
        .map_err(|e| format!("analysis/cli-failed: cannot read file: {}", io_read_reason(&e)))?;
    parse_json_text(&text)
}

pub fn parse_json_text(text: &str) -> Result<Table, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let s = text.trim();
    let elements = json_top_level_records(s)?;
    let mut headers: Vec<String> = Vec::new();
    let mut rows: Vec<Vec<Value>> = Vec::new();
    for (i, el) in elements.iter().enumerate() {
        if !el.starts_with('{') || !el.ends_with('}') {
            return Err(format!(
                "analysis/invalid-args: bad json record at index {i}"
            ));
        }
        let rec = parse_json_object(el)
            .map_err(|_| "analysis/invalid-args: bad json file".to_string())?;
        // BTreeMap sorts keys; re-read key order from the record text.
        let order = record_key_order(el)?;
        let mut row: Vec<Value> = Vec::with_capacity(headers.len());
        for _ in 0..headers.len() {
            row.push(Value::Null);
        }
        for key in order {
            let v = rec.get(&key).cloned().unwrap_or(Value::Null);
            match headers.iter().position(|h| *h == key) {
                Some(ci) => row[ci] = v,
                None => {
                    headers.push(key);
                    row.push(v);
                }
            }
        }
        rows.push(row);
    }
    finish_table(headers, rows)
}

/// Split top-level `[{…},…]` / `{"records":[…]}` into element strings.
fn json_top_level_records(s: &str) -> Result<Vec<String>, String> {
    if s.starts_with('[') {
        return split_json_elements(s);
    }
    if s.starts_with('{') {
        return json_object_records(s);
    }
    Err("analysis/invalid-args: json must be a top-level array or {records:[…]}".into())
}

fn split_json_elements(arr: &str) -> Result<Vec<String>, String> {
    let inner = arr
        .trim()
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or("analysis/invalid-args: bad json file")?;
    let mut out = Vec::new();
    let mut depth = 0usize;
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
                if depth == 0 {
                    return Err("analysis/invalid-args: bad json file".into());
                }
                depth -= 1;
                cur.push(ch);
            }
            ',' if depth == 0 => {
                if !cur.trim().is_empty() {
                    out.push(std::mem::take(&mut cur).trim().to_string());
                }
                cur.clear();
            }
            c => cur.push(c),
        }
    }
    if depth != 0 {
        return Err("analysis/invalid-args: bad json file".into());
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    Ok(out)
}

/// Extract the `records` array elements from a top-level JSON object.
fn json_object_records(obj: &str) -> Result<Vec<String>, String> {
    let no_records =
        || "analysis/invalid-args: json must be a top-level array or {records:[…]}".to_string();
    let chars: Vec<char> = obj.chars().collect();
    let mut i = 1usize;
    loop {
        while i < chars.len() && (chars[i].is_whitespace() || chars[i] == ',') {
            i += 1;
        }
        if i >= chars.len() || chars[i] == '}' {
            return Err(no_records());
        }
        if chars[i] != '"' {
            return Err("analysis/invalid-args: bad json file".into());
        }
        let (key, ni) = read_json_string(&chars, i)?;
        i = ni;
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() || chars[i] != ':' {
            return Err("analysis/invalid-args: bad json file".into());
        }
        i += 1;
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        let val_start = i;
        let val_end = skip_json_value(&chars, i)?;
        if key == "records" {
            let val: String = chars[val_start..val_end].iter().collect();
            let t = val.trim();
            if !t.starts_with('[') {
                return Err(no_records());
            }
            return split_json_elements(t);
        }
        i = val_end;
    }
}

/// Advance past one JSON value starting at `i`; returns the end index.
fn skip_json_value(chars: &[char], start: usize) -> Result<usize, String> {
    let mut i = start;
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    if i >= chars.len() {
        return Err("analysis/invalid-args: bad json file".into());
    }
    match chars[i] {
        '"' => read_json_string(chars, i).map(|(_, n)| n),
        '{' | '[' => {
            let open = chars[i];
            let close = if open == '{' { '}' } else { ']' };
            let mut depth = 0usize;
            let mut in_str = false;
            let mut esc = false;
            while i < chars.len() {
                let c = chars[i];
                if in_str {
                    if esc {
                        esc = false;
                    } else if c == '\\' {
                        esc = true;
                    } else if c == '"' {
                        in_str = false;
                    }
                    i += 1;
                    continue;
                }
                if c == '"' {
                    in_str = true;
                } else if c == open {
                    depth += 1;
                } else if c == close {
                    depth -= 1;
                    i += 1;
                    if depth == 0 {
                        return Ok(i);
                    }
                    continue;
                }
                i += 1;
            }
            Err("analysis/invalid-args: bad json file".into())
        }
        't' => expect_lit(chars, i, "true").map(|_| i + 4),
        'f' => expect_lit(chars, i, "false").map(|_| i + 5),
        'n' => expect_lit(chars, i, "null").map(|_| i + 4),
        _ => {
            let mut j = i;
            while j < chars.len()
                && (chars[j].is_ascii_digit() || matches!(chars[j], '-' | '+' | '.' | 'e' | 'E'))
            {
                j += 1;
            }
            if j == i {
                return Err("analysis/invalid-args: bad json file".into());
            }
            Ok(j)
        }
    }
}

/// First-seen key order of a flat JSON object text.
fn record_key_order(el: &str) -> Result<Vec<String>, String> {
    let inner = el
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .ok_or("analysis/invalid-args: bad json file")?;
    let chars: Vec<char> = inner.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        while i < chars.len() && (chars[i].is_whitespace() || chars[i] == ',') {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        if chars[i] != '"' {
            return Err("analysis/invalid-args: bad json file".into());
        }
        let (key, ni) = read_json_string(&chars, i)?;
        i = ni;
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() || chars[i] != ':' {
            return Err("analysis/invalid-args: bad json file".into());
        }
        i += 1;
        i = skip_json_value(&chars, i)?;
        if !out.contains(&key) {
            out.push(key);
        }
    }
    Ok(out)
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
    let i = start;
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
            "{{\"version\":1,\"datasetId\":{},\"rowCount\":{},\"columnCount\":{},\"columns\":[{}],\"sourceKind\":{},\"sourcePath\":{},\"label\":{},\"producedBy\":{},\"parentDatasetId\":{},\"createdAt\":{}}}",
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
            json_string(&now_iso_utc()),
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
    fs::create_dir_all(&dir).map_err(|_| "analysis/cli-failed: write failed".to_string())?;
    let path = dir.join(format!("{id}.csv"));
    let mut f = fs::File::create(&path).map_err(|_| "analysis/cli-failed: write failed".to_string())?;
    let header = table
        .columns
        .iter()
        .map(|c| csv_escape(&c.name))
        .collect::<Vec<_>>()
        .join(",");
    writeln!(f, "{header}").map_err(|_| "analysis/cli-failed: write failed".to_string())?;
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
        writeln!(f, "{line}").map_err(|_| "analysis/cli-failed: write failed".to_string())?;
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
    fs::create_dir_all(&dir).map_err(|_| "analysis/cli-failed: write failed".to_string())?;
    let path = dir.join(format!("{}.meta.json", meta.dataset_id));
    fs::write(path, format!("{}\n", meta.to_json()))
        .map_err(|_| "analysis/cli-failed: write failed".to_string())
}

pub fn load_dataset(workspace: &Path, id: &str) -> Result<(Meta, Table), String> {
    validate_dataset_id(id)?;
    let meta_path = datasets_dir(workspace).join(format!("{id}.meta.json"));
    let csv_path = datasets_dir(workspace).join(format!("{id}.csv"));
    if !meta_path.exists() || !csv_path.exists() {
        return Err("analysis/unknown-dataset".into());
    }
    let meta_raw = fs::read_to_string(&meta_path)
        .map_err(|e| format!("analysis/cli-failed: cannot read file: {}", io_read_reason(&e)))?;
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

/// One `list` summary row. `created_at` is nullable (absent on old datasets).
pub struct DatasetSummary {
    pub dataset_id: String,
    pub row_count: u64,
    pub column_count: u64,
    pub label: Option<String>,
    pub produced_by: String,
    pub parent_dataset_id: Option<String>,
    pub source_kind: String,
    pub created_at: Option<String>,
}

impl DatasetSummary {
    pub fn to_json(&self) -> String {
        format!(
            "{{\"datasetId\":{},\"rowCount\":{},\"columnCount\":{},\"label\":{},\"producedBy\":{},\"parentDatasetId\":{},\"sourceKind\":{},\"createdAt\":{}}}",
            json_string(&self.dataset_id),
            self.row_count,
            self.column_count,
            self.label
                .as_deref()
                .map(json_string)
                .unwrap_or_else(|| "null".into()),
            json_string(&self.produced_by),
            self.parent_dataset_id
                .as_deref()
                .map(json_string)
                .unwrap_or_else(|| "null".into()),
            json_string(&self.source_kind),
            self.created_at
                .as_deref()
                .map(json_string)
                .unwrap_or_else(|| "null".into()),
        )
    }
}

/// Summarize every `*.meta.json` under the datasets dir (sorted by datasetId).
pub fn list_dataset_summaries(workspace: &Path) -> Result<Vec<DatasetSummary>, String> {
    let dir = datasets_dir(workspace);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut names: Vec<String> = fs::read_dir(&dir)
        .map_err(|_| "analysis/cli-failed: cannot read file: read failed".to_string())?
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
            .map_err(|e| format!("analysis/cli-failed: cannot read file: {}", io_read_reason(&e)))?;
        let fallback_id = name.trim_end_matches(".meta.json").to_string();
        out.push(DatasetSummary {
            dataset_id: extract_string(&raw, "datasetId").unwrap_or(fallback_id),
            row_count: extract_u64(&raw, "rowCount").unwrap_or(0),
            column_count: extract_u64(&raw, "columnCount").unwrap_or(0),
            label: extract_string(&raw, "label"),
            produced_by: extract_string(&raw, "producedBy").unwrap_or_else(|| "ingest".into()),
            parent_dataset_id: extract_string(&raw, "parentDatasetId"),
            source_kind: extract_string(&raw, "sourceKind").unwrap_or_else(|| "csv".into()),
            created_at: extract_string(&raw, "createdAt"),
        });
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
