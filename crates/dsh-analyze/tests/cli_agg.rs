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

fn ingest_csv(ws: &Path, file: &str, body: &str, id: &str) {
    fs::write(ws.join("data").join(file), body).unwrap();
    let out = bin()
        .arg("--workspace")
        .arg(ws)
        .args(["ingest", "--input", &format!("data/{file}"), "--dataset-id", id])
        .output()
        .expect("spawn dsh-analyze");
    assert!(
        out.status.success(),
        "ingest failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn ingest(ws: &Path) {
    ingest_csv(ws, "a.csv", "amount\n100\n3000\n5000\n", ID);
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

/// limit outside 1..5000 must exit 2 instead of silently clamping.
#[test]
fn agg_limit_out_of_range_rejected_exit_2() {
    let ws = temp_workspace();
    ingest(&ws);
    for spec in [r#"{"limit":0}"#, r#"{"limit":-5}"#, r#"{"limit":99999}"#] {
        let out = run_agg(&ws, spec);
        assert_eq!(out.status.code(), Some(2), "spec {spec} must exit 2");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("limit must be an integer 1..5000"), "stderr={err}");
    }
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

/// `in` matches every listed member, not just the first (PR #6 review item).
#[test]
fn agg_filter_in_matches_all_values() {
    let ws = temp_workspace();
    ingest(&ws);
    for spec in [
        r#"{"filter":[{"op":"in","column":"amount","value":[100,3000]}]}"#,
        r#"{"filter":[{"op":"in","column":"amount","value":["100","3000"]}]}"#,
    ] {
        let out = run_agg(&ws, spec);
        assert!(
            out.status.success(),
            "spec {spec} must succeed, stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        let line = String::from_utf8_lossy(&out.stdout);
        assert!(line.contains("\"rowCount\":2"), "spec {spec} stdout={line}");
    }
}

/// Bad filter value shapes exit 2 instead of silently defaulting.
#[test]
fn agg_filter_value_shapes_rejected_exit_2() {
    let ws = temp_workspace();
    ingest(&ws);
    for spec in [
        r#"{"filter":[{"op":"eq","column":"amount","value":[1,2]}]}"#,
        r#"{"filter":[{"op":"eq","column":"amount"}]}"#,
        r#"{"filter":[{"op":"eq","column":"amount","value":{"x":1}}]}"#,
    ] {
        let out = run_agg(&ws, spec);
        assert_eq!(out.status.code(), Some(2), "spec {spec} must exit 2");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("filter value"), "stderr={err}");
    }
}

/// Column names containing `]` survive array extraction (string-aware brackets).
#[test]
fn agg_select_column_with_bracket() {
    let ws = temp_workspace();
    ingest_csv(&ws, "br.csv", "A]B\n1\n2\n3\n", ID);
    let out = run_agg(&ws, r#"{"select":["A]B"]}"#);
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let line = String::from_utf8_lossy(&out.stdout);
    assert!(line.contains("\"rowCount\":3"), "stdout={line}");
    assert!(line.contains("A]B"), "stdout={line}");
}

/// Number literals parse identically on every path; trailing junk errors.
#[test]
fn agg_number_lexing_consistent() {
    let ws = temp_workspace();
    ingest(&ws);
    // 2e1 = 20 (a silent prefix read of 2 would truncate to rowCount 2)
    let out = run_agg(&ws, r#"{"limit":2e1}"#);
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("\"rowCount\":3"),
        "stdout={}",
        String::from_utf8_lossy(&out.stdout)
    );
    // 3e3 = 3000: only 5000 survives (a prefix read of 3 passes all rows)
    let out = run_agg(&ws, r#"{"filter":[{"op":"gt","column":"amount","value":3e3}]}"#);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("\"rowCount\":1"));
    // in scalar 1e2 = 100: one row (a prefix read of 1 matches nothing)
    let out = run_agg(&ws, r#"{"filter":[{"op":"in","column":"amount","value":1e2}]}"#);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("\"rowCount\":1"));
    // trailing junk is rejected instead of reading the prefix
    let out = run_agg(&ws, r#"{"limit":2abc}"#);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("bad number"), "stderr={err}");
}

/// Escaped quotes inside a filter item must not swallow later filters
/// (the whole table used to come back with ok:true).
#[test]
fn agg_escaped_quotes_keep_all_filters() {
    let ws = temp_workspace();
    ingest(&ws);
    let out = run_agg(
        &ws,
        r#"{"filter":[{"op":"ne","column":"amount","value":"x\"y"},{"op":"gt","column":"amount","value":3000}]}"#,
    );
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    // first filter keeps all rows, second keeps only 5000
    // (if it were swallowed the response would return all 3 rows)
    let line = String::from_utf8_lossy(&out.stdout);
    assert!(line.contains("\"rowCount\":1"), "stdout={line}");
}

// ---- v0.1.7 requirement regressions ---------------------------------------

const RICH: &str = "region,amount,day\n\
east,100,2026-10-01\n\
east,3000,2026/10/02\n\
west,5000,20261003\n\
west,7000,not-a-date\n\
east,200,2026-10-01\n";

fn ingest_rich(ws: &Path) {
    ingest_csv(ws, "rich.csv", RICH, ID);
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

// R-1: mutate arithmetic type-checks before eval; bad types exit 2.

#[test]
fn r1_numeric_mutate_still_works() {
    let ws = temp_workspace();
    ingest_rich(&ws);
    let out = run_agg(
        &ws,
        r#"{"mutate":[{"as":"dbl","expr":"amount * 2"}],"select":["dbl"],"orderBy":[{"column":"dbl"}]}"#,
    );
    assert!(out.status.success(), "stderr={}", stderr_of(&out));
    let line = stdout_of(&out);
    assert!(line.contains("\"rowCount\":5"), "stdout={line}");
    assert!(line.contains("200"), "stdout={line}");
}

#[test]
fn r1_mutate_type_error_exits_2() {
    let ws = temp_workspace();
    ingest_rich(&ws);
    let out = run_agg(&ws, r#"{"mutate":[{"as":"x","expr":"region + 1"}]}"#);
    assert_eq!(out.status.code(), Some(2));
    let err = stderr_of(&out);
    assert!(
        err.contains("analysis/invalid-args: mutate x: cannot add number to string"),
        "stderr={err}"
    );
    assert!(!stdout_of(&out).contains("\"ok\":true"));
}

#[test]
fn r1_negate_and_other_verbs() {
    let ws = temp_workspace();
    ingest_rich(&ws);
    let out = run_agg(&ws, r#"{"mutate":[{"as":"x","expr":"-region"}]}"#);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr_of(&out).contains("cannot negate string"));
    let out = run_agg(&ws, r#"{"mutate":[{"as":"x","expr":"amount * region"}]}"#);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr_of(&out).contains("cannot multiply number by string"));
}

// R-3: --spec never leaks OS error text.

#[test]
fn r3_spec_from_workspace_file() {
    let ws = temp_workspace();
    ingest_rich(&ws);
    fs::write(
        ws.join("data").join("spec.json"),
        r#"{"aggs":[{"fn":"count","as":"n"}]}"#,
    )
    .unwrap();
    let out = run_agg(&ws, "data/spec.json");
    assert!(out.status.success(), "stderr={}", stderr_of(&out));
    assert!(stdout_of(&out).contains("\"rowCount\":1"));
}

#[test]
fn r3_spec_bad_input_stable_message() {
    let ws = temp_workspace();
    ingest_rich(&ws);
    for bad in ["a:b:c", "diff:amount:d", "not json"] {
        let out = run_agg(&ws, bad);
        assert_eq!(out.status.code(), Some(2), "input {bad} must exit 2");
        let err = stderr_of(&out);
        assert!(
            err.contains("--spec must be JSON object or a workspace path"),
            "input={bad} stderr={err}"
        );
        assert!(!err.contains("os error"), "input={bad} stderr={err}");
    }
}

#[test]
fn r3_spec_missing_file_stable_reason() {
    let ws = temp_workspace();
    ingest_rich(&ws);
    let out = run_agg(&ws, "data/nope.json");
    assert_eq!(out.status.code(), Some(2));
    let err = stderr_of(&out);
    assert!(
        err.contains("cannot read spec file: file not found"),
        "stderr={err}"
    );
    assert!(!err.contains("os error"), "stderr={err}");
}

// R-4: new date formats parse; parseFailSample is actionable.

#[test]
fn r4_slash_and_compact_dates_bucket() {
    let ws = temp_workspace();
    ingest_rich(&ws);
    let out = run_agg(
        &ws,
        r#"{"groupBy":[{"dateColumn":"day","period":"month"}],"aggs":[{"fn":"count","as":"n"}],"orderBy":[{"column":"day_month"}]}"#,
    );
    assert!(out.status.success(), "stderr={}", stderr_of(&out));
    let line = stdout_of(&out);
    assert!(line.contains("\"2026-10\""), "stdout={line}");
    assert!(line.contains("[\"2026-10\",4]"), "stdout={line}");
}

#[test]
fn r4_parse_fail_sample_visible() {
    let ws = temp_workspace();
    ingest_rich(&ws);
    let out = run_agg(
        &ws,
        r#"{"groupBy":[{"dateColumn":"day","period":"day"}],"aggs":[{"fn":"count","as":"n"}]}"#,
    );
    assert!(out.status.success(), "stderr={}", stderr_of(&out));
    let line = stdout_of(&out);
    assert!(line.contains("\"parseFailCount\":1"), "stdout={line}");
    assert!(
        line.contains("\"parseFailSample\":[\"not-a-date\"]"),
        "stdout={line}"
    );
}

// R-20: agg response warnings[] for runtime Nulls.

#[test]
fn r20_mutate_null_warning() {
    let ws = temp_workspace();
    ingest_rich(&ws);
    let out = run_agg(&ws, r#"{"mutate":[{"as":"q","expr":"amount / 0"}]}"#);
    assert!(out.status.success(), "stderr={}", stderr_of(&out));
    let line = stdout_of(&out);
    assert!(
        line.contains("\"code\":\"mutate-null\"") && line.contains("\"count\":5"),
        "stdout={line}"
    );
}

#[test]
fn r20_cast_null_warning() {
    let ws = temp_workspace();
    ingest_rich(&ws);
    let out = run_agg(&ws, r#"{"mutate":[{"as":"n","expr":"to_int(day)"}]}"#);
    assert!(out.status.success(), "stderr={}", stderr_of(&out));
    let line = stdout_of(&out);
    // day values: only "20261003" casts to int; the other 4 fail
    assert!(
        line.contains("\"code\":\"cast-null\"") && line.contains("\"count\":4"),
        "stdout={line}"
    );
}

#[test]
fn r20_date_parse_fail_warning_and_no_warnings_when_clean() {
    let ws = temp_workspace();
    ingest_rich(&ws);
    let out = run_agg(
        &ws,
        r#"{"groupBy":[{"dateColumn":"day","period":"day"}],"aggs":[{"fn":"count","as":"n"}]}"#,
    );
    assert!(out.status.success(), "stderr={}", stderr_of(&out));
    let line = stdout_of(&out);
    assert!(
        line.contains("\"code\":\"date-parse-fail\"") && line.contains("\"count\":1"),
        "stdout={line}"
    );
    let out = run_agg(&ws, r#"{"aggs":[{"fn":"count","as":"n"}]}"#);
    assert!(out.status.success());
    assert!(!stdout_of(&out).contains("\"warnings\""));
}

// R-5: filter {all|any|not} groups.

#[test]
fn r5_filter_or_and_not() {
    let ws = temp_workspace();
    ingest_rich(&ws);
    let out = run_agg(
        &ws,
        r#"{"filter":{"any":[{"op":"eq","column":"region","value":"west"},{"op":"eq","column":"amount","value":100}]},"aggs":[{"fn":"count","as":"n"}]}"#,
    );
    assert!(out.status.success(), "stderr={}", stderr_of(&out));
    assert!(stdout_of(&out).contains("[[3]]"), "stdout={}", stdout_of(&out));
    let out = run_agg(
        &ws,
        r#"{"filter":{"all":[{"any":[{"op":"eq","column":"region","value":"east"},{"op":"eq","column":"region","value":"west"}]},{"not":{"column":"amount","op":"gt","value":3000}}]},"aggs":[{"fn":"count","as":"n"}]}"#,
    );
    assert!(out.status.success(), "stderr={}", stderr_of(&out));
    assert!(stdout_of(&out).contains("[[3]]"), "stdout={}", stdout_of(&out));
    let out = run_agg(
        &ws,
        r#"{"filter":{"not":{"column":"region","op":"eq","value":"east"}},"aggs":[{"fn":"count","as":"n"}]}"#,
    );
    assert!(out.status.success(), "stderr={}", stderr_of(&out));
    assert!(stdout_of(&out).contains("[[2]]"), "stdout={}", stdout_of(&out));
}

#[test]
fn r5_filter_unknown_shape_and_depth() {
    let ws = temp_workspace();
    ingest_rich(&ws);
    let out = run_agg(&ws, r#"{"filter":{"xor":[]}}"#);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr_of(&out).contains("filter must be an array or {all|any|not} group"));
    let mut core = r#"{"column":"amount","op":"gt","value":0}"#.to_string();
    for _ in 0..9 {
        core = format!(r#"{{"not":{core}}}"#);
    }
    let out = run_agg(&ws, &format!(r#"{{"filter":{core}}}"#));
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr_of(&out).contains("filter nesting too deep"));
}

// R-6: new agg functions.

#[test]
fn r6_new_agg_fns_positive() {
    let ws = temp_workspace();
    ingest_rich(&ws);
    let out = run_agg(
        &ws,
        r#"{"aggs":[{"fn":"count_distinct","column":"region","as":"nr"},{"fn":"percentile","column":"amount","p":0.95,"as":"p95"},{"fn":"first","column":"amount","as":"f"},{"fn":"last","column":"amount","as":"l"},{"fn":"count_if","where":{"column":"amount","op":"gt","value":1000},"as":"big"}]}"#,
    );
    assert!(out.status.success(), "stderr={}", stderr_of(&out));
    let line = stdout_of(&out);
    assert!(line.contains("\"nr\""), "stdout={line}");
    // rows carry values positionally: [nr, p95, f, l, big] = [2, 7000, 100, 200, 3]
    assert!(line.contains("[2,7000,100,200,3]"), "stdout={line}");
}

#[test]
fn r6_new_agg_fns_bad_params() {
    let ws = temp_workspace();
    ingest_rich(&ws);
    for spec in [
        r#"{"aggs":[{"fn":"percentile","column":"amount","as":"p"}]}"#,
        r#"{"aggs":[{"fn":"percentile","column":"amount","p":1.5,"as":"p"}]}"#,
        r#"{"aggs":[{"fn":"count_if","column":"amount","where":{"column":"amount","op":"gt","value":1},"as":"c"}]}"#,
        r#"{"aggs":[{"fn":"count_distinct","as":"nr"}]}"#,
    ] {
        let out = run_agg(&ws, spec);
        assert_eq!(out.status.code(), Some(2), "spec {spec} must exit 2");
        assert!(
            stderr_of(&out).contains("analysis/invalid-args"),
            "spec {spec} stderr={}",
            stderr_of(&out)
        );
    }
}

// R-7: having filters result columns after aggregation.

#[test]
fn r7_having_positive_and_unknown_column() {
    let ws = temp_workspace();
    ingest_rich(&ws);
    let out = run_agg(
        &ws,
        r#"{"groupBy":["region"],"aggs":[{"fn":"count","as":"n"}],"having":[{"column":"n","op":"gt","value":2}],"orderBy":[{"column":"region"}]}"#,
    );
    assert!(out.status.success(), "stderr={}", stderr_of(&out));
    let line = stdout_of(&out);
    assert!(line.contains("\"rowCount\":1"), "stdout={line}");
    assert!(line.contains("\"east\""), "stdout={line}");
    let out = run_agg(
        &ws,
        r#"{"groupBy":["region"],"aggs":[{"fn":"count","as":"n"}],"having":[{"column":"nope","op":"gt","value":1}]}"#,
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr_of(&out).contains("unknown column nope"));
}

// R-9: select rename + whitelist casts.

#[test]
fn r9_select_rename_and_to_date_group() {
    let ws = temp_workspace();
    ingest_rich(&ws);
    let out = run_agg(
        &ws,
        r#"{"select":[{"column":"region","as":"r"},{"column":"amount","as":"a"}],"limit":1}"#,
    );
    assert!(out.status.success(), "stderr={}", stderr_of(&out));
    let line = stdout_of(&out);
    assert!(line.contains("\"columns\":[\"r\",\"a\"]"), "stdout={line}");
    let out = run_agg(
        &ws,
        r#"{"mutate":[{"as":"d","expr":"to_date(\"2026/10/02\", \"%Y/%m/%d\")"}],"groupBy":[{"dateColumn":"d","period":"month"}],"aggs":[{"fn":"count","as":"n"}]}"#,
    );
    assert!(out.status.success(), "stderr={}", stderr_of(&out));
    let line = stdout_of(&out);
    assert!(line.contains("\"2026-10\""), "stdout={line}");
}

#[test]
fn r9_select_duplicate_and_to_date_type_error() {
    let ws = temp_workspace();
    ingest_rich(&ws);
    let out = run_agg(
        &ws,
        r#"{"select":[{"column":"region","as":"x"},{"column":"amount","as":"x"}]}"#,
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr_of(&out).contains("duplicate select name"));
    let out = run_agg(&ws, r#"{"mutate":[{"as":"d","expr":"to_date(amount, \"%Y\")"}]}"#);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr_of(&out).contains("to_date value must be string"));
    let out = run_agg(&ws, r#"{"mutate":[{"as":"d","expr":"to_date(day, \"%F\")"}]}"#);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr_of(&out).contains("bad date format directive"));
}

// R-15: topKPerGroup.

#[test]
fn r15_top_k_per_group_positive() {
    let ws = temp_workspace();
    ingest_rich(&ws);
    let out = run_agg(
        &ws,
        r#"{"topKPerGroup":{"by":["region"],"k":1,"orderBy":{"column":"amount","desc":true}},"select":["region","amount"],"orderBy":[{"column":"region"}]}"#,
    );
    assert!(out.status.success(), "stderr={}", stderr_of(&out));
    let line = stdout_of(&out);
    assert!(line.contains("\"rowCount\":2"), "stdout={line}");
    assert!(line.contains("[\"east\",3000]"), "stdout={line}");
    assert!(line.contains("[\"west\",7000]"), "stdout={line}");
}

#[test]
fn r15_top_k_out_of_range() {
    let ws = temp_workspace();
    ingest_rich(&ws);
    for spec in [
        r#"{"topKPerGroup":{"by":["region"],"k":0}}"#,
        r#"{"topKPerGroup":{"by":["region"],"k":1001}}"#,
        r#"{"topKPerGroup":{"by":[],"k":1}}"#,
    ] {
        let out = run_agg(&ws, spec);
        assert_eq!(out.status.code(), Some(2), "spec {spec} must exit 2");
        assert!(
            stderr_of(&out).contains("analysis/invalid-args"),
            "spec {spec}"
        );
    }
}

// R-29: timezone-aware time bucketing (S2.2 / S2.3).

/// §7.1 three-row +08:00 flow sample.
const FLOW: &str = "ts,amt\n\
2026-10-01T00:30:00+08:00,10\n\
2026-10-01T12:00:00+08:00,20\n\
2026-10-02T01:00:00+08:00,30\n";

fn ingest_flow(ws: &Path) {
    ingest_csv(ws, "flow.csv", FLOW, ID);
}

/// T1: day buckets with `tz:"Asia/Shanghai"` and default-UTC regression,
/// both sides locked against the §7.1 sample.
#[test]
fn r29_tz_day_buckets_both_sides() {
    let ws = temp_workspace();
    ingest_flow(&ws);
    let out = run_agg(
        &ws,
        r#"{"groupBy":[{"dateColumn":"ts","period":"day","tz":"Asia/Shanghai"}],"aggs":[{"fn":"sum","column":"amt","as":"s"},{"fn":"count","as":"n"}],"orderBy":[{"column":"ts_day"}]}"#,
    );
    assert!(out.status.success(), "stderr={}", stderr_of(&out));
    let line = stdout_of(&out);
    assert!(line.contains(r#"["2026-10-01",30,2]"#), "stdout={line}");
    assert!(line.contains(r#"["2026-10-02",30,1]"#), "stdout={line}");
    assert!(!line.contains("2026-09-30"), "stdout={line}");
    // fixed-offset spelling resolves to the same E
    let out = run_agg(
        &ws,
        r#"{"groupBy":[{"dateColumn":"ts","period":"day","tz":"+08:00"}],"aggs":[{"fn":"sum","column":"amt","as":"s"},{"fn":"count","as":"n"}],"orderBy":[{"column":"ts_day"}]}"#,
    );
    assert!(out.status.success(), "stderr={}", stderr_of(&out));
    let line = stdout_of(&out);
    assert!(line.contains(r#"["2026-10-01",30,2]"#), "stdout={line}");
    assert!(line.contains(r#"["2026-10-02",30,1]"#), "stdout={line}");
    // default (no tz): historical UTC buckets unchanged
    let out = run_agg(
        &ws,
        r#"{"groupBy":[{"dateColumn":"ts","period":"day"}],"aggs":[{"fn":"sum","column":"amt","as":"s"},{"fn":"count","as":"n"}],"orderBy":[{"column":"ts_day"}]}"#,
    );
    assert!(out.status.success(), "stderr={}", stderr_of(&out));
    let line = stdout_of(&out);
    assert!(line.contains(r#"["2026-09-30",10,1]"#), "stdout={line}");
    assert!(line.contains(r#"["2026-10-01",50,2]"#), "stdout={line}");
}

/// T2: month boundary — local 10-01 00:30+08:00 is October in Shanghai,
/// September under default UTC.
#[test]
fn r29_tz_month_cross_boundary() {
    let ws = temp_workspace();
    ingest_flow(&ws);
    let out = run_agg(
        &ws,
        r#"{"groupBy":[{"dateColumn":"ts","period":"month","tz":"Asia/Shanghai"}],"aggs":[{"fn":"count","as":"n"}],"orderBy":[{"column":"ts_month"}]}"#,
    );
    assert!(out.status.success(), "stderr={}", stderr_of(&out));
    let line = stdout_of(&out);
    assert!(line.contains(r#"["2026-10",3]"#), "stdout={line}");
    let out = run_agg(
        &ws,
        r#"{"groupBy":[{"dateColumn":"ts","period":"month"}],"aggs":[{"fn":"count","as":"n"}],"orderBy":[{"column":"ts_month"}]}"#,
    );
    assert!(out.status.success(), "stderr={}", stderr_of(&out));
    let line = stdout_of(&out);
    assert!(line.contains(r#"["2026-09",1]"#), "stdout={line}");
    assert!(line.contains(r#"["2026-10",2]"#), "stdout={line}");
}

/// T2: week boundary — Sunday 2026-10-04 23:30Z is Monday 2026-10-05
/// in Shanghai (different ISO week than under default UTC).
#[test]
fn r29_tz_week_cross_boundary() {
    let ws = temp_workspace();
    ingest_csv(&ws, "week.csv", "ts\n2026-10-04T23:30:00Z\n", ID);
    let out = run_agg(
        &ws,
        r#"{"groupBy":[{"dateColumn":"ts","period":"week","tz":"Asia/Shanghai"}],"aggs":[{"fn":"count","as":"n"}]}"#,
    );
    assert!(out.status.success(), "stderr={}", stderr_of(&out));
    let line = stdout_of(&out);
    assert!(line.contains(r#"["2026-10-05",1]"#), "stdout={line}");
    let out = run_agg(
        &ws,
        r#"{"groupBy":[{"dateColumn":"ts","period":"week"}],"aggs":[{"fn":"count","as":"n"}]}"#,
    );
    assert!(out.status.success(), "stderr={}", stderr_of(&out));
    let line = stdout_of(&out);
    assert!(line.contains(r#"["2026-09-28",1]"#), "stdout={line}");
}

/// T2: date-only values never drift under tz; naive values are the local
/// wall-clock of E (no conversion).
#[test]
fn r29_tz_date_only_and_naive_local() {
    let ws = temp_workspace();
    ingest_rich(&ws);
    // date-only `2026-10-01` (2 rows) buckets identically with tz
    let out = run_agg(
        &ws,
        r#"{"groupBy":[{"dateColumn":"day","period":"day","tz":"Asia/Shanghai"}],"aggs":[{"fn":"count","as":"n"}],"orderBy":[{"column":"day_day"}]}"#,
    );
    assert!(out.status.success(), "stderr={}", stderr_of(&out));
    let line = stdout_of(&out);
    assert!(line.contains(r#"["2026-10-01",2]"#), "stdout={line}");
    assert!(line.contains(r#"["2026-10-02",1]"#), "stdout={line}");
    assert!(line.contains(r#"["2026-10-03",1]"#), "stdout={line}");

    let ws = temp_workspace();
    ingest_csv(&ws, "naive.csv", "ts\n2026-09-30 18:00:00\n", ID);
    let out = run_agg(
        &ws,
        r#"{"groupBy":[{"dateColumn":"ts","period":"day","tz":"Asia/Shanghai"}],"aggs":[{"fn":"count","as":"n"}]}"#,
    );
    assert!(out.status.success(), "stderr={}", stderr_of(&out));
    let line = stdout_of(&out);
    assert!(line.contains(r#"["2026-09-30",1]"#), "stdout={line}");
}

/// T1/T2: invalid tz values exit 2 with the exact bad-tz message.
#[test]
fn r29_bad_tz_exits_2() {
    let ws = temp_workspace();
    ingest_rich(&ws);
    let check = |spec: String, show: &str| {
        let out = run_agg(&ws, &spec);
        assert_eq!(out.status.code(), Some(2), "tz {show} must exit 2");
        let err = stderr_of(&out);
        assert!(
            err.contains(&format!(
                "analysis/invalid-args: bad tz \"{show}\" (expected +HH:MM or Asia/Shanghai)"
            )),
            "tz={show} stderr={err}"
        );
        assert!(!stdout_of(&out).contains("\"ok\":true"), "tz={show}");
    };
    for tz in ["Asia/Tokyo", "+15:00"] {
        check(
            format!(
                r#"{{"groupBy":[{{"dateColumn":"day","period":"day","tz":"{tz}"}}],"aggs":[{{"fn":"count","as":"n"}}]}}"#
            ),
            tz,
        );
    }
    // present-but-non-string tz: rejected like the tool layer (was silently UTC)
    for tz in ["null", "480"] {
        check(
            format!(
                r#"{{"groupBy":[{{"dateColumn":"day","period":"day","tz":{tz}}}],"aggs":[{{"fn":"count","as":"n"}}]}}"#
            ),
            tz,
        );
    }
}

// ---- v0.1.12 R-36 / R-37: agg inline rows budget ---------------------------

/// Count top-level elements of the `"rows":[…]` array in one JSON line.
fn rows_len(line: &str) -> usize {
    let key = "\"rows\":[";
    let Some(idx) = line.find(key) else {
        return usize::MAX;
    };
    let mut i = idx + key.len();
    let b = line.as_bytes();
    if i >= b.len() {
        return 0;
    }
    if b[i] == b']' {
        return 0;
    }
    let mut depth = 0usize;
    let mut count = 0usize;
    while i < b.len() {
        match b[i] {
            b'[' => {
                if depth == 0 {
                    count += 1;
                }
                depth += 1;
            }
            b']' => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            _ => {}
        }
        i += 1;
    }
    count
}

fn json_str_field(line: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\":\"");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Build a 21-day day-bucket source (> budget 20) and ingest under `ID`.
fn ingest_21_days(ws: &Path) {
    let mut body = String::from("ts,v\n");
    for i in 1..=21 {
        body.push_str(&format!("2026-01-{i:02}T00:30:00+08:00,{i}\n"));
    }
    ingest_csv(ws, "d21.csv", &body, ID);
}

const DAY_SPEC: &str = r#"{"groupBy":[{"dateColumn":"ts","period":"day","tz":"Asia/Shanghai"}],"aggs":[{"fn":"count","as":"n"}],"orderBy":[{"column":"ts_day"}]}"#;

fn run_agg_out(ws: &Path, spec: &str, out_label: &str) -> String {
    let out = bin()
        .arg("--workspace")
        .arg(ws)
        .args(["agg", "--dataset-id", ID, "--spec", spec, "--out", out_label])
        .output()
        .expect("spawn dsh-analyze");
    assert!(
        out.status.success(),
        "agg --out failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// R-36-A: 21 buckets, no `--out` → 20-row inline preview, truncated=true,
/// outDatasetId=null. Never `rowCount=21 && rows=[]`.
#[test]
fn r36_over_budget_no_out_returns_20_row_preview() {
    let ws = temp_workspace();
    ingest_21_days(&ws);
    let out = run_agg(&ws, DAY_SPEC);
    assert!(out.status.success(), "stderr={}", stderr_of(&out));
    let line = stdout_of(&out);
    assert!(line.contains("\"rowCount\":21"), "stdout={line}");
    assert!(line.contains("\"truncated\":true"), "stdout={line}");
    assert!(line.contains("\"outDatasetId\":null"), "stdout={line}");
    let n = rows_len(&line);
    assert_eq!(n, 20, "expected 20 inline preview rows, got {n}: {line}");
    assert!(!line.contains("\"rows\":[]"), "must not drop rows: {line}");
    // first day present in the preview
    assert!(line.contains(r#"["2026-01-01",1]"#), "stdout={line}");
    // 21st day is beyond the budget
    assert!(!line.contains(r#"["2026-01-21",1]"#), "stdout={line}");
}

/// R-36 regression: ≤20 rows, no `--out` → full rows, truncated=false.
#[test]
fn r36_under_budget_no_out_full_rows() {
    let ws = temp_workspace();
    ingest_21_days(&ws);
    // limit to 10 day buckets
    let spec = format!(
        r#"{{"groupBy":[{{"dateColumn":"ts","period":"day","tz":"Asia/Shanghai"}}],"aggs":[{{"fn":"count","as":"n"}}],"orderBy":[{{"column":"ts_day"}}],"limit":10}}"#
    );
    let out = run_agg(&ws, &spec);
    assert!(out.status.success(), "stderr={}", stderr_of(&out));
    let line = stdout_of(&out);
    assert!(line.contains("\"rowCount\":10"), "stdout={line}");
    assert!(line.contains("\"truncated\":false"), "stdout={line}");
    assert!(line.contains("\"outDatasetId\":null"), "stdout={line}");
    let n = rows_len(&line);
    assert_eq!(n, 10, "expected full 10 rows, got {n}: {line}");
}

/// R-36 regression: `--out` still persists; head can read all 21 rows.
#[test]
fn r36_with_out_persists_full_dataset() {
    let ws = temp_workspace();
    ingest_21_days(&ws);
    let line = run_agg_out(&ws, DAY_SPEC, "d21");
    assert!(line.contains("\"rowCount\":21"), "stdout={line}");
    assert!(line.contains("\"truncated\":true"), "stdout={line}");
    let out_id = json_str_field(&line, "outDatasetId").expect("outDatasetId");
    assert!(out_id.starts_with("ds_"), "got {out_id}");
    // preview still present (R-37)
    let n = rows_len(&line);
    assert_eq!(n, 20, "expected 20 preview rows with --out, got {n}: {line}");

    let head = bin()
        .arg("--workspace")
        .arg(&ws)
        .args(["head", "--dataset-id", &out_id, "--n", "50"])
        .output()
        .expect("spawn dsh-analyze");
    assert!(head.status.success(), "stderr={}", stderr_of(&head));
    let head_line = stdout_of(&head);
    assert!(
        head_line.contains("\"rowCount\":21"),
        "head must see full 21 rows: {head_line}"
    );
}

/// R-37: small result + `--out` → full inline rows AND outDatasetId.
#[test]
fn r37_small_result_with_out_returns_inline_rows() {
    let ws = temp_workspace();
    ingest_21_days(&ws);
    let spec = format!(
        r#"{{"groupBy":[{{"dateColumn":"ts","period":"day","tz":"Asia/Shanghai"}}],"aggs":[{{"fn":"count","as":"n"}}],"orderBy":[{{"column":"ts_day"}}],"limit":3}}"#
    );
    let line = run_agg_out(&ws, &spec, "small3");
    assert!(line.contains("\"rowCount\":3"), "stdout={line}");
    assert!(line.contains("\"truncated\":false"), "stdout={line}");
    let out_id = json_str_field(&line, "outDatasetId").expect("outDatasetId");
    assert!(out_id.starts_with("ds_"), "got {out_id}");
    let n = rows_len(&line);
    assert_eq!(n, 3, "expected full 3 rows with --out, got {n}: {line}");

    let head = bin()
        .arg("--workspace")
        .arg(&ws)
        .args(["head", "--dataset-id", &out_id, "--n", "5"])
        .output()
        .expect("spawn dsh-analyze");
    assert!(head.status.success(), "stderr={}", stderr_of(&head));
    assert!(
        stdout_of(&head).contains("\"rowCount\":3"),
        "head must see 3 rows: {}",
        stdout_of(&head)
    );
}
