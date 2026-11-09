//! Integration tests: export dirs, ingest formats, head/values clamps, help, list filters.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_dsh-analyze"))
}

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_workspace() -> PathBuf {
    let n = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "dsh-analyze-periphery-{}-{n}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(dir.join("data")).unwrap();
    dir
}

fn write_file(ws: &Path, name: &str, body: &str) {
    fs::write(ws.join("data").join(name), body).unwrap();
}

fn run_ok(ws: &Path, args: &[&str]) -> String {
    let out = bin()
        .arg("--workspace")
        .arg(ws)
        .args(args)
        .output()
        .expect("spawn dsh-analyze");
    assert!(
        out.status.success(),
        "command failed: {args:?}\nstderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn run_fail(ws: &Path, args: &[&str], expect_code: i32) -> String {
    let out = bin()
        .arg("--workspace")
        .arg(ws)
        .args(args)
        .output()
        .expect("spawn dsh-analyze");
    assert_eq!(
        out.status.code(),
        Some(expect_code),
        "expected exit {expect_code} for {args:?}\nstderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn json_usize_field(line: &str, field: &str) -> Option<usize> {
    let needle = format!("\"{field}\":");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

fn ingest(ws: &Path, file: &str, id: &str) -> String {
    run_ok(
        ws,
        &["ingest", "--input", &format!("data/{file}"), "--dataset-id", id],
    )
}

// ---------------------------------------------------------------- R-2 export

#[test]
fn export_nested_output_succeeds() {
    let ws = temp_workspace();
    write_file(&ws, "d.csv", "id\n1\n");
    ingest(&ws, "d.csv", "ds_aabbccdd11223344");

    // Missing nested parents must not fail (R-2); file lands under outputs/.
    let out = run_ok(
        &ws,
        &[
            "export",
            "--dataset-id",
            "ds_aabbccdd11223344",
            "--output",
            "outputs/nested/a.csv",
        ],
    );
    assert!(out.contains("\"ok\":true"), "{out}");
    assert!(ws.join(".dsh-analyze/outputs/a.csv").exists(), "got: {out}");

    // Deep nesting under the forced outputs root keeps its structure.
    let out2 = run_ok(
        &ws,
        &[
            "export",
            "--dataset-id",
            "ds_aabbccdd11223344",
            "--output",
            ".dsh-analyze/outputs/nested/deep/b.csv",
        ],
    );
    assert!(out2.contains("\"ok\":true"), "{out2}");
    assert!(ws.join(".dsh-analyze/outputs/nested/deep/b.csv").exists());

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn export_escape_is_invalid_path() {
    let ws = temp_workspace();
    write_file(&ws, "d.csv", "id\n1\n");
    ingest(&ws, "d.csv", "ds_aabbccdd11223344");

    let err = run_fail(
        &ws,
        &[
            "export",
            "--dataset-id",
            "ds_aabbccdd11223344",
            "--output",
            "../escape.csv",
        ],
        2,
    );
    assert!(
        err.contains("analysis/invalid-path: path escapes workspace"),
        "stderr={err}"
    );
    assert!(!err.contains("os error"), "no OS text: {err}");

    let _ = fs::remove_dir_all(&ws);
}

// ------------------------------------------------------------- R-23 export formats

#[test]
fn export_csv_xlsx_parquet_same_shape_and_bad_format() {
    let ws = temp_workspace();
    write_file(&ws, "d.csv", "id,amount,ok\n1,10.5,true\n2,20,false\n3,,\n");
    let out = ingest(&ws, "d.csv", "ds_aabbccdd11223344");
    assert!(out.contains("\"ok\":true"), "{out}");

    let csv = run_ok(
        &ws,
        &["export", "--dataset-id", "ds_aabbccdd11223344", "--output", "outputs/r.csv"],
    );
    assert!(csv.contains("\"format\":\"csv\"") || csv.contains("\"ok\":true"), "{csv}");
    let csv_body = fs::read_to_string(ws.join(".dsh-analyze/outputs/r.csv")).unwrap();
    assert!(csv_body.starts_with("id,amount,ok"), "{csv_body}");
    assert_eq!(csv_body.lines().count(), 4); // header + 3

    let xlsx = run_ok(
        &ws,
        &["export", "--dataset-id", "ds_aabbccdd11223344", "--output", "outputs/r.xlsx"],
    );
    assert!(xlsx.contains("\"ok\":true"), "{xlsx}");
    let xlsx_path = ws.join(".dsh-analyze/outputs/r.xlsx");
    assert!(xlsx_path.exists());
    // xlsx is a zip (PK header)
    let magic = fs::read(&xlsx_path).unwrap();
    assert_eq!(&magic[..2], b"PK", "xlsx zip magic");

    let pq = run_ok(
        &ws,
        &[
            "export",
            "--dataset-id",
            "ds_aabbccdd11223344",
            "--output",
            "outputs/r.parquet",
            "--format",
            "parquet",
        ],
    );
    assert!(pq.contains("\"ok\":true"), "{pq}");
    let pq_path = ws.join(".dsh-analyze/outputs/r.parquet");
    assert!(pq_path.exists());

    // Round-trip parquet → ingest
    // Copy into data/ for ingest confinement
    fs::copy(&pq_path, ws.join("data").join("rt.parquet")).unwrap();
    let back = run_ok(&ws, &["ingest", "--input", "data/rt.parquet"]);
    assert!(back.contains("\"rowCount\":3"), "{back}");
    assert!(back.contains("\"columnCount\":3"), "{back}");

    // Same rows/cols: schema names match
    let sch = run_ok(
        &ws,
        &["schema", "--dataset-id", "ds_aabbccdd11223344"],
    );
    assert!(sch.contains("\"name\":\"id\""), "{sch}");

    // Invalid format → exit 2
    let err = run_fail(
        &ws,
        &[
            "export",
            "--dataset-id",
            "ds_aabbccdd11223344",
            "--output",
            "outputs/bad.csv",
            "--format",
            "xml",
        ],
        2,
    );
    assert!(err.contains("analysis/invalid-args: bad format"), "stderr={err}");

    let _ = fs::remove_dir_all(&ws);
}

// ------------------------------------------------------------- R-11/R-17 json

#[test]
fn ingest_json_array_and_records_object() {
    let ws = temp_workspace();
    write_file(
        &ws,
        "arr.json",
        "[{\"id\":1,\"amount\":10},{\"id\":2,\"amount\":\"x\"}]",
    );
    let out = ingest(&ws, "arr.json", "ds_aabbccdd11223344");
    assert_eq!(json_usize_field(&out, "rowCount"), Some(2), "{out}");
    assert_eq!(json_usize_field(&out, "columnCount"), Some(2), "{out}");
    // key union in first-seen order, mixed dtype promotes to utf8.
    assert!(
        out.contains("{\"name\":\"id\",\"dtype\":\"i64\"}")
            && out.contains("{\"name\":\"amount\",\"dtype\":\"utf8\"}"),
        "{out}"
    );

    write_file(&ws, "rec.json", "{\"records\":[{\"a\":1},{\"a\":2,\"b\":true}]}");
    let out2 = ingest(&ws, "rec.json", "ds_aabbccdd11223355");
    assert_eq!(json_usize_field(&out2, "rowCount"), Some(2), "{out2}");
    assert_eq!(json_usize_field(&out2, "columnCount"), Some(2), "{out2}");
    assert!(out2.contains("{\"name\":\"a\"") && out2.contains("{\"name\":\"b\""), "{out2}");

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn ingest_json_bad_record_names_index() {
    let ws = temp_workspace();
    write_file(&ws, "bad.json", "[{\"a\":1},2]");
    let err = run_fail(
        &ws,
        &["ingest", "--input", "data/bad.json", "--format", "json"],
        2,
    );
    assert!(
        err.contains("analysis/invalid-args: bad json record at index 1"),
        "stderr={err}"
    );

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn ingest_xlsx_still_limits_parquet_rejects_corrupt() {
    let ws = temp_workspace();
    write_file(&ws, "w.xlsx", "dummy");
    write_file(&ws, "w.parquet", "dummy");

    // R-31: dummy xlsx is corrupt → exit 2 actionable, not exit 4.
    let err = run_fail(
        &ws,
        &["ingest", "--input", "data/w.xlsx", "--format", "xlsx"],
        2,
    );
    assert!(
        err.contains("analysis/invalid-args: cannot read xlsx"),
        "stderr={err}"
    );
    assert!(!err.to_lowercase().contains("os error"), "stderr={err}");

    // .xlsm stays a truly-unsupported format → exit 4.
    write_file(&ws, "w.xlsm", "dummy");
    let err_xlsm = run_fail(
        &ws,
        &["ingest", "--input", "data/w.xlsm", "--format", "xlsm"],
        4,
    );
    assert!(
        err_xlsm.contains("analysis/limit: .xlsm not supported"),
        "stderr={err_xlsm}"
    );

    // Non-parquet masquerade → stable analysis/*, no OS text, exit 1
    let err2 = run_fail(&ws, &["ingest", "--input", "data/w.parquet"], 1);
    assert!(
        err2.contains("analysis/cli-failed: not a valid parquet file"),
        "stderr={err2}"
    );
    assert!(!err2.to_lowercase().contains("os error"), "stderr={err2}");

    let _ = fs::remove_dir_all(&ws);
}

/// Build a small parquet in the workspace via the export path of a temp CSV…
/// Actually write parquet with the parquet crate directly in the test.
#[test]
fn ingest_parquet_types_and_roundtrip() {
    use arrow_array::{BooleanArray, Float64Array, Int64Array, StringArray, TimestampMicrosecondArray};
    use arrow_schema::{DataType, Field, Schema, TimeUnit};
    use parquet::arrow::ArrowWriter;
    use std::sync::Arc;

    let ws = temp_workspace();
    let path = ws.join("data").join("types.parquet");

    let schema = Schema::new(vec![
        Field::new("i", DataType::Int64, true),
        Field::new("f", DataType::Float64, true),
        Field::new("b", DataType::Boolean, true),
        Field::new("s", DataType::Utf8, true),
        Field::new(
            "ts",
            DataType::Timestamp(TimeUnit::Microsecond, None),
            true,
        ),
    ]);
    let batch = arrow_array::RecordBatch::try_new(
        Arc::new(schema.clone()),
        vec![
            Arc::new(Int64Array::from(vec![Some(1i64), Some(2), None])),
            Arc::new(Float64Array::from(vec![Some(1.5f64), None, Some(3.25)])),
            Arc::new(BooleanArray::from(vec![Some(true), Some(false), None])),
            Arc::new(StringArray::from(vec![Some("a"), None, Some("c")])),
            Arc::new(TimestampMicrosecondArray::from(vec![
                Some(1_700_000_000_000_000i64),
                None,
                Some(1_700_000_001_000_000),
            ])),
        ],
    )
    .unwrap();

    let file = fs::File::create(&path).unwrap();
    let mut writer = ArrowWriter::try_new(file, Arc::new(schema), None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();

    let out = run_ok(
        &ws,
        &["ingest", "--input", "data/types.parquet", "--label", "t"],
    );
    assert!(out.contains("\"ok\":true"), "{out}");
    assert!(out.contains("\"name\":\"i\""), "{out}");
    assert!(out.contains("\"dtype\":\"i64\""), "{out}");
    assert!(out.contains("\"dtype\":\"f64\""), "{out}");
    assert!(out.contains("\"dtype\":\"bool\""), "{out}");
    assert!(out.contains("\"dtype\":\"utf8\""), "{out}");

    // Extract datasetId
    let id = out
        .split("\"datasetId\":\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .expect("datasetId")
        .to_string();

    let head = run_ok(&ws, &["head", "--dataset-id", &id, "--n", "3"]);
    assert!(head.contains("1.5") || head.contains("1"), "{head}");
    assert!(head.contains("2023-11-14T") || head.contains("2023-11-1"), "ts readable: {head}");

    let schema_out = run_ok(&ws, &["schema", "--dataset-id", &id]);
    assert!(schema_out.contains("\"rowCount\":3"), "{schema_out}");

    let _ = fs::remove_dir_all(&ws);
}

/// Decimal128 must be rejected explicitly — never silently read as null.
#[test]
fn ingest_parquet_unsupported_decimal_rejected() {
    use arrow_array::{Decimal128Array, RecordBatch};
    use arrow_schema::{DataType, Field, Schema};
    use parquet::arrow::ArrowWriter;
    use std::sync::Arc;

    let ws = temp_workspace();
    let path = ws.join("data").join("dec.parquet");

    let schema = Schema::new(vec![Field::new(
        "price",
        DataType::Decimal128(10, 2),
        true,
    )]);
    let batch = RecordBatch::try_new(
        Arc::new(schema.clone()),
        vec![Arc::new(
            Decimal128Array::from(vec![12_345i128, 67_890, 11_111])
                .with_precision_and_scale(10, 2)
                .unwrap(),
        )],
    )
    .unwrap();

    let file = fs::File::create(&path).unwrap();
    let mut writer = ArrowWriter::try_new(file, Arc::new(schema), None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();

    let err = run_fail(&ws, &["ingest", "--input", "data/dec.parquet"], 2);
    assert!(
        err.contains(
            "analysis/invalid-args: parquet type not supported for column price"
        ),
        "stderr={err}"
    );

    let _ = fs::remove_dir_all(&ws);
}

/// Empty table parquet export must honor declared dtypes, not all-Boolean.
#[test]
fn export_parquet_empty_table_keeps_schema_dtypes() {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    let ws = temp_workspace();
    write_file(&ws, "empty.csv", "a,b,c\n");
    let out = ingest(&ws, "empty.csv", "ds_aabbccdd11223344");
    assert!(out.contains("\"rowCount\":0"), "{out}");

    let pq = run_ok(
        &ws,
        &[
            "export",
            "--dataset-id",
            "ds_aabbccdd11223344",
            "--output",
            "outputs/e.parquet",
            "--format",
            "parquet",
        ],
    );
    assert!(pq.contains("\"ok\":true"), "{pq}");

    let file = fs::File::open(ws.join(".dsh-analyze/outputs/e.parquet")).unwrap();
    let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
    let schema = builder.schema();
    assert_eq!(schema.fields().len(), 3);
    for field in schema.fields() {
        assert_eq!(
            field.data_type(),
            &arrow_schema::DataType::Utf8,
            "empty export column {} should keep utf8, got {:?}",
            field.name(),
            field.data_type()
        );
    }

    let _ = fs::remove_dir_all(&ws);
}

/// K3: an all-null column has no value evidence either — export must honor
/// the declared dtype (utf8 here), not collapse to Boolean.
#[test]
fn export_parquet_all_null_column_uses_declared_dtype() {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    let ws = temp_workspace();
    write_file(&ws, "an.csv", "a,b\n1,\n2,\n3,\n");
    let out = ingest(&ws, "an.csv", "ds_aabbccdd11223344");
    assert!(out.contains("\"name\":\"b\",\"dtype\":\"utf8\""), "{out}");

    let pq = run_ok(
        &ws,
        &[
            "export",
            "--dataset-id",
            "ds_aabbccdd11223344",
            "--output",
            "outputs/an.parquet",
            "--format",
            "parquet",
        ],
    );
    assert!(pq.contains("\"ok\":true"), "{pq}");

    let file = fs::File::open(ws.join(".dsh-analyze/outputs/an.parquet")).unwrap();
    let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
    let schema = builder.schema();
    let b = schema.field_with_name("b").unwrap();
    assert_eq!(
        b.data_type(),
        &arrow_schema::DataType::Utf8,
        "all-null column should keep declared utf8, got {:?}",
        b.data_type()
    );

    let _ = fs::remove_dir_all(&ws);
}

/// `--format` is case-insensitive on ingest and export; `XLSX` hits the
/// exit-4 limit (not `bad format`).
#[test]
fn format_flags_are_case_insensitive() {
    let ws = temp_workspace();
    write_file(&ws, "d.csv", "a,b\n1,2\n");
    write_file(&ws, "w.xlsx", "dummy");

    let out = run_ok(
        &ws,
        &[
            "ingest",
            "--input",
            "data/d.csv",
            "--format",
            "CSV",
            "--dataset-id",
            "ds_aabbccdd11223344",
        ],
    );
    assert!(out.contains("\"ok\":true"), "{out}");

    // R-31: case-insensitive XLSX reaches the xlsx reader; dummy file → exit 2.
    let err = run_fail(
        &ws,
        &["ingest", "--input", "data/w.xlsx", "--format", "XLSX"],
        2,
    );
    assert!(
        err.contains("analysis/invalid-args: cannot read xlsx"),
        "stderr={err}"
    );

    let pq = run_ok(
        &ws,
        &[
            "export",
            "--dataset-id",
            "ds_aabbccdd11223344",
            "--output",
            "outputs/c.parquet",
            "--format",
            "PARQUET",
        ],
    );
    assert!(pq.contains("\"format\":\"parquet\""), "{pq}");

    let _ = fs::remove_dir_all(&ws);
}

/// Integers beyond 2^53 export to xlsx as text cells (no silent precision
/// loss); the file must still be written as a valid workbook.
#[test]
fn export_xlsx_large_int_writes_text_cell() {
    let ws = temp_workspace();
    write_file(&ws, "big.csv", "n\n9007199254740993\n");
    ingest(&ws, "big.csv", "ds_aabbccdd11223344");

    let out = run_ok(
        &ws,
        &[
            "export",
            "--dataset-id",
            "ds_aabbccdd11223344",
            "--output",
            "outputs/big.xlsx",
            "--format",
            "xlsx",
        ],
    );
    assert!(out.contains("\"ok\":true"), "{out}");
    let path = ws.join(".dsh-analyze/outputs/big.xlsx");
    let magic = fs::read(&path).unwrap();
    assert_eq!(&magic[..2], b"PK", "xlsx zip magic");

    let _ = fs::remove_dir_all(&ws);
}

// ------------------------------------------------------- R-12 head / R-13 values

#[test]
fn head_reports_requested_n_and_clamps() {
    let ws = temp_workspace();
    let mut body = String::from("id\n");
    for i in 0..120 {
        body.push_str(&format!("{i}\n"));
    }
    write_file(&ws, "d.csv", &body);
    ingest(&ws, "d.csv", "ds_aabbccdd11223344");

    let out50 = run_ok(&ws, &["head", "--dataset-id", "ds_aabbccdd11223344", "--n", "50"]);
    assert_eq!(json_usize_field(&out50, "requestedN"), Some(50), "{out50}");
    assert_eq!(json_usize_field(&out50, "n"), Some(50), "{out50}");
    assert!(!out50.contains("warnings"), "{out50}");

    let out500 = run_ok(&ws, &["head", "--dataset-id", "ds_aabbccdd11223344", "--n", "500"]);
    assert_eq!(json_usize_field(&out500, "requestedN"), Some(500), "{out500}");
    assert_eq!(json_usize_field(&out500, "n"), Some(100), "{out500}");
    assert!(
        out500.contains("{\"code\":\"clamp\",\"field\":\"n\",\"requested\":500,\"actual\":100}"),
        "{out500}"
    );

    let err = run_fail(
        &ws,
        &["head", "--dataset-id", "ds_aabbccdd11223344", "--n", "0"],
        2,
    );
    assert!(err.contains("--n must be integer >= 1"), "stderr={err}");

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn values_reports_requested_limit_and_clamps() {
    let ws = temp_workspace();
    let mut body = String::from("v\n");
    for i in 0..5 {
        body.push_str(&format!("{i}\n"));
    }
    write_file(&ws, "d.csv", &body);
    ingest(&ws, "d.csv", "ds_aabbccdd11223344");

    let out = run_ok(
        &ws,
        &[
            "values",
            "--dataset-id",
            "ds_aabbccdd11223344",
            "--column",
            "v",
            "--limit",
            "500",
        ],
    );
    assert_eq!(json_usize_field(&out, "requestedLimit"), Some(500), "{out}");
    assert_eq!(json_usize_field(&out, "limit"), Some(100), "{out}");
    assert!(
        out.contains(
            "{\"code\":\"clamp\",\"field\":\"limit\",\"requested\":500,\"actual\":100}"
        ),
        "{out}"
    );

    let err = run_fail(
        &ws,
        &[
            "values",
            "--dataset-id",
            "ds_aabbccdd11223344",
            "--column",
            "v",
            "--limit",
            "0",
        ],
        2,
    );
    assert!(err.contains("--limit must be integer >= 1"), "stderr={err}");

    let _ = fs::remove_dir_all(&ws);
}

// -------------------------------------------------------------- R-14 --help

#[test]
fn help_states_ops_forms_and_formats() {
    let out = bin().arg("--help").output().expect("spawn");
    assert!(out.status.success(), "--help must exit 0");
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("--ops accepts a JSON array OR a workspace path"), "{text}");
    assert!(text.contains("--spec likewise"), "{text}");
    assert!(text.contains("--format csv|json|jsonl|parquet|xlsx"), "{text}");
    assert!(text.contains(".xlsm reports analysis/limit"), "{text}");
    for cmd in ["ingest", "union", "diff", "pivot", "list", "export", "verify"] {
        assert!(text.contains(cmd), "missing {cmd} in usage: {text}");
    }

    let out_h = bin().arg("-h").output().expect("spawn");
    assert!(out_h.status.success(), "-h must exit 0");
    let text_h = String::from_utf8(out_h.stdout).unwrap();
    assert!(text_h.contains("JSON array OR a workspace path"), "{text_h}");

    let out_none = bin().output().expect("spawn");
    assert!(out_none.status.success(), "no args must exit 0");
    assert!(
        String::from_utf8(out_none.stdout).unwrap().contains("Usage"),
        "no args prints usage"
    );
}

// ------------------------------------------------------- R-19 list filters

#[test]
fn list_filters_and_created_at() {
    let ws = temp_workspace();
    write_file(&ws, "a.csv", "id\n1\n");
    write_file(&ws, "b.csv", "id\n2\n");
    ingest(&ws, "a.csv", "ds_aabbccdd11223344");
    run_ok(
        &ws,
        &[
            "ingest",
            "--input",
            "data/b.csv",
            "--dataset-id",
            "ds_aabbccdd11223355",
            "--label",
            "Sales East",
        ],
    );

    let all = run_ok(&ws, &["list"]);
    assert!(all.contains("\"createdAt\":\""), "createdAt present: {all}");
    assert_eq!(all.matches("\"datasetId\":").count(), 2, "{all}");

    let by_label = run_ok(&ws, &["list", "--label-like", "sales ea"]);
    assert!(by_label.contains("ds_aabbccdd11223355"), "{by_label}");
    assert!(!by_label.contains("ds_aabbccdd11223344"), "{by_label}");

    let by_pb = run_ok(&ws, &["list", "--produced-by", "ingest"]);
    assert_eq!(by_pb.matches("\"datasetId\":").count(), 2, "{by_pb}");
    let none = run_ok(&ws, &["list", "--produced-by", "join"]);
    assert_eq!(none.matches("\"datasetId\":").count(), 0, "{none}");

    let since_future = run_ok(&ws, &["list", "--since", "2999-01-01T00:00:00"]);
    assert_eq!(since_future.matches("\"datasetId\":").count(), 0, "{since_future}");
    let since_past = run_ok(&ws, &["list", "--since", "2000-01-01"]);
    assert_eq!(since_past.matches("\"datasetId\":").count(), 2, "{since_past}");

    // combined AND semantics
    let combined = run_ok(
        &ws,
        &[
            "list",
            "--label-like",
            "sales",
            "--produced-by",
            "ingest",
            "--since",
            "2000-01-01",
        ],
    );
    assert!(combined.contains("ds_aabbccdd11223355"), "{combined}");
    assert!(!combined.contains("ds_aabbccdd11223344"), "{combined}");

    let err = run_fail(&ws, &["list", "--since", "tomorrow"], 2);
    assert!(err.contains("--since must be YYYY-MM-DD[THH:MM:SS]"), "stderr={err}");

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn list_since_excludes_datasets_without_created_at() {
    let ws = temp_workspace();
    write_file(&ws, "a.csv", "id\n1\n");
    ingest(&ws, "a.csv", "ds_aabbccdd11223344");

    // Simulate a pre-createdAt dataset by stripping the field from meta.
    let meta_path = ws
        .join(".dsh-analyze")
        .join("datasets")
        .join("ds_aabbccdd11223344.meta.json");
    let raw = fs::read_to_string(&meta_path).unwrap();
    let idx = raw.find(",\"createdAt\":").expect("createdAt in meta");
    let end = idx + raw[idx..].find('}').unwrap();
    let stripped = format!("{}{}", &raw[..idx], &raw[end..]);
    fs::write(&meta_path, stripped).unwrap();

    let all = run_ok(&ws, &["list"]);
    assert!(all.contains("ds_aabbccdd11223344"), "{all}");
    assert!(all.contains("\"createdAt\":null"), "{all}");

    let since = run_ok(&ws, &["list", "--since", "2000-01-01"]);
    assert_eq!(since.matches("\"datasetId\":").count(), 0, "{since}");

    let _ = fs::remove_dir_all(&ws);
}

// ---------------------------------------------------------- capabilities (S2.0)

#[test]
fn version_capabilities_include_new_commands() {
    let out = {
        let o = bin().arg("version").output().expect("spawn");
        assert!(o.status.success());
        String::from_utf8(o.stdout).unwrap()
    };
    assert!(out.contains("\"version\":\"0.1.12\""), "{out}");
    for tok in ["\"union\"", "\"diff\"", "\"pivot\"", "\"verify\""] {
        assert!(out.contains(tok), "missing {tok}: {out}");
    }
}

// ------------------------------------------------------------- R-30 encoding

fn write_bytes(ws: &Path, name: &str, bytes: &[u8]) {
    fs::write(ws.join("data").join(name), bytes).unwrap();
}

/// GBK/GB18030 bytes for the §7.2 Chinese fixture (generated, not hardcoded).
fn gbk_fixture(text: &str) -> Vec<u8> {
    let (bytes, _, had_errors) = encoding_rs::GBK.encode(text);
    assert!(!had_errors, "fixture text must be GBK-encodable: {text}");
    bytes.into_owned()
}

const GBK_FIXTURE: &str = "地区,金额\n东省,100\n西省,200\n";
const PLAN_B_UTF8: &str =
    "analysis/invalid-args: file is not valid UTF-8 (re-save as CSV UTF-8, or pass --encoding gbk)";

#[test]
fn ingest_gb18030_default_utf8_rejects_with_plan_b_message() {
    let ws = temp_workspace();
    let bytes = gbk_fixture(GBK_FIXTURE);
    // Fixture sanity: invalid UTF-8, valid GB18030 (never lossy — both checked).
    assert!(std::str::from_utf8(&bytes).is_err(), "fixture must not be UTF-8");
    assert!(
        encoding_rs::GBK
            .decode_without_bom_handling_and_without_replacement(&bytes)
            .is_some(),
        "fixture must decode as GB18030"
    );
    write_bytes(&ws, "gbk.csv", &bytes);

    let err = run_fail(&ws, &["ingest", "--input", "data/gbk.csv"], 2);
    assert!(
        err.contains(PLAN_B_UTF8),
        "exact Plan B message expected, stderr={err}"
    );
    // No dataset must be written on decode failure.
    let list = run_ok(&ws, &["list"]);
    assert_eq!(list.matches("\"datasetId\":").count(), 0, "{list}");

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn ingest_gb18030_gbk_gb18030_and_auto_succeed_auto_warns() {
    let ws = temp_workspace();
    write_bytes(&ws, "gbk.csv", &gbk_fixture(GBK_FIXTURE));

    // (b) --encoding gb18030 (uppercase: flag value is case-insensitive)
    let out = run_ok(
        &ws,
        &[
            "ingest",
            "--input",
            "data/gbk.csv",
            "--encoding",
            "GB18030",
            "--dataset-id",
            "ds_gbk000000000001",
        ],
    );
    assert_eq!(json_usize_field(&out, "rowCount"), Some(2), "{out}");
    assert!(!out.contains("warnings"), "explicit mode must not warn: {out}");
    let head = run_ok(&ws, &["head", "--dataset-id", "ds_gbk000000000001", "--n", "5"]);
    assert!(head.contains("地区") && head.contains("金额"), "header: {head}");
    assert!(head.contains("东省") && head.contains("西省"), "values: {head}");
    assert!(head.contains("100") && head.contains("200"), "amounts: {head}");

    // (c) --encoding gbk — same decoder path, no warning
    let out2 = run_ok(
        &ws,
        &[
            "ingest",
            "--input",
            "data/gbk.csv",
            "--encoding",
            "gbk",
            "--dataset-id",
            "ds_gbk000000000002",
        ],
    );
    assert_eq!(json_usize_field(&out2, "rowCount"), Some(2), "{out2}");
    assert!(!out2.contains("warnings"), "explicit gbk must not warn: {out2}");

    // (d) --encoding auto → GB18030 fallback with encoding-fallback warning
    let out3 = run_ok(
        &ws,
        &[
            "ingest",
            "--input",
            "data/gbk.csv",
            "--encoding",
            "auto",
            "--dataset-id",
            "ds_gbk000000000003",
        ],
    );
    assert_eq!(json_usize_field(&out3, "rowCount"), Some(2), "{out3}");
    assert!(
        out3.contains(
            "\"warnings\":[{\"code\":\"encoding-fallback\",\"encoding\":\"gb18030\"}]"
        ),
        "auto must warn on fallback: {out3}"
    );
    let head3 = run_ok(&ws, &["head", "--dataset-id", "ds_gbk000000000003", "--n", "5"]);
    assert!(head3.contains("东省") && head3.contains("地区"), "{head3}");

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn ingest_utf8_with_and_without_bom_default_and_auto_no_warning() {
    let ws = temp_workspace();
    write_bytes(&ws, "u8.csv", GBK_FIXTURE.as_bytes());
    let mut bom = vec![0xEF, 0xBB, 0xBF];
    bom.extend_from_slice(GBK_FIXTURE.as_bytes());
    write_bytes(&ws, "bom.csv", &bom);

    for (name, id) in [
        ("u8.csv", "ds_utf8000000000001"),
        ("bom.csv", "ds_utf8000000000002"),
    ] {
        // Default utf-8
        let out = run_ok(
            &ws,
            &["ingest", "--input", &format!("data/{name}"), "--dataset-id", id],
        );
        assert_eq!(json_usize_field(&out, "rowCount"), Some(2), "{name}: {out}");
        assert!(!out.contains("warnings"), "utf-8 must not warn: {out}");
        let head = run_ok(&ws, &["head", "--dataset-id", id, "--n", "5"]);
        assert!(
            head.contains("地区") && head.contains("东省"),
            "{name} header must be BOM-clean: {head}"
        );

        // auto: valid UTF-8 (BOM or not) → same rows, no fallback warning
        let id_auto = format!("{id}a");
        let out2 = run_ok(
            &ws,
            &[
                "ingest",
                "--input",
                &format!("data/{name}"),
                "--encoding",
                "auto",
                "--dataset-id",
                &id_auto,
            ],
        );
        assert_eq!(json_usize_field(&out2, "rowCount"), Some(2), "{name}: {out2}");
        assert!(!out2.contains("warnings"), "auto on utf-8 must not warn: {out2}");
    }

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn ingest_bom_is_never_data_in_any_mode() {
    let ws = temp_workspace();
    let gbk_body = gbk_fixture(GBK_FIXTURE);
    let mut bom_gbk = vec![0xEF, 0xBB, 0xBF];
    bom_gbk.extend_from_slice(&gbk_body);
    write_bytes(&ws, "bom-gbk.csv", &bom_gbk);

    // (a) explicit gbk: the BOM is stripped, never decoded into garbage chars
    let out = run_ok(
        &ws,
        &[
            "ingest",
            "--input",
            "data/bom-gbk.csv",
            "--encoding",
            "gbk",
            "--dataset-id",
            "ds_bomgbk0000000001",
        ],
    );
    assert_eq!(json_usize_field(&out, "rowCount"), Some(2), "{out}");
    let head = run_ok(&ws, &["head", "--dataset-id", "ds_bomgbk0000000001", "--n", "5"]);
    assert!(head.contains("地区") && head.contains("东省"), "header: {head}");

    // (b) auto: BOM no longer suppresses the GB18030 fallback
    let out2 = run_ok(
        &ws,
        &[
            "ingest",
            "--input",
            "data/bom-gbk.csv",
            "--encoding",
            "auto",
            "--dataset-id",
            "ds_bomgbk0000000002",
        ],
    );
    assert_eq!(json_usize_field(&out2, "rowCount"), Some(2), "{out2}");
    assert!(
        out2.contains(
            "\"warnings\":[{\"code\":\"encoding-fallback\",\"encoding\":\"gb18030\"}]"
        ),
        "auto must fall back past a BOM: {out2}"
    );
    let head2 = run_ok(&ws, &["head", "--dataset-id", "ds_bomgbk0000000002", "--n", "5"]);
    assert!(head2.contains("地区") && head2.contains("东省"), "{head2}");

    // (c) auto + BOM + corrupt body: still Plan B, no dataset
    let bom_bad = vec![0xEF, 0xBB, 0xBF, b'i', b'd', b',', b'x', b'\n', 0xFF, b'\n'];
    write_bytes(&ws, "bom-bad.csv", &bom_bad);
    let err = run_fail(
        &ws,
        &[
            "ingest",
            "--input",
            "data/bom-bad.csv",
            "--encoding",
            "auto",
            "--dataset-id",
            "ds_bomgbk0000000003",
        ],
        2,
    );
    assert!(err.contains(PLAN_B_UTF8), "stderr={err}");
    assert!(
        !ws.join(".dsh-analyze/datasets/ds_bomgbk0000000003.meta.json").exists(),
        "meta must not exist after decode failure"
    );

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn ingest_corrupt_bytes_rejected_in_every_mode_no_dataset() {
    let ws = temp_workspace();
    // Lone 0xFF: invalid UTF-8 and malformed for the GB18030 decoder too
    // (0xFF is neither ASCII, 0x80, nor a 0x81..=0xFE lead byte).
    // Built as a Vec so the check below is a real runtime assertion.
    let corrupt: Vec<u8> = vec![b'i', b'd', b',', b'x', b'\n', 0xFF, b'\n'];
    assert!(std::str::from_utf8(&corrupt).is_err(), "corrupt must fail UTF-8");
    assert!(
        encoding_rs::GBK
            .decode_without_bom_handling_and_without_replacement(&corrupt)
            .is_none(),
        "corrupt must fail GB18030 fatal decode"
    );
    write_bytes(&ws, "corrupt.csv", &corrupt);

    for enc in [None, Some("utf-8"), Some("gbk"), Some("gb18030"), Some("auto")] {
        let mut args = vec![
            "ingest",
            "--input",
            "data/corrupt.csv",
            "--dataset-id",
            "ds_corrupt00000001",
        ];
        if let Some(e) = enc {
            args.extend(["--encoding", e]);
        }
        let err = run_fail(&ws, &args, 2);
        assert!(
            err.starts_with("analysis/invalid-args:"),
            "enc={enc:?} stderr={err}"
        );
    }

    // Never a garbage dataset: nothing listed, no meta written.
    let list = run_ok(&ws, &["list"]);
    assert_eq!(list.matches("\"datasetId\":").count(), 0, "{list}");
    assert!(
        !ws.join(".dsh-analyze/datasets/ds_corrupt00000001.meta.json").exists(),
        "meta must not exist after decode failure"
    );

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn ingest_encoding_flag_rejected_for_non_csv_formats() {
    let ws = temp_workspace();
    write_file(&ws, "d.json", "[{\"a\":1}]");

    let err = run_fail(
        &ws,
        &["ingest", "--input", "data/d.json", "--encoding", "gbk"],
        2,
    );
    assert!(
        err.contains("analysis/invalid-args: --encoding only applies to csv"),
        "stderr={err}"
    );

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn ingest_bad_encoding_value_is_invalid_args() {
    let ws = temp_workspace();
    write_file(&ws, "d.csv", "a\n1\n");

    let err = run_fail(
        &ws,
        &["ingest", "--input", "data/d.csv", "--encoding", "latin1"],
        2,
    );
    assert!(
        err.contains("analysis/invalid-args: bad encoding latin1"),
        "stderr={err}"
    );

    let _ = fs::remove_dir_all(&ws);
}

/// R-31: export xlsx → ingest xlsx round-trip preserves rows/columns.
#[test]
fn ingest_xlsx_roundtrip_from_export() {
    let ws = temp_workspace();
    write_file(
        &ws,
        "src.csv",
        "name,amount,flag\n\
         alpha,10,true\n\
         beta,20.5,false\n\
         gamma,30,true\n",
    );
    let ing = run_ok(
        &ws,
        &[
            "ingest",
            "--input",
            "data/src.csv",
            "--format",
            "csv",
            "--dataset-id",
            "ds_aabbccdd11223344",
        ],
    );
    assert!(ing.contains("\"ok\":true"), "{ing}");

    let exp = run_ok(
        &ws,
        &[
            "export",
            "--dataset-id",
            "ds_aabbccdd11223344",
            "--output",
            "outputs/rt.xlsx",
            "--format",
            "xlsx",
        ],
    );
    assert!(exp.contains("\"ok\":true"), "{exp}");

    let ing2 = run_ok(
        &ws,
        &[
            "ingest",
            "--input",
            ".dsh-analyze/outputs/rt.xlsx",
            "--format",
            "xlsx",
            "--dataset-id",
            "ds_bbccddee11223344",
        ],
    );
    assert!(ing2.contains("\"ok\":true"), "{ing2}");
    assert!(ing2.contains("\"rowCount\":3"), "{ing2}");
    assert!(ing2.contains("\"columnCount\":3"), "{ing2}");
    assert!(ing2.contains("name"), "{ing2}");
    assert!(ing2.contains("amount"), "{ing2}");

    let head = run_ok(
        &ws,
        &["head", "--dataset-id", "ds_bbccddee11223344", "--n", "5"],
    );
    assert!(head.contains("alpha"), "{head}");
    assert!(head.contains("beta"), "{head}");

    let _ = fs::remove_dir_all(&ws);
}

/// R-31: multi-sheet workbook defaults to first sheet + sheet-skipped warning;
/// --sheet selects by name.
#[test]
fn ingest_xlsx_multisheet_and_sheet_flag() {
    use rust_xlsxwriter::{Workbook, Worksheet};
    let ws = temp_workspace();
    // Build a 2-sheet workbook on disk under data/
    let path = ws.join("data").join("multi.xlsx");
    let mut wb = Workbook::new();
    let mut s1 = Worksheet::new();
    s1.write_string(0, 0, "a").unwrap();
    s1.write_number(1, 0, 1.0).unwrap();
    s1.set_name("第一张").unwrap();
    wb.worksheets_mut().push(s1);
    let mut s2 = Worksheet::new();
    s2.write_string(0, 0, "b").unwrap();
    s2.write_number(1, 0, 2.0).unwrap();
    s2.set_name("second").unwrap();
    wb.worksheets_mut().push(s2);
    wb.save(&path).unwrap();

    let out = run_ok(
        &ws,
        &[
            "ingest",
            "--input",
            "data/multi.xlsx",
            "--format",
            "xlsx",
            "--dataset-id",
            "ds_aabbccdd11223344",
        ],
    );
    assert!(out.contains("\"ok\":true"), "{out}");
    assert!(out.contains("sheet-skipped"), "warning: {out}");
    assert!(out.contains("\"a\""), "first sheet headers: {out}");

    let out2 = run_ok(
        &ws,
        &[
            "ingest",
            "--input",
            "data/multi.xlsx",
            "--format",
            "xlsx",
            "--sheet",
            "second",
            "--dataset-id",
            "ds_bbccddee11223344",
        ],
    );
    assert!(out2.contains("\"ok\":true"), "{out2}");
    assert!(out2.contains("\"b\""), "second sheet: {out2}");
    // still sheet-skipped for the other sheet
    assert!(out2.contains("sheet-skipped"), "{out2}");

    let _ = fs::remove_dir_all(&ws);
}


// ---- v0.1.12 R-32 / R-33 ---------------------------------------------------

/// R-32: new writes land as .parquet; load still works; legacy csv readable.
#[test]
fn r32_parquet_storage_and_legacy_csv_fallback() {
    let ws = temp_workspace();
    write_file(&ws, "a.csv", "x,y\n1,2\n3,4\n");
    let ing = run_ok(
        &ws,
        &[
            "ingest",
            "--input",
            "data/a.csv",
            "--format",
            "csv",
            "--dataset-id",
            "ds_aabbccdd11223344",
        ],
    );
    assert!(ing.contains("\"ok\":true"), "{ing}");
    assert!(
        ws.join(".dsh-analyze/datasets/ds_aabbccdd11223344.parquet")
            .exists(),
        "expected parquet data file"
    );

    let out = run_ok(
        &ws,
        &[
            "agg",
            "--dataset-id",
            "ds_aabbccdd11223344",
            "--spec",
            r#"{"aggs":[{"fn":"count","as":"n"}]}"#,
            "--out",
            "cnt",
        ],
    );
    assert!(out.contains("\"ok\":true"), "{out}");
    let list = run_ok(&ws, &["list"]);
    assert!(list.contains("ds_"), "{list}");

    // Legacy CSV-only dataset still loads (hand-written meta + csv, no parquet).
    let legacy_id = "ds_bbccddee11223344";
    let ds_dir = ws.join(".dsh-analyze/datasets");
    fs::create_dir_all(&ds_dir).unwrap();
    fs::write(ds_dir.join(format!("{legacy_id}.csv")), "x\n9\n").unwrap();
    fs::write(
        ds_dir.join(format!("{legacy_id}.meta.json")),
        format!(
            "{{\"version\":1,\"datasetId\":\"{legacy_id}\",\"rowCount\":1,\"columnCount\":1,\"columns\":[{{\"name\":\"x\",\"dtype\":\"i64\"}}],\"sourceKind\":\"csv\",\"sourcePath\":\"data/legacy.csv\",\"label\":null,\"producedBy\":\"ingest\",\"parentDatasetId\":null,\"createdAt\":\"2026-01-01T00:00:00Z\"}}\n"
        ),
    )
    .unwrap();
    let head = run_ok(&ws, &["head", "--dataset-id", legacy_id, "--n", "5"]);
    assert!(head.contains("9"), "legacy csv head: {head}");

    let _ = fs::remove_dir_all(&ws);
}

/// R-33: ingest sourceChecksum + parentDatasetIds chain + verify.
#[test]
fn r33_checksum_parents_and_verify() {
    let ws = temp_workspace();
    write_file(&ws, "src.csv", "a,b\n1,2\n3,4\n");
    let ing = run_ok(
        &ws,
        &[
            "ingest",
            "--input",
            "data/src.csv",
            "--format",
            "csv",
            "--dataset-id",
            "ds_aabbccdd11223344",
        ],
    );
    assert!(ing.contains("\"ok\":true"), "{ing}");

    let list = run_ok(&ws, &["list"]);
    assert!(list.contains("sourceChecksum"), "{list}");
    assert!(list.contains("sha256:"), "{list}");
    assert!(list.contains("parentDatasetIds"), "{list}");

    let agg = run_ok(
        &ws,
        &[
            "agg",
            "--dataset-id",
            "ds_aabbccdd11223344",
            "--spec",
            r#"{"aggs":[{"fn":"count","as":"n"}]}"#,
            "--out",
            "cnt",
        ],
    );
    assert!(agg.contains("\"ok\":true"), "{agg}");
    let list2 = run_ok(&ws, &["list", "--produced-by", "agg"]);
    assert!(list2.contains("parentDatasetIds"), "{list2}");
    assert!(list2.contains("ds_aabbccdd11223344"), "parent: {list2}");

    let v = run_ok(
        &ws,
        &[
            "verify",
            "--dataset-id",
            "ds_aabbccdd11223344",
            "--check-source",
        ],
    );
    assert!(v.contains("\"ok\":true"), "{v}");
    assert!(v.contains("data-file"), "{v}");

    // tamper source → verify fails exit 1
    write_file(&ws, "src.csv", "a,b\n1,2\n3,4\n5,6\n");
    let out = bin()
        .arg("--workspace")
        .arg(&ws)
        .args([
            "verify",
            "--dataset-id",
            "ds_aabbccdd11223344",
            "--check-source",
        ])
        .output()
        .expect("spawn");
    assert_eq!(out.status.code(), Some(1), "modified source must exit 1");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("verify failed") || err.contains("mismatch"),
        "{err}"
    );

    let _ = fs::remove_dir_all(&ws);
}

/// R-33: join records both parents in parentDatasetIds.
#[test]
fn r33_join_two_parents() {
    let ws = temp_workspace();
    write_file(&ws, "l.csv", "k,v\n1,10\n");
    write_file(&ws, "r.csv", "k,w\n1,20\n");
    run_ok(
        &ws,
        &[
            "ingest",
            "--input",
            "data/l.csv",
            "--format",
            "csv",
            "--dataset-id",
            "ds_aabbccdd11223344",
        ],
    );
    run_ok(
        &ws,
        &[
            "ingest",
            "--input",
            "data/r.csv",
            "--format",
            "csv",
            "--dataset-id",
            "ds_bbccddee11223344",
        ],
    );
    let j = run_ok(
        &ws,
        &[
            "join",
            "--left",
            "ds_aabbccdd11223344",
            "--right",
            "ds_bbccddee11223344",
            "--on",
            "k",
        ],
    );
    assert!(j.contains("\"ok\":true"), "{j}");
    let list = run_ok(&ws, &["list", "--produced-by", "join"]);
    assert!(list.contains("parentDatasetIds"), "{list}");
    assert!(list.contains("ds_aabbccdd11223344"), "{list}");
    assert!(list.contains("ds_bbccddee11223344"), "right parent: {list}");

    let _ = fs::remove_dir_all(&ws);
}

/// R-33: verify mismatch exits 1 but still prints the result JSON on stdout
/// (the tool layer relies on this), with a cli-failed stderr prefix.
#[test]
fn verify_rowcount_mismatch_prints_json_and_exits_1() {
    let ws = temp_workspace();
    write_file(&ws, "v.csv", "a,b\n1,2\n3,4\n");
    run_ok(
        &ws,
        &[
            "ingest",
            "--input",
            "data/v.csv",
            "--format",
            "csv",
            "--dataset-id",
            "ds_aabbccdd11223344",
        ],
    );
    let meta_path = ws
        .join(".dsh-analyze/datasets")
        .join("ds_aabbccdd11223344.meta.json");
    let meta = fs::read_to_string(&meta_path).unwrap();
    assert!(meta.contains("\"rowCount\":2"), "{meta}");
    fs::write(
        &meta_path,
        meta.replace("\"rowCount\":2", "\"rowCount\":99"),
    )
    .unwrap();

    let out = bin()
        .arg("--workspace")
        .arg(&ws)
        .args([
            "verify",
            "--dataset-id",
            "ds_aabbccdd11223344",
        ])
        .output()
        .expect("spawn dsh-analyze");
    assert_eq!(out.status.code(), Some(1), "mismatch must exit 1");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\"ok\":false"), "stdout: {stdout}");
    assert!(stdout.contains("\"row-count\""), "stdout: {stdout}");
    assert!(stdout.contains("\"declared\":99"), "stdout: {stdout}");
    assert!(stdout.contains("\"actual\":2"), "stdout: {stdout}");
    assert!(stdout.contains("\"data-file\""), "stdout: {stdout}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("analysis/cli-failed: dataset verify failed"),
        "stderr prefix must match exit 1: {stderr}"
    );

    let _ = fs::remove_dir_all(&ws);
}

/// R-31/R-32: xlsx text cells (numeric-looking / NA / true) keep their string
/// identity across parquet storage — never re-coerced on load.
#[test]
fn xlsx_text_cells_survive_storage() {
    use rust_xlsxwriter::{Workbook, Worksheet};
    let ws = temp_workspace();
    let path = ws.join("data").join("text.xlsx");
    let mut wb = Workbook::new();
    let mut s = Worksheet::new();
    s.write_string(0, 0, "code").unwrap();
    s.write_string(0, 1, "note").unwrap();
    s.write_string(1, 0, "007").unwrap();
    s.write_string(1, 1, "NA").unwrap();
    s.write_string(2, 0, "42x").unwrap();
    s.write_string(2, 1, "true").unwrap();
    wb.worksheets_mut().push(s);
    wb.save(&path).unwrap();

    run_ok(
        &ws,
        &[
            "ingest",
            "--input",
            "data/text.xlsx",
            "--format",
            "xlsx",
            "--dataset-id",
            "ds_aabbccdd11223344",
        ],
    );
    let head = run_ok(
        &ws,
        &["head", "--dataset-id", "ds_aabbccdd11223344", "--n", "5"],
    );
    assert!(head.contains("\"007\""), "string 007 must survive: {head}");
    assert!(head.contains("\"NA\""), "string NA must survive: {head}");
    assert!(head.contains("\"42x\""), "{head}");
    assert!(head.contains("\"true\""), "string true must stay a string: {head}");
    assert!(!head.contains(",7,"), "007 must not coerce to 7: {head}");

    // meta dtype (schema) stays utf8 and matches the loaded values.
    let schema = run_ok(&ws, &["schema", "--dataset-id", "ds_aabbccdd11223344"]);
    assert!(schema.contains("\"name\":\"code\",\"dtype\":\"utf8\""), "{schema}");

    let _ = fs::remove_dir_all(&ws);
}

/// R-32: json string values that look like scalars keep string identity
/// through parquet storage (legacy CSV reload used to coerce them).
#[test]
fn json_stringy_values_survive_storage() {
    let ws = temp_workspace();
    write_file(
        &ws,
        "s.json",
        r#"[{"code":"007","flag":"true","note":"NA"}]"#,
    );
    run_ok(
        &ws,
        &[
            "ingest",
            "--input",
            "data/s.json",
            "--format",
            "json",
            "--dataset-id",
            "ds_aabbccdd11223344",
        ],
    );
    let head = run_ok(
        &ws,
        &["head", "--dataset-id", "ds_aabbccdd11223344", "--n", "5"],
    );
    assert!(head.contains("\"007\""), "{head}");
    assert!(head.contains("\"true\""), "{head}");
    assert!(head.contains("\"NA\""), "{head}");

    let _ = fs::remove_dir_all(&ws);
}

/// R-31: all-digit sheet NAMES win over index interpretation; digits fall
/// back to a 0-based index only when no sheet matches by name.
#[test]
fn ingest_xlsx_all_digit_sheet_name() {
    use rust_xlsxwriter::{Workbook, Worksheet};
    let ws = temp_workspace();
    let path = ws.join("data").join("digits.xlsx");
    let mut wb = Workbook::new();
    let mut s1 = Worksheet::new();
    s1.write_string(0, 0, "f").unwrap();
    s1.write_string(1, 0, "first-row").unwrap();
    s1.set_name("first").unwrap();
    wb.worksheets_mut().push(s1);
    let mut s2 = Worksheet::new();
    s2.write_string(0, 0, "y").unwrap();
    s2.write_string(1, 0, "year-row").unwrap();
    s2.set_name("2024").unwrap();
    wb.worksheets_mut().push(s2);
    wb.save(&path).unwrap();

    // Exact name match: "2024" selects the sheet named 2024 (not index 2024).
    let by_name = run_ok(
        &ws,
        &[
            "ingest",
            "--input",
            "data/digits.xlsx",
            "--format",
            "xlsx",
            "--sheet",
            "2024",
            "--dataset-id",
            "ds_aabbccdd11223344",
        ],
    );
    assert!(by_name.contains("\"ok\":true"), "{by_name}");
    assert!(by_name.contains("\"y\""), "named sheet header: {by_name}");
    let head1 = run_ok(
        &ws,
        &["head", "--dataset-id", "ds_aabbccdd11223344", "--n", "5"],
    );
    assert!(head1.contains("year-row"), "{head1}");

    // Digits without a matching name fall back to the 0-based index.
    let by_index = run_ok(
        &ws,
        &[
            "ingest",
            "--input",
            "data/digits.xlsx",
            "--format",
            "xlsx",
            "--sheet",
            "0",
            "--dataset-id",
            "ds_bbccddee11223344",
        ],
    );
    assert!(by_index.contains("\"ok\":true"), "{by_index}");
    let head2 = run_ok(
        &ws,
        &["head", "--dataset-id", "ds_bbccddee11223344", "--n", "5"],
    );
    assert!(head2.contains("first-row"), "{head2}");

    let _ = fs::remove_dir_all(&ws);
}
