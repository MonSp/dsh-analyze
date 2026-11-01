//! Integration tests for `agg` spec validation against the real binary.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_dsh-analyze"))
}

/// Unique per call within this process. pid+nanos alone can collide when
/// parallel tests start in the same tick (seen on macOS CI runners).
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_workspace() -> PathBuf {
    let n = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "dsh-analyze-agg-{}-{n}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(dir.join("data")).unwrap();
    dir
}

const ID: &str = "ds_aabbccdd11223344";

fn ingest(ws: &Path) {
    fs::write(ws.join("data").join("a.csv"), "amount\n100\n3000\n5000\n").unwrap();
    let out = bin()
        .arg("--workspace")
        .arg(ws)
        .args(["ingest", "--input", "data/a.csv", "--dataset-id", ID])
        .output()
        .expect("spawn dsh-analyze");
    assert!(
        out.status.success(),
        "ingest failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn run_agg(ws: &Path, spec: &str) -> Output {
    bin()
        .arg("--workspace")
        .arg(ws)
        .args(["agg", "--dataset-id", ID, "--spec", spec])
        .output()
        .expect("spawn dsh-analyze")
}

/// A filter that is not an array must exit 2 with `filter must be an array`,
/// never return the full table with ok:true (v0.1.5 defects BUG-3).
#[test]
fn agg_filter_non_array_rejected_exit_2() {
    let ws = temp_workspace();
    ingest(&ws);
    for spec in [
        r#"{"filter":{"column":"amount","op":"gt","value":3000}}"#,
        r#"{"filter":"amount > 3000"}"#,
        r#"{"filter":{}}"#,
    ] {
        let out = run_agg(&ws, spec);
        assert_eq!(
            out.status.code(),
            Some(2),
            "spec {spec} must exit 2, stdout={}",
            String::from_utf8_lossy(&out.stdout)
        );
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("filter must be an array"), "stderr={err}");
    }
}

/// Unknown top-level spec keys must be rejected, not silently dropped.
#[test]
fn agg_unknown_spec_key_rejected_exit_2() {
    let ws = temp_workspace();
    ingest(&ws);
    let out = run_agg(&ws, r#"{"select":["amount"],"filters":[]}"#);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("unknown spec key"), "stderr={err}");
}

/// Sanity: an array filter still filters (only rows above 3000 survive).
#[test]
fn agg_filter_array_applies() {
    let ws = temp_workspace();
    ingest(&ws);
    let out = run_agg(
        &ws,
        r#"{"filter":[{"column":"amount","op":"gt","value":3000}],"select":["amount"]}"#,
    );
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let line = String::from_utf8_lossy(&out.stdout);
    assert!(line.contains("\"rowCount\":1"), "stdout={line}");
}

/// Nested same-name keys (e.g. `limit` inside a filter item) must never shadow
/// top-level fields: key order must not change the result.
#[test]
fn agg_nested_keys_never_shadow_top_level() {
    let ws = temp_workspace();
    ingest(&ws);
    for spec in [
        r#"{"filter":[{"op":"gt","column":"amount","value":0,"limit":2}],"limit":1}"#,
        r#"{"limit":1,"filter":[{"op":"gt","column":"amount","value":0,"limit":2}]}"#,
    ] {
        let out = run_agg(&ws, spec);
        assert!(
            out.status.success(),
            "spec {spec} must succeed, stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        let line = String::from_utf8_lossy(&out.stdout);
        assert!(line.contains("\"rowCount\":1"), "spec {spec} stdout={line}");
    }
    // a nested `filter` array must not replace the real top-level filter
    let out = run_agg(
        &ws,
        r#"{"mutate":[{"as":"dbl","expr":"amount * 2","filter":[]}],"filter":[{"op":"eq","column":"amount","value":100}]}"#,
    );
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let line = String::from_utf8_lossy(&out.stdout);
    assert!(line.contains("\"rowCount\":1"), "stdout={line}");
}
