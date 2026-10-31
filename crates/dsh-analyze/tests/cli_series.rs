//! Integration tests against the real `dsh-analyze` binary (series command).

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
        "dsh-analyze-series-{}-{n}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(dir.join("data")).unwrap();
    dir
}

fn write_csv(ws: &Path, name: &str, body: &str) {
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
    String::from_utf8(out.stderr).unwrap_or_default()
}

fn ingest(ws: &Path, name: &str) -> String {
    let stdout = run_ok(ws, &["ingest", "--input", name]);
    extract_ds_id(&stdout).expect("datasetId")
}

fn extract_ds_id(line: &str) -> Option<String> {
    let needle = "\"datasetId\":\"";
    let start = line.find(needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn extract_i64(line: &str, field: &str) -> Option<i64> {
    let needle = format!("\"{field}\":");
    let start = line.find(&needle)? + needle.len();
    let rest = line[start..].trim_start();
    let end = rest.find([',', '}', ']']).unwrap_or(rest.len());
    rest[..end].trim().parse::<i64>().ok()
}

fn extract_f64(line: &str, field: &str) -> Option<f64> {
    let needle = format!("\"{field}\":");
    let start = line.find(&needle)? + needle.len();
    let rest = line[start..].trim_start();
    let end = rest.find([',', '}', ']']).unwrap_or(rest.len());
    rest[..end].trim().parse::<f64>().ok()
}

fn extract_str(line: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\":\"");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Return the full head JSON for a dataset (rows ≤ 20).
fn head_json(ws: &Path, ds: &str) -> String {
    run_ok(ws, &["head", "--dataset-id", ds, "--n", "20"])
}

/// Assert head JSON contains the exact `"rows":[...]` payload.
fn assert_rows_json(head: &str, expected_rows: &str) {
    let needle = format!("\"rows\":{expected_rows}");
    assert!(
        head.contains(&needle),
        "expected rows {expected_rows} in {head}"
    );
}

/// Hand calc: ordered by t.
/// t: 1,2,3,4  v: 10,20,30,50
/// lag1: null,10,20,30
/// diff1: null,10,10,20
/// rolling2 mean: null,15,25,40
#[test]
fn series_lag_diff_rolling_hand_calc() {
    let ws = temp_workspace();
    write_csv(&ws, "s.csv", "t,v\n1,10\n2,20\n3,30\n4,50\n");
    let id = ingest(&ws, "data/s.csv");
    let ops = r#"[
      {"fn":"lag","column":"v","periods":1,"as":"v_lag"},
      {"fn":"diff","column":"v","periods":1,"as":"v_diff"},
      {"fn":"rolling","column":"v","window":2,"agg":"mean","as":"v_ma2"}
    ]"#;
    let out = run_ok(
        &ws,
        &[
            "series",
            "--dataset-id",
            &id,
            "--order-by",
            "t",
            "--ops",
            ops,
        ],
    );
    assert_eq!(extract_str(&out, "orderBy").as_deref(), Some("t"));
    assert_eq!(extract_i64(&out, "opCount"), Some(3));
    assert_eq!(extract_i64(&out, "rowCount"), Some(4));
    let new_id = extract_ds_id(&out).unwrap();

    let head = head_json(&ws, &new_id);
    // cols: t,v,v_lag,v_diff,v_ma2
    assert_rows_json(
        &head,
        "[[1,10,null,null,null],[2,20,10,10,15],[3,30,20,10,25],[4,50,30,20,40]]",
    );
}

#[test]
fn series_order_by_nulls_last() {
    let ws = temp_workspace();
    write_csv(&ws, "s.csv", "t,v\n2,20\n,99\n1,10\n");
    let id = ingest(&ws, "data/s.csv");
    let ops = r#"[{"fn":"lag","column":"v","periods":1,"as":"v_lag"}]"#;
    let out = run_ok(
        &ws,
        &["series", "--dataset-id", &id, "--order-by", "t", "--ops", ops],
    );
    let new_id = extract_ds_id(&out).unwrap();
    // sorted t: 1,2,null → v: 10,20,99 → lag: null,10,20
    let head = head_json(&ws, &new_id);
    assert_rows_json(&head, "[[1,10,null],[2,20,10],[null,99,20]]");
}

#[test]
fn series_pct_change_zero_prev_and_incomplete_window() {
    let ws = temp_workspace();
    write_csv(&ws, "s.csv", "t,v\n1,10\n2,0\n3,20\n");
    let id = ingest(&ws, "data/s.csv");
    let ops = r#"[
      {"fn":"pct_change","column":"v","periods":1,"as":"pct"},
      {"fn":"rolling","column":"v","window":3,"agg":"sum","as":"roll"}
    ]"#;
    let out = run_ok(
        &ws,
        &["series", "--dataset-id", &id, "--order-by", "t", "--ops", ops],
    );
    let new_id = extract_ds_id(&out).unwrap();
    let head = head_json(&ws, &new_id);
    // pct: null, (0-10)/10=-1, prev=0 → null; roll window=3 → only last row
    assert_rows_json(&head, "[[1,10,null,null],[2,0,-1,null],[3,20,null,30]]");
}

#[test]
fn series_unknown_column_exit_2() {
    let ws = temp_workspace();
    write_csv(&ws, "s.csv", "t,v\n1,10\n");
    let id = ingest(&ws, "data/s.csv");
    let stderr = run_fail(
        &ws,
        &[
            "series",
            "--dataset-id",
            &id,
            "--order-by",
            "missing",
            "--ops",
            r#"[{"fn":"lag","column":"v","as":"x"}]"#,
        ],
        2,
    );
    assert!(stderr.contains("order-by") || stderr.contains("invalid-args"));
}

#[test]
fn series_version_has_capability() {
    let ws = temp_workspace();
    let out = run_ok(&ws, &["version"]);
    assert!(out.contains("\"version\":\"0.1.5\""));
    assert!(out.contains("\"series\""));
}

/// Mixed-type order-by: Bool < number < Str; nulls last.
/// Input order: str-a, 2, true, null, 1, str-b, false
/// Expected sort: false, true, 1, 2, str-a, str-b, null
#[test]
fn series_order_by_mixed_types_bool_num_str() {
    let ws = temp_workspace();
    write_csv(
        &ws,
        "s.csv",
        "k,v\nstr-a,10\n2,20\ntrue,30\n,40\n1,50\nstr-b,60\nfalse,70\n",
    );
    let id = ingest(&ws, "data/s.csv");
    let ops = r#"[{"fn":"lag","column":"v","as":"v_lag"}]"#;
    let out = run_ok(
        &ws,
        &["series", "--dataset-id", &id, "--order-by", "k", "--ops", ops],
    );
    let new_id = extract_ds_id(&out).unwrap();
    let head = head_json(&ws, &new_id);
    // v after sort: 70,30,50,20,10,60,40 → lag: null,70,30,50,20,10,60
    assert_rows_json(
        &head,
        "[[false,70,null],[true,30,70],[1,50,30],[2,20,50],[\"str-a\",10,20],[\"str-b\",60,10],[null,40,60]]",
    );
}

#[test]
fn series_rejects_empty_ops_and_bad_fn() {
    let ws = temp_workspace();
    write_csv(&ws, "s.csv", "t,v\n1,10\n");
    let id = ingest(&ws, "data/s.csv");
    run_fail(
        &ws,
        &["series", "--dataset-id", &id, "--order-by", "t", "--ops", "[]"],
        2,
    );
    run_fail(
        &ws,
        &[
            "series",
            "--dataset-id",
            &id,
            "--order-by",
            "t",
            "--ops",
            r#"[{"fn":"nope","column":"v","as":"x"}]"#,
        ],
        2,
    );
}

#[test]
fn series_lead_and_forward_fill_guard() {
    let ws = temp_workspace();
    write_csv(&ws, "s.csv", "t,v\n1,10\n2,20\n3,30\n");
    let id = ingest(&ws, "data/s.csv");
    let ops = r#"[{"fn":"lead","column":"v","periods":2,"as":"v_lead"}]"#;
    let out = run_ok(
        &ws,
        &["series", "--dataset-id", &id, "--order-by", "t", "--ops", ops],
    );
    let new_id = extract_ds_id(&out).unwrap();
    let head = head_json(&ws, &new_id);
    assert_rows_json(&head, "[[1,10,30],[2,20,null],[3,30,null]]");
}

#[test]
fn series_response_source_fields() {
    let ws = temp_workspace();
    write_csv(&ws, "s.csv", "t,v\n1,10\n2,20\n");
    let id = ingest(&ws, "data/s.csv");
    let out = run_ok(
        &ws,
        &[
            "series",
            "--dataset-id",
            &id,
            "--order-by",
            "t",
            "--ops",
            r#"[{"fn":"diff","column":"v","as":"d"}]"#,
            "--out",
            "series-run",
        ],
    );
    assert_eq!(extract_str(&out, "sourceDatasetId").as_deref(), Some(id.as_str()));
    assert_eq!(extract_f64(&out, "rowCount").map(|r| r as i64), Some(2));
    let new_id = extract_ds_id(&out).unwrap();
    let list = run_ok(&ws, &["list"]);
    assert!(list.contains(&new_id));
    assert!(list.contains("series-run"));
}
