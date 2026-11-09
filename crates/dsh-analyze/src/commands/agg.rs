use std::fs;
use std::path::Path;

use crate::cli::{env_u64, fail, flag_value};
use crate::dataset::{
    ensure_within_workspace, json_string, load_dataset, new_dataset_id, save_meta,
    write_table_parquet_placeholder, Meta,
};

pub(crate) fn cmd_agg(workspace: &Path, args: &[String]) -> Result<String, u8> {
    let id = match flag_value(args, "--dataset-id") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --dataset-id required"),
    };
    let spec_raw = match flag_value(args, "--spec") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --spec required"),
    };
    // Spec may be inline JSON object or a workspace-relative file path (R-3).
    let spec_text = resolve_spec(workspace, &spec_raw)?;
    // Windows tools may write a UTF-8 BOM; trim() does not strip U+FEFF.
    let spec_text = spec_text.strip_prefix('\u{feff}').unwrap_or(&spec_text);
    let out = flag_value(args, "--out").map(|s| s.to_string());
    let default_limit: usize = env_u64(
        "DSH_ANALYZE_DEFAULT_ROW_LIMIT",
        "INV_ANALYSIS_DEFAULT_ROW_LIMIT",
        1000,
    ) as usize;
    let (_meta, table) = load_dataset(workspace, &id).map_err(|e| {
        eprintln!("{e}");
        if e.contains("unknown") {
            5u8
        } else {
            2u8
        }
    })?;
    let spec = crate::agg::parse_spec_json(&spec_text).map_err(|e| {
        eprintln!("{e}");
        2u8
    })?;
    let outcome = crate::agg::run_agg(&table, &spec, default_limit, 5000).map_err(|e| {
        eprintln!("{e}");
        2u8
    })?;
    let result = outcome.table;

    let mut out_dataset_id = String::new();
    if let Some(out_label) = out {
        let new_id = new_dataset_id();
        let (bytes, _) = write_table_parquet_placeholder(workspace, &new_id, &result).map_err(|e| {
            eprintln!("{e}");
            1u8
        })?;
        let _ = bytes;
        let meta = Meta {
            dataset_id: new_id.clone(),
            row_count: result.rows.len(),
            columns: result.columns.clone(),
            source_kind: "csv".into(),
            source_path: format!("agg:{id}"),
            label: Some(out_label),
            produced_by: "agg".into(),
            parent_dataset_id: Some(id.clone()),
            parent_dataset_ids: vec![id.clone()],
            source_checksum: None,
        };
        save_meta(workspace, &meta).map_err(|e| {
            eprintln!("{e}");
            1u8
        })?;
        out_dataset_id = new_id;
    }

    // R-36-A + R-37: always inline-preview up to `budget_rows` rows.
    // Full result still requires `--out` / `export` when rowCount > budget.
    // With `--out`, data is also persisted; inline rows are a preview only.
    let budget_rows = 20;
    let include_count = result.rows.len().min(budget_rows);
    let truncated = result.rows.len() > budget_rows;
    let cols: Vec<String> = result.columns.iter().map(|c| c.name.clone()).collect();
    let rows_json = result
        .rows
        .iter()
        .take(include_count)
        .map(|row| {
            format!(
                "[{}]",
                row.iter().map(|v| v.to_json()).collect::<Vec<_>>().join(",")
            )
        })
        .collect::<Vec<_>>()
        .join(",");

    let mut extra = String::new();
    if outcome.parse_fail_count > 0 {
        extra.push_str(&format!(
            ",\"parseFailCount\":{},\"parseFailSample\":[{}]",
            outcome.parse_fail_count,
            outcome
                .parse_fail_sample
                .iter()
                .map(|s| json_string(s))
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    if !outcome.warnings.is_empty() {
        let entries: Vec<String> = outcome
            .warnings
            .iter()
            .map(|w| {
                let mut parts = vec![format!("\"code\":{}", json_string(w.code))];
                if let Some(col) = &w.column {
                    parts.push(format!("\"column\":{}", json_string(col)));
                }
                parts.push(format!("\"count\":{}", w.count));
                format!("{{{}}}", parts.join(","))
            })
            .collect();
        extra.push_str(&format!(",\"warnings\":[{}]", entries.join(",")));
    }

    Ok(format!(
        "{{\"ok\":true,\"datasetId\":{},\"outDatasetId\":{},\"rowCount\":{},\"columns\":[{}],\"rows\":[{}],\"truncated\":{}{extra}}}",
        json_string(&id),
        if out_dataset_id.is_empty() {
            "null".into()
        } else {
            json_string(&out_dataset_id)
        },
        result.rows.len(),
        cols.iter().map(|c| json_string(c)).collect::<Vec<_>>().join(","),
        rows_json,
        if truncated { "true" } else { "false" }
    ))
}

/// Resolve `--spec` as inline JSON object or a workspace file (R-3).
/// Never leaks OS error text; non-JSON non-file input gets a stable message.
fn resolve_spec(workspace: &Path, spec_raw: &str) -> Result<String, u8> {
    let t = spec_raw.trim_start();
    if t.starts_with('{') {
        return Ok(spec_raw.to_string());
    }
    if !looks_like_workspace_path(spec_raw) {
        return fail(
            2,
            "analysis/invalid-args: --spec must be JSON object or a workspace path",
        );
    }
    let spec_path = match ensure_within_workspace(workspace, Path::new(spec_raw)) {
        Ok(p) => p,
        Err(e) if e.contains("escapes") => {
            return fail(2, &e);
        }
        Err(_) => {
            // Missing parent dirs / unreadable parents: stable reason, no OS text.
            return fail(2, "analysis/invalid-args: cannot read spec file: file not found");
        }
    };
    if !spec_path.exists() {
        return fail(2, "analysis/invalid-args: cannot read spec file: file not found");
    }
    match fs::read_to_string(&spec_path) {
        Ok(text) => Ok(text),
        Err(_) => fail(2, "analysis/invalid-args: cannot read spec file: file not readable"),
    }
}

/// A spec path candidate: no whitespace and no filename-illegal characters
/// (after an optional `C:\` drive prefix). `a:b:c`, `diff:amount:d`, `not json`
/// are therefore NOT paths and get the "must be JSON object or a workspace
/// path" error instead of a leaked OS message.
fn looks_like_workspace_path(s: &str) -> bool {
    if s.trim().is_empty() {
        return false;
    }
    let body = if s.len() >= 3
        && s.as_bytes()[0].is_ascii_alphabetic()
        && s.as_bytes()[1] == b':'
        && (s.as_bytes()[2] == b'\\' || s.as_bytes()[2] == b'/')
    {
        &s[2..]
    } else {
        s
    };
    !body
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|'))
}
