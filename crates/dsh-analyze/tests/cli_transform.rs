//! Integration tests against the real `dsh-analyze` binary (union / diff / pivot / join --on).

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
        "dsh-analyze-transform-{}-{n}-{nanos}",
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

// ---------------------------------------------------------------- union (R-8a)

#[test]
fn union_strict_appends_and_reports_counts() {
    let ws = temp_workspace();
    write_csv(&ws, "l.csv", "id,amount\n1,10\n2,20\n");
    write_csv(&ws, "r.csv", "id,amount\n3,30\n");
    ingest(&ws, "l.csv", "ds_left000011112222");
    ingest(&ws, "r.csv", "ds_right00011112222");

    let out = run_ok(
        &ws,
        &[
            "union",
            "--left",
            "ds_left000011112222",
            "--right",
            "ds_right00011112222",
            "--out",
            "merged",
        ],
    );
    assert_eq!(json_usize_field(&out, "rowCount"), Some(3), "{out}");
    assert_eq!(json_usize_field(&out, "leftRows"), Some(2), "{out}");
    assert_eq!(json_usize_field(&out, "rightRows"), Some(1), "{out}");
    let new_id = json_str_field(&out, "datasetId").expect("datasetId");
    assert!(new_id.starts_with("ds_"), "{out}");
    let head = run_ok(&ws, &["head", "--dataset-id", &new_id, "--n", "10"]);
    assert!(head.contains("[3,30]"), "right rows appended: {head}");
    let list = run_ok(&ws, &["list"]);
    assert!(list.contains("\"producedBy\":\"union\""), "{list}");
    assert!(
        list.contains("\"parentDatasetId\":\"ds_left000011112222\""),
        "parent must be left: {list}"
    );

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn union_strict_mismatch_names_sides() {
    let ws = temp_workspace();
    write_csv(&ws, "l.csv", "id,amount\n1,10\n");
    write_csv(&ws, "r.csv", "id\n2\n");
    ingest(&ws, "l.csv", "ds_left000011112222");
    ingest(&ws, "r.csv", "ds_right00011112222");

    let err = run_fail(
        &ws,
        &[
            "union",
            "--left",
            "ds_left000011112222",
            "--right",
            "ds_right00011112222",
        ],
        2,
    );
    assert!(
        err.contains("analysis/invalid-args: union column mismatch: left-only amount"),
        "stderr={err}"
    );

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn union_intersect_keeps_shared_columns() {
    let ws = temp_workspace();
    write_csv(&ws, "l.csv", "id,amount\n1,10\n2,20\n");
    write_csv(&ws, "r.csv", "id,qty\n3,5\n");
    ingest(&ws, "l.csv", "ds_left000011112222");
    ingest(&ws, "r.csv", "ds_right00011112222");

    let out = run_ok(
        &ws,
        &[
            "union",
            "--left",
            "ds_left000011112222",
            "--right",
            "ds_right00011112222",
            "--columns-policy",
            "intersect",
        ],
    );
    assert_eq!(json_usize_field(&out, "rowCount"), Some(3), "{out}");
    assert!(out.contains("\"columns\":[{\"name\":\"id\""), "{out}");
    assert!(!out.contains("\"name\":\"amount\""), "{out}");
    assert!(!out.contains("\"name\":\"qty\""), "{out}");

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn union_intersect_without_shared_columns_fails() {
    let ws = temp_workspace();
    write_csv(&ws, "l.csv", "a\n1\n");
    write_csv(&ws, "r.csv", "b\n2\n");
    ingest(&ws, "l.csv", "ds_left000011112222");
    ingest(&ws, "r.csv", "ds_right00011112222");

    let err = run_fail(
        &ws,
        &[
            "union",
            "--left",
            "ds_left000011112222",
            "--right",
            "ds_right00011112222",
            "--columns-policy",
            "intersect",
        ],
        2,
    );
    assert!(
        err.contains("union column mismatch: no shared columns"),
        "stderr={err}"
    );

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn union_dtype_conflict_promotes_to_utf8() {
    let ws = temp_workspace();
    write_csv(&ws, "l.csv", "id,amount\n1,10\n");
    write_csv(&ws, "r.csv", "id,amount\n2,abc\n");
    ingest(&ws, "l.csv", "ds_left000011112222");
    ingest(&ws, "r.csv", "ds_right00011112222");

    let out = run_ok(
        &ws,
        &[
            "union",
            "--left",
            "ds_left000011112222",
            "--right",
            "ds_right00011112222",
        ],
    );
    assert!(
        out.contains("{\"name\":\"amount\",\"dtype\":\"utf8\"}"),
        "mixed dtype must promote to utf8: {out}"
    );

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn union_row_cap_uses_max_rows_limit() {
    let ws = temp_workspace();
    write_csv(&ws, "l.csv", "id\n1\n2\n");
    write_csv(&ws, "r.csv", "id\n3\n");
    ingest(&ws, "l.csv", "ds_left000011112222");
    ingest(&ws, "r.csv", "ds_right00011112222");

    let out = bin()
        .arg("--workspace")
        .arg(&ws)
        .env("DSH_ANALYZE_MAX_ROWS", "2")
        .args([
            "union",
            "--left",
            "ds_left000011112222",
            "--right",
            "ds_right00011112222",
        ])
        .output()
        .expect("spawn dsh-analyze");
    assert_eq!(out.status.code(), Some(3), "limit exit code");
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        err.contains("analysis/limit: union result too large"),
        "stderr={err}"
    );

    let _ = fs::remove_dir_all(&ws);
}

// ----------------------------------------------------------------- diff (R-8b)

#[test]
fn diff_multiset_counts_and_samples() {
    let ws = temp_workspace();
    write_csv(&ws, "l.csv", "id,amount\n1,10\n2,20\n3,30\n");
    write_csv(&ws, "r.csv", "id,amount\n2,20\n3,30\n4,40\n");
    ingest(&ws, "l.csv", "ds_left000011112222");
    ingest(&ws, "r.csv", "ds_right00011112222");

    let out = run_ok(
        &ws,
        &[
            "diff",
            "--left",
            "ds_left000011112222",
            "--right",
            "ds_right00011112222",
        ],
    );
    assert_eq!(json_usize_field(&out, "onlyLeft"), Some(1), "{out}");
    assert_eq!(json_usize_field(&out, "onlyRight"), Some(1), "{out}");
    assert_eq!(json_usize_field(&out, "changed"), Some(0), "{out}");
    assert!(out.contains("\"onlyLeft\":[{\"id\":1,\"amount\":10}]"), "{out}");
    assert!(out.contains("\"onlyRight\":[{\"id\":4,\"amount\":40}]"), "{out}");
    assert!(out.contains("\"changed\":[]}"), "{out}");
    assert!(!out.contains("\"warnings\""), "no clamp: {out}");

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn diff_keyed_reports_changed_with_diff_columns() {
    let ws = temp_workspace();
    write_csv(&ws, "l.csv", "k,amount\n1,10\n2,20\n3,30\n");
    write_csv(&ws, "r.csv", "k,amount\n1,10\n2,99\n4,40\n");
    ingest(&ws, "l.csv", "ds_left000011112222");
    ingest(&ws, "r.csv", "ds_right00011112222");

    let out = run_ok(
        &ws,
        &[
            "diff",
            "--left",
            "ds_left000011112222",
            "--right",
            "ds_right00011112222",
            "--on",
            "k",
        ],
    );
    assert_eq!(json_usize_field(&out, "onlyLeft"), Some(1), "{out}");
    assert_eq!(json_usize_field(&out, "onlyRight"), Some(1), "{out}");
    assert_eq!(json_usize_field(&out, "changed"), Some(1), "{out}");
    assert!(
        out.contains("\"key\":{\"k\":2}") && out.contains("\"diffColumns\":[\"amount\"]"),
        "{out}"
    );
    assert!(
        out.contains("\"left\":{\"k\":2,\"amount\":20}")
            && out.contains("\"right\":{\"k\":2,\"amount\":99}"),
        "{out}"
    );
    // diff persists a report dataset: produced_by diff, parent = left.
    let list = run_ok(&ws, &["list"]);
    assert!(list.contains("\"producedBy\":\"diff\""), "{list}");
    assert!(
        list.contains("\"parentDatasetId\":\"ds_left000011112222\""),
        "parent must be left: {list}"
    );

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn diff_multi_key_and_left_right_key_pairs() {
    let ws = temp_workspace();
    write_csv(&ws, "l.csv", "k1,k2,amount\n1,a,10\n2,b,20\n");
    write_csv(&ws, "r.csv", "kk1,kk2,amount\n1,a,10\n2,b,22\n");
    ingest(&ws, "l.csv", "ds_left000011112222");
    ingest(&ws, "r.csv", "ds_right00011112222");

    let out = run_ok(
        &ws,
        &[
            "diff",
            "--left",
            "ds_left000011112222",
            "--right",
            "ds_right00011112222",
            "--on",
            "k1:kk1,k2:kk2",
        ],
    );
    assert_eq!(json_usize_field(&out, "changed"), Some(1), "{out}");
    assert_eq!(json_usize_field(&out, "onlyLeft"), Some(0), "{out}");
    assert_eq!(json_usize_field(&out, "onlyRight"), Some(0), "{out}");
    assert!(
        out.contains("\"key\":{\"k1\":2,\"k2\":\"b\"}"),
        "key uses left names: {out}"
    );
    assert!(out.contains("\"diffColumns\":[\"amount\"]"), "{out}");

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn diff_null_keys_never_match() {
    let ws = temp_workspace();
    write_csv(&ws, "l.csv", "k,amount\n1,10\n,20\n");
    write_csv(&ws, "r.csv", "k,amount\n,22\n");
    ingest(&ws, "l.csv", "ds_left000011112222");
    ingest(&ws, "r.csv", "ds_right00011112222");

    let out = run_ok(
        &ws,
        &[
            "diff",
            "--left",
            "ds_left000011112222",
            "--right",
            "ds_right00011112222",
            "--on",
            "k",
        ],
    );
    assert_eq!(json_usize_field(&out, "onlyLeft"), Some(2), "{out}");
    assert_eq!(json_usize_field(&out, "onlyRight"), Some(1), "{out}");
    assert_eq!(json_usize_field(&out, "changed"), Some(0), "{out}");

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn diff_sample_limit_clamps_to_20_with_warning() {
    let ws = temp_workspace();
    let mut l = String::from("id\n");
    for i in 0..25 {
        l.push_str(&format!("{i}\n"));
    }
    write_csv(&ws, "l.csv", &l);
    write_csv(&ws, "r.csv", "id\n999\n");
    ingest(&ws, "l.csv", "ds_left000011112222");
    ingest(&ws, "r.csv", "ds_right00011112222");

    let out = run_ok(
        &ws,
        &[
            "diff",
            "--left",
            "ds_left000011112222",
            "--right",
            "ds_right00011112222",
            "--sample-limit",
            "50",
        ],
    );
    assert_eq!(json_usize_field(&out, "onlyLeft"), Some(25), "{out}");
    assert!(
        out.contains(
            "{\"code\":\"clamp\",\"field\":\"sampleLimit\",\"requested\":50,\"actual\":20}"
        ),
        "clamp warning expected: {out}"
    );
    // samples are capped at the clamped limit
    let after = &out[out.find("\"onlyLeft\":[").unwrap()..];
    let only_left_arr = &after[..after.find("],\"onlyRight\"").unwrap()];
    let sample_count = only_left_arr.matches("{\"id\":").count();
    assert!(sample_count <= 20, "samples must be <= 20: {out}");

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn diff_rejects_bad_sample_limit() {
    let ws = temp_workspace();
    write_csv(&ws, "l.csv", "id\n1\n");
    write_csv(&ws, "r.csv", "id\n2\n");
    ingest(&ws, "l.csv", "ds_left000011112222");
    ingest(&ws, "r.csv", "ds_right00011112222");

    for bad in ["0", "abc", "-3"] {
        let err = run_fail(
            &ws,
            &[
                "diff",
                "--left",
                "ds_left000011112222",
                "--right",
                "ds_right00011112222",
                "--sample-limit",
                bad,
            ],
            2,
        );
        assert!(err.contains("sample-limit"), "stderr={err}");
    }

    let _ = fs::remove_dir_all(&ws);
}

// ------------------------------------------------------------- join --on (R-10)

#[test]
fn join_on_pair_different_key_names() {
    let ws = temp_workspace();
    write_csv(&ws, "l.csv", "order_id,amount\n1,10\n2,20\n");
    write_csv(&ws, "r.csv", "oid,qty\n1,5\n2,6\n");
    ingest(&ws, "l.csv", "ds_left000011112222");
    ingest(&ws, "r.csv", "ds_right00011112222");

    let out = run_ok(
        &ws,
        &[
            "join",
            "--left",
            "ds_left000011112222",
            "--right",
            "ds_right00011112222",
            "--on",
            "order_id:oid",
        ],
    );
    assert_eq!(json_usize_field(&out, "rowCount"), Some(2), "{out}");
    let new_id = json_str_field(&out, "datasetId").expect("datasetId");
    let head = run_ok(&ws, &["head", "--dataset-id", &new_id, "--n", "10"]);
    assert!(head.contains("[1,10,5]"), "joined rows: {head}");

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn join_on_errors_name_the_side() {
    let ws = temp_workspace();
    write_csv(&ws, "l.csv", "order_id,amount\n1,10\n");
    write_csv(&ws, "r.csv", "oid,qty\n1,5\n");
    ingest(&ws, "l.csv", "ds_left000011112222");
    ingest(&ws, "r.csv", "ds_right00011112222");

    let err = run_fail(
        &ws,
        &[
            "join",
            "--left",
            "ds_left000011112222",
            "--right",
            "ds_right00011112222",
            "--on",
            "nope:oid",
        ],
        2,
    );
    assert!(
        err.contains("analysis/invalid-args: unknown column nope on left"),
        "stderr={err}"
    );

    let err = run_fail(
        &ws,
        &[
            "join",
            "--left",
            "ds_left000011112222",
            "--right",
            "ds_right00011112222",
            "--on",
            "order_id:nope",
        ],
        2,
    );
    assert!(
        err.contains("analysis/invalid-args: unknown column nope on right"),
        "stderr={err}"
    );

    let err = run_fail(
        &ws,
        &[
            "join",
            "--left",
            "ds_left000011112222",
            "--right",
            "ds_right00011112222",
            "--on",
            "order_id:oid,order_id:qty",
        ],
        2,
    );
    assert!(
        err.contains("analysis/invalid-args: duplicate join key order_id"),
        "stderr={err}"
    );

    let err = run_fail(
        &ws,
        &[
            "join",
            "--left",
            "ds_left000011112222",
            "--right",
            "ds_right00011112222",
            "--on",
            "order_id:oid:extra",
        ],
        2,
    );
    assert!(
        err.contains("analysis/invalid-args: on pairs must be leftKey:rightKey"),
        "stderr={err}"
    );

    let _ = fs::remove_dir_all(&ws);
}

// ---------------------------------------------------------------- pivot (R-16b)

#[test]
fn pivot_writes_wide_dataset() {
    let ws = temp_workspace();
    write_csv(
        &ws,
        "p.csv",
        "region,quarter,amount\nEast,Q1,10\nEast,Q2,20\nWest,Q1,30\n",
    );
    ingest(&ws, "p.csv", "ds_src00001111222233");

    let out = run_ok(
        &ws,
        &[
            "pivot",
            "--dataset-id",
            "ds_src00001111222233",
            "--index",
            "region",
            "--columns",
            "quarter",
            "--values",
            "amount",
            "--agg",
            "sum",
        ],
    );
    assert_eq!(json_usize_field(&out, "rowCount"), Some(2), "{out}");
    assert!(out.contains("\"indexColumns\":[\"region\"]"), "{out}");
    assert!(out.contains("\"pivotColumn\":\"quarter\""), "{out}");
    assert!(
        out.contains("{\"name\":\"Q1\"") && out.contains("{\"name\":\"Q2\""),
        "pivot value columns: {out}"
    );
    let new_id = json_str_field(&out, "datasetId").expect("datasetId");
    let head = run_ok(&ws, &["head", "--dataset-id", &new_id, "--n", "10"]);
    assert!(
        head.contains("[\"East\",10,20]") && head.contains("[\"West\",30,null]"),
        "wide rows: {head}"
    );
    let list = run_ok(&ws, &["list"]);
    assert!(list.contains("\"producedBy\":\"pivot\""), "{list}");
    assert!(
        list.contains("\"parentDatasetId\":\"ds_src00001111222233\""),
        "{list}"
    );

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn pivot_exceeds_50_columns_fails() {
    let ws = temp_workspace();
    let mut body = String::from("idx,kind,v\n");
    for i in 0..51 {
        body.push_str(&format!("r1,K{i},{i}\n"));
    }
    write_csv(&ws, "p.csv", &body);
    ingest(&ws, "p.csv", "ds_src00001111222233");

    let err = run_fail(
        &ws,
        &[
            "pivot",
            "--dataset-id",
            "ds_src00001111222233",
            "--index",
            "idx",
            "--columns",
            "kind",
            "--values",
            "v",
            "--agg",
            "sum",
        ],
        3,
    );
    assert!(
        err.contains("analysis/limit: pivot columns exceed 50"),
        "stderr={err}"
    );

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn pivot_rejects_bad_agg() {
    let ws = temp_workspace();
    write_csv(&ws, "p.csv", "a,b,v\n1,x,2\n");
    ingest(&ws, "p.csv", "ds_src00001111222233");
    let err = run_fail(
        &ws,
        &[
            "pivot",
            "--dataset-id",
            "ds_src00001111222233",
            "--index",
            "a",
            "--columns",
            "b",
            "--values",
            "v",
            "--agg",
            "median",
        ],
        2,
    );
    assert!(err.contains("--agg must be sum|mean|min|max|count"), "stderr={err}");
    let _ = fs::remove_dir_all(&ws);
}
