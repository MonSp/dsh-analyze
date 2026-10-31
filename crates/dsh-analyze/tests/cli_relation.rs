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

/// String keys that merely contain "null" are valid join keys (only real Null is missing).
#[test]
fn join_string_keys_with_null_substring() {
    let ws = temp_workspace();
    write_csv(
        &ws,
        "ls.csv",
        "k,vl\nnullable,L-nsh\nnullify,L-nfy\na,L-a\n,L-missing\n",
    );
    write_csv(&ws, "rs.csv", "k,vr\nnullable,R-nsh\nnullify,R-nfy\nb,R-b\n");
    run_ok(
        &ws,
        &[
            "ingest",
            "--input",
            "data/ls.csv",
            "--dataset-id",
            "ds_left000000000001",
        ],
    );
    run_ok(
        &ws,
        &[
            "ingest",
            "--input",
            "data/rs.csv",
            "--dataset-id",
            "ds_right00000000001",
        ],
    );

    let inner = run_ok(
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
            "inner",
        ],
    );
    assert_eq!(json_usize_field(&inner, "rowCount"), Some(2), "got: {inner}");
    let id = json_str_field(&inner, "datasetId").expect("inner id");
    let head = run_ok(&ws, &["head", "--dataset-id", &id, "--n", "5"]);
    assert!(head.contains("nullable") && head.contains("nullify"), "got: {head}");

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
    assert_eq!(json_usize_field(&anti, "rowCount"), Some(2), "got: {anti}");

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

/// Spearman on x=[1,2,3], y=[1,3,2]: mid-ranks = identity; pearson(ranks)=0.5.
#[test]
fn corr_spearman_ties_hand_calc() {
    let ws = temp_workspace();
    write_csv(&ws, "c.csv", "x,y\n1,1\n2,3\n3,2\n");
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
            "spearman",
        ],
    );
    assert!(out.contains("\"method\":\"spearman\""), "got: {out}");
    let r = json_f64_field(&out, "r").expect("r field");
    assert!(
        (r - 0.5).abs() < 1e-12,
        "expected spearman r=0.5, got {r}\n{out}"
    );
    assert_eq!(json_usize_field(&out, "n"), Some(3), "got: {out}");

    let _ = fs::remove_dir_all(&ws);
}

/// Listwise omit keeps only rows where every listed column is numeric.
#[test]
fn corr_pairwise_omit_is_listwise() {
    let ws = temp_workspace();
    // rows 1 and 3 have null y; omit on (x,y) → n=2; complete → n=2 as well.
    // With (x,y,z): omit uses only row 0 (all three present); complete uses per-pair.
    write_csv(
        &ws,
        "c.csv",
        "x,y,z\n1,2,10\n2,,10\n3,6,10\n4,,9\n",
    );
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

    // complete on x,y: rows 0 and 2 → n=2, r=1
    let complete = run_ok(
        &ws,
        &[
            "corr",
            "--dataset-id",
            "ds_aabbccdd11223344",
            "--columns",
            "x,y",
            "--pairwise",
            "complete",
        ],
    );
    assert_eq!(json_usize_field(&complete, "n"), Some(2), "got: {complete}");
    let r_c = json_f64_field(&complete, "r").expect("r");
    assert!((r_c - 1.0).abs() < 1e-12, "got: {complete}");

    // omit listwise: rows where x,y,z are ALL numeric → rows 0 and 2 → n=2 for every pair.
    let omit = run_ok(
        &ws,
        &[
            "corr",
            "--dataset-id",
            "ds_aabbccdd11223344",
            "--columns",
            "x,y,z",
            "--pairwise",
            "omit",
        ],
    );
    assert!(omit.contains("\"pairwise\":\"omit\""), "got: {omit}");
    for n in json_all_usize_field(&omit, "n") {
        assert_eq!(n, 2, "omit listwise keeps only full rows: {omit}");
    }
    // x,z under omit: rows (1,10) and (3,10) → constant z → degenerate, not n=4
    assert!(omit.contains("\"degenerate\":true"), "got: {omit}");

    let _ = fs::remove_dir_all(&ws);
}

/// min-numeric: n below threshold → r null, degenerate false.
#[test]
fn corr_min_numeric_threshold() {
    let ws = temp_workspace();
    write_csv(&ws, "c.csv", "x,y\n1,2\n3,4\n5,6\n");
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
            "--min-numeric",
            "4",
        ],
    );
    assert!(out.contains("\"minNumeric\":4"), "got: {out}");
    assert!(out.contains("\"r\":null"), "got: {out}");
    assert!(out.contains("\"degenerate\":false"), "got: {out}");
    assert_eq!(json_usize_field(&out, "n"), Some(3), "got: {out}");

    let _ = fs::remove_dir_all(&ws);
}

/// corr rejects duplicate column names.
#[test]
fn corr_rejects_duplicate_columns() {
    let ws = temp_workspace();
    write_csv(&ws, "c.csv", "x,y\n1,2\n3,4\n");
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
    let stderr = run_fail(
        &ws,
        &[
            "corr",
            "--dataset-id",
            "ds_aabbccdd11223344",
            "--columns",
            "x,x",
        ],
        2,
    );
    assert!(stderr.contains("duplicate"), "stderr={stderr}");
    let _ = fs::remove_dir_all(&ws);
}

/// drop-null false: Null is its own category (JSON label `__null__`),
/// distinct from the literal string `"__null__"`.
#[test]
fn crosstab_drop_null_false_null_is_separate_category() {
    let ws = temp_workspace();
    // r: x, x, "__null__" (string); c: 1, null, 2
    write_csv(&ws, "x.csv", "r,c\nx,1\nx,\n__null__,2\n");
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
            "--drop-null",
            "false",
        ],
    );
    assert!(out.contains("\"nullRowCount\":0"), "got: {out}");
    assert_eq!(json_usize_field(&out, "grandTotal"), Some(3), "got: {out}");
    // 2 distinct row categories (x + string "__null__"), 3 col categories (1, null, 2)
    let rows_json = extract_array(&out, "rowCategories");
    assert_eq!(
        rows_json.matches("\"__null__\"").count(),
        1,
        "string __null__ category: {out}"
    );
    assert!(
        rows_json.contains("null"),
        "null category labeled __null__ or null: {rows_json} / {out}"
    );
    // matrix must be 2×3 (not merged): two outer rows
    let matrix = extract_array(&out, "matrix");
    assert!(
        matrix.starts_with("[[") && matrix.matches("],[").count() == 1,
        "expected 2×3 matrix: {out}"
    );
    // cell counts: each of 3 cells is 1
    assert_eq!(json_usize_field(&out, "cellCount"), Some(3), "got: {out}");

    // drop-null true: only the (x, null) row is dropped → 2 cells remain
    let dropped = run_ok(
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
    assert_eq!(json_usize_field(&dropped, "nullRowCount"), Some(1), "got: {dropped}");
    assert_eq!(json_usize_field(&dropped, "grandTotal"), Some(2), "got: {dropped}");

    let _ = fs::remove_dir_all(&ws);
}

/// Categories containing `|` must not break the matrix.
#[test]
fn crosstab_pipe_in_category_values() {
    let ws = temp_workspace();
    write_csv(&ws, "x.csv", "r,c\na|b,1\na|b,2\nc|d,1\n");
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
    assert_eq!(json_usize_field(&out, "grandTotal"), Some(3), "got: {out}");
    assert!(
        out.contains("\"rowCategories\":[\"a|b\",\"c|d\"]"),
        "got: {out}"
    );
    // rows: a|b → [1,1]; c|d → [1,0]
    assert!(out.contains("\"matrix\":[[1,1],[1,0]]"), "got: {out}");
    let _ = fs::remove_dir_all(&ws);
}

/// Pull every `"n":<usize>` occurrence (pair list has multiple).
fn json_all_usize_field(line: &str, field: &str) -> Vec<usize> {
    let needle = format!("\"{field}\":");
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find(&needle) {
        let after = &rest[start + needle.len()..];
        let end = after.find([',', '}', ']']).unwrap_or(after.len());
        if let Ok(v) = after[..end].trim().parse::<usize>() {
            out.push(v);
        }
        rest = &rest[start + needle.len()..];
    }
    out
}

/// Extract a top-level JSON array value by key (naive but enough for tests).
fn extract_array(line: &str, field: &str) -> String {
    let needle = format!("\"{field}\":[");
    let start = line.find(&needle).expect("field") + needle.len() - 1;
    let mut depth = 0usize;
    for (i, ch) in line[start..].char_indices() {
        match ch {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return line[start..=start + i].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("unterminated array for {field}");
}
