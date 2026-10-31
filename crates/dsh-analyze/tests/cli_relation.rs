//! Integration tests against the real `dsh-analyze` binary (relation commands).

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
        "dsh-analyze-relation-{}-{nanos}",
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

/// Minimal JSON number extractor for `"r":<number>`.
fn json_f64_field(line: &str, field: &str) -> Option<f64> {
    let needle = format!("\"{field}\":");
    let start = line.find(&needle)? + needle.len();
    let rest = line[start..].trim_start();
    let end = rest
        .find([',', '}', ']'])
        .unwrap_or(rest.len());
    rest[..end].trim().parse::<f64>().ok()
}

fn json_usize_field(line: &str, field: &str) -> Option<usize> {
    let needle = format!("\"{field}\":");
    let start = line.find(&needle)? + needle.len();
    let rest = line[start..].trim_start();
    let end = rest.find([',', '}', ']']).unwrap_or(rest.len());
    rest[..end].trim().parse::<usize>().ok()
}

fn json_str_field(line: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\":\"");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Pearson r for x=[1,2,3,4,5], y=[2,4,5,4,5].
/// Hand: mean_x=3, mean_y=4; sxy=6, sxx=10, syy=6 → r = 6/sqrt(60) ≈ 0.7745966692414834
#[test]
fn corr_pearson_matches_hand_calc() {
    let ws = temp_workspace();
    write_csv(&ws, "c.csv", "x,y\n1,2\n2,4\n3,5\n4,4\n5,5\n");
    run_ok(
        &ws,
        &[
            "ingest",
            "--input",
            "data/c.csv",
            "--dataset-id",
            "ds_aabbccdd11223344",
        ],
    );

    let out = run_ok(
        &ws,
        &[
            "corr",
            "--dataset-id",
            "ds_aabbccdd11223344",
            "--columns",
            "x,y",
            "--method",
            "pearson",
        ],
    );

    assert!(out.contains("\"ok\":true"), "got: {out}");
    assert!(out.contains("\"method\":\"pearson\""), "got: {out}");
    let r = json_f64_field(&out, "r").expect("r field");
    let expected = 6.0 / (10.0_f64 * 6.0).sqrt();
    assert!(
        (r - expected).abs() < 1e-9,
        "expected r≈{expected}, got {r}\n{out}"
    );
    assert_eq!(json_usize_field(&out, "n"), Some(5), "got: {out}");
    assert!(out.contains("\"degenerate\":false"), "got: {out}");

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn corr_constant_column_degenerate_null() {
    let ws = temp_workspace();
    write_csv(&ws, "c.csv", "x,y\n1,7\n2,7\n3,7\n4,7\n");
    run_ok(
        &ws,
        &[
            "ingest",
            "--input",
            "data/c.csv",
            "--dataset-id",
            "ds_aabbccdd11223344",
        ],
    );

    let out = run_ok(
        &ws,
        &[
            "corr",
            "--dataset-id",
            "ds_aabbccdd11223344",
            "--columns",
            "x,y",
        ],
    );
    assert!(out.contains("\"r\":null"), "got: {out}");
    assert!(out.contains("\"degenerate\":true"), "got: {out}");
    assert!(json_f64_field(&out, "r").is_none());

    // non-numeric column rejected
    write_csv(&ws, "s.csv", "x,s\n1,a\n2,b\n");
    run_ok(
        &ws,
        &[
            "ingest",
            "--input",
            "data/s.csv",
            "--dataset-id",
            "ds_bbccdd1122334455",
        ],
    );
    let stderr = run_fail(
        &ws,
        &[
            "corr",
            "--dataset-id",
            "ds_bbccdd1122334455",
            "--columns",
            "x,s",
        ],
        2,
    );
    assert!(
        stderr.contains("not numeric-capable") || stderr.contains("invalid-args"),
        "stderr={stderr}"
    );

    let _ = fs::remove_dir_all(&ws);
}

/// Hand table:
/// rows: a,b,c  cols: 1,2,3
/// a,1 x2; a,2 x1; b,2 x1; c,3 x2; c,1 x1  → 7 rows
#[test]
fn crosstab_hand_table_and_top_truncation() {
    let ws = temp_workspace();
    write_csv(
        &ws,
        "x.csv",
        "r,c\na,1\na,1\na,2\nb,2\nc,3\nc,3\nc,1\n",
    );
    run_ok(
        &ws,
        &[
            "ingest",
            "--input",
            "data/x.csv",
            "--dataset-id",
            "ds_aabbccdd11223344",
        ],
    );

    let out = run_ok(
        &ws,
        &[
            "crosstab",
            "--dataset-id",
            "ds_aabbccdd11223344",
            "--rows",
            "r",
            "--cols",
            "c",
        ],
    );
    assert!(out.contains("\"ok\":true"), "got: {out}");
    assert_eq!(json_usize_field(&out, "grandTotal"), Some(7), "got: {out}");
    assert_eq!(json_usize_field(&out, "cellCount"), Some(7), "got: {out}");
    // row categories: a=3 first-seen, c=3 later, b=1 → a, c, b
    assert!(
        out.contains("\"rowCategories\":[\"a\",\"c\",\"b\"]"),
        "got: {out}"
    );
    // col: 1→3, 2→2, 3→2; first-seen 1=0,2=2,3=4 → 1,2,3 (I64 cells → JSON numbers)
    assert!(out.contains("\"colCategories\":[1,2,3]"), "got: {out}");
    // matrix a: [2,1,0] c: [1,0,2] b: [0,1,0]
    assert!(out.contains("\"matrix\":[[2,1,0],[1,0,2],[0,1,0]]"), "got: {out}");

    // top=1 → truncated true
    let top1 = run_ok(
        &ws,
        &[
            "crosstab",
            "--dataset-id",
            "ds_aabbccdd11223344",
            "--rows",
            "r",
            "--cols",
            "c",
            "--top",
            "1",
        ],
    );
    assert!(top1.contains("\"truncated\":true"), "got: {top1}");
    assert!(top1.contains("\"rowCategories\":[\"a\"]"), "got: {top1}");

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn join_semi_anti_left_only_and_null_keys() {
    let ws = temp_workspace();
    // left: keys 1,2,3,null ; right: keys 1,1,2 (multi-match on 1)
    write_csv(&ws, "l.csv", "k,vl\n1,L1\n2,L2\n3,L3\n,Lnull\n");
    write_csv(&ws, "r.csv", "k,vr\n1,R1\n1,R1b\n2,R2\n");

    run_ok(
        &ws,
        &[
            "ingest",
            "--input",
            "data/l.csv",
            "--dataset-id",
            "ds_left000000000001",
        ],
    );
    run_ok(
        &ws,
        &[
            "ingest",
            "--input",
            "data/r.csv",
            "--dataset-id",
            "ds_right00000000001",
        ],
    );

    // semi: keys 1 and 2 match → 2 rows, left columns only (k, vl)
    let semi = run_ok(
        &ws,
        &[
            "join",
            "--left",
            "ds_left000000000001",
            "--right",
            "ds_right00000000001",
            "--on",
            "k",
            "--how",
            "semi",
        ],
    );
    assert!(semi.contains("\"how\":\"semi\""), "got: {semi}");
    assert_eq!(json_usize_field(&semi, "rowCount"), Some(2), "got: {semi}");
    assert_eq!(json_usize_field(&semi, "columnCount"), Some(2), "got: {semi}");
    let semi_id = json_str_field(&semi, "datasetId").expect("semi id");
    let head = run_ok(&ws, &["head", "--dataset-id", &semi_id, "--n", "5"]);
    assert!(!head.contains("vr"), "semi must not join right cols: {head}");
    assert!(head.contains("L1") && head.contains("L2"), "got: {head}");

    // anti: keys 3 and null → 2 rows
    let anti = run_ok(
        &ws,
        &[
            "join",
            "--left",
            "ds_left000000000001",
            "--right",
            "ds_right00000000001",
            "--on",
            "k",
            "--how",
            "anti",
        ],
    );
    assert!(anti.contains("\"how\":\"anti\""), "got: {anti}");
    assert_eq!(json_usize_field(&anti, "rowCount"), Some(2), "got: {anti}");
    assert_eq!(json_usize_field(&anti, "columnCount"), Some(2), "got: {anti}");
    let anti_id = json_str_field(&anti, "datasetId").expect("anti id");
    let ahead = run_ok(&ws, &["head", "--dataset-id", &anti_id, "--n", "5"]);
    assert!(ahead.contains("L3") && ahead.contains("Lnull"), "got: {ahead}");
    assert!(!ahead.contains("L1"), "anti should drop matched: {ahead}");

    // multi-match semi still once per left row: key 1 appears once
    let semi_head = run_ok(&ws, &["head", "--dataset-id", &semi_id, "--n", "5"]);
    assert!(
        !semi_head.contains("R1b"),
        "semi must not expand right multi-match: {semi_head}"
    );

    // bad how
    let stderr = run_fail(
        &ws,
        &[
            "join",
            "--left",
            "ds_left000000000001",
            "--right",
            "ds_right00000000001",
            "--on",
            "k",
            "--how",
            "outer",
        ],
        2,
    );
    assert!(stderr.contains("inner|left|anti|semi"), "stderr={stderr}");

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn version_capabilities_include_corr_crosstab() {
    let out = {
        let out = bin().arg("version").output().expect("spawn");
        assert!(out.status.success(), "version failed");
        String::from_utf8(out.stdout).unwrap()
    };
    assert!(out.contains("\"0.1.4\""), "got: {out}");
    for tok in [
        "\"ingest\"",
        "\"head\"",
        "\"schema\"",
        "\"describe\"",
        "\"agg\"",
        "\"list\"",
        "\"export\"",
        "\"rm\"",
        "\"values\"",
        "\"sample\"",
        "\"join\"",
        "\"profile\"",
        "\"dedup\"",
        "\"outliers\"",
        "\"corr\"",
        "\"crosstab\"",
    ] {
        assert!(out.contains(tok), "missing {tok} in {out}");
    }
    // order: corr then crosstab last
    let corr_at = out.find("\"corr\"").expect("corr");
    let ct_at = out.find("\"crosstab\"").expect("crosstab");
    let out_at = out.find("\"outliers\"").expect("outliers");
    assert!(out_at < corr_at && corr_at < ct_at, "order wrong: {out}");
}
