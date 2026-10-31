//! Integration tests against the real `dsh-analyze` binary (quality commands).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_dsh-analyze"))
}

fn temp_workspace() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "dsh-analyze-quality-{}-{nanos}",
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
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Extract a JSON string field value: `"name":"value"`.
fn json_str_field(line: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\":\"");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// `outliers --out` must emit valid JSON with a quoted string `outDatasetId`.
/// Regression: a bare `ds_…` token made JSON.parse fail after the dataset was written.
#[test]
fn outliers_out_emits_quoted_string_id() {
    let ws = temp_workspace();
    write_csv(&ws, "out.csv", "amount\n1\n2\n3\n4\n100\n");

    run_ok(
        &ws,
        &[
            "ingest",
            "--input",
            "data/out.csv",
            "--dataset-id",
            "ds_aabbccdd11223344",
        ],
    );

    let out = run_ok(
        &ws,
        &[
            "outliers",
            "--dataset-id",
            "ds_aabbccdd11223344",
            "--columns",
            "amount",
            "--method",
            "iqr",
            "--out",
            "extreme",
        ],
    );

    // Valid-JSON smoke: no bare ds_ token after the field name.
    assert!(
        !out.contains("\"outDatasetId\":ds_"),
        "outDatasetId must be a quoted JSON string, got: {out}"
    );
    let out_id = json_str_field(&out, "outDatasetId").expect("quoted outDatasetId");
    assert!(out_id.starts_with("ds_"), "got {out_id}");
    assert!(out.contains("\"outRowCount\":"), "outRowCount missing: {out}");

    // Written dataset is readable via head (rowCount=1, single value 100).
    let head = run_ok(&ws, &["head", "--dataset-id", &out_id, "--n", "5"]);
    assert!(
        head.contains("\"rowCount\":1"),
        "expected 1 outlier row, got: {head}"
    );
    assert!(head.contains("100"), "expected value 100, got: {head}");

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn outliers_without_out_has_json_null_id() {
    let ws = temp_workspace();
    write_csv(&ws, "out.csv", "amount\n1\n2\n3\n100\n");
    run_ok(
        &ws,
        &[
            "ingest",
            "--input",
            "data/out.csv",
            "--dataset-id",
            "ds_aabbccdd11223344",
        ],
    );

    let out = run_ok(
        &ws,
        &[
            "outliers",
            "--dataset-id",
            "ds_aabbccdd11223344",
            "--columns",
            "amount",
        ],
    );
    assert!(
        out.contains("\"outDatasetId\":null"),
        "expected null outDatasetId, got: {out}"
    );
    assert!(json_str_field(&out, "outDatasetId").is_none());

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn outliers_rejects_invalid_sample_limit() {
    let ws = temp_workspace();
    write_csv(&ws, "out.csv", "amount\n1\n2\n100\n");
    run_ok(
        &ws,
        &[
            "ingest",
            "--input",
            "data/out.csv",
            "--dataset-id",
            "ds_aabbccdd11223344",
        ],
    );

    let stderr = run_fail(
        &ws,
        &[
            "outliers",
            "--dataset-id",
            "ds_aabbccdd11223344",
            "--columns",
            "amount",
            "--sample-limit",
            "abc",
        ],
        2,
    );
    assert!(stderr.contains("sample-limit"), "stderr={stderr}");

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn dedup_rejects_invalid_sample_limit() {
    let ws = temp_workspace();
    write_csv(&ws, "d.csv", "a,b\n1,2\n1,2\n");
    run_ok(
        &ws,
        &[
            "ingest",
            "--input",
            "data/d.csv",
            "--dataset-id",
            "ds_aabbccdd11223344",
        ],
    );

    let stderr = run_fail(
        &ws,
        &[
            "dedup",
            "--dataset-id",
            "ds_aabbccdd11223344",
            "--sample-limit",
            "0",
        ],
        2,
    );
    assert!(stderr.contains("sample-limit"), "stderr={stderr}");

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn profile_and_describe_are_valid_json_shapes() {
    let ws = temp_workspace();
    write_csv(&ws, "p.csv", "phone,amount\n1,10.5\n1,20\n,30\n");
    run_ok(
        &ws,
        &[
            "ingest",
            "--input",
            "data/p.csv",
            "--dataset-id",
            "ds_aabbccdd11223344",
        ],
    );

    let profile = run_ok(&ws, &["profile", "--dataset-id", "ds_aabbccdd11223344"]);
    assert!(profile.contains("\"ok\":true"), "{profile}");
    assert!(profile.contains("\"typeStability\""), "{profile}");
    assert!(!profile.contains("\"typeStability\":,"), "missing typeStability: {profile}");

    let describe = run_ok(&ws, &["describe", "--dataset-id", "ds_aabbccdd11223344"]);
    for field in ["q01", "q10", "q90", "q99"] {
        assert!(
            describe.contains(&format!("\"{field}\":")),
            "describe missing {field}: {describe}"
        );
    }

    let _ = fs::remove_dir_all(&ws);
}
