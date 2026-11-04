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

fn json_str_field(line: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\":\"");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
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
fn ingest_xlsx_and_parquet_report_limit() {
    let ws = temp_workspace();
    write_file(&ws, "w.xlsx", "dummy");
    write_file(&ws, "w.parquet", "dummy");

    let err = run_fail(
        &ws,
        &["ingest", "--input", "data/w.xlsx", "--format", "xlsx"],
        4,
    );
    assert!(
        err.contains("analysis/limit: xlsx not supported (convert to csv/jsonl)"),
        "stderr={err}"
    );

    let err2 = run_fail(
        &ws,
        &["ingest", "--input", "data/w.parquet"],
        4,
    );
    assert!(
        err2.contains("analysis/limit: parquet not supported yet (planned, see docs/interface.md)"),
        "stderr={err2}"
    );

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
    assert!(text.contains("--format csv|json|jsonl"), "{text}");
    assert!(text.contains("parquet/xlsx not enabled"), "{text}");
    for cmd in ["ingest", "union", "diff", "pivot", "list", "export"] {
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
    assert!(out.contains("\"version\":\"0.2.0\""), "{out}");
    for tok in ["\"union\"", "\"diff\"", "\"pivot\""] {
        assert!(out.contains(tok), "missing {tok}: {out}");
    }
}
