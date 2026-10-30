use std::fs;
use std::path::Path;

use crate::cli::{env_u64, fail, flag_value};
use crate::dataset::{
    ensure_within_workspace, json_string, new_dataset_id, parse_csv_path, parse_jsonl_path,
    save_meta, write_table_parquet_placeholder, Meta,
};

pub(crate) fn cmd_ingest(workspace: &Path, args: &[String]) -> Result<String, u8> {
    let input = match flag_value(args, "--input") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --input required"),
    };
    let format = flag_value(args, "--format")
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            let lower = input.to_ascii_lowercase();
            if lower.ends_with(".parquet") {
                "parquet".into()
            } else if lower.ends_with(".jsonl") {
                "jsonl".into()
            } else {
                "csv".into()
            }
        });
    let label = flag_value(args, "--label").map(|s| s.to_string());
    let dataset_id = flag_value(args, "--dataset-id")
        .map(|s| s.to_string())
        .unwrap_or_else(new_dataset_id);

    let path = ensure_within_workspace(workspace, Path::new(&input)).map_err(|e| {
        eprintln!("{e}");
        2u8
    })?;
    if !path.is_file() {
        return fail(2, "analysis/invalid-path: not a file");
    }
    let max_bytes: u64 = env_u64(
        "DSH_ANALYZE_MAX_INGEST_BYTES",
        "INV_ANALYSIS_MAX_INGEST_BYTES",
        256 * 1024 * 1024,
    );
    let meta_fs = fs::metadata(&path).map_err(|e| {
        eprintln!("analysis/cli-failed: {e}");
        1u8
    })?;
    if meta_fs.len() > max_bytes {
        return fail(3, "analysis/limit: input too large");
    }

    let table = match format.as_str() {
        "csv" => parse_csv_path(&path).map_err(|e| {
            eprintln!("{e}");
            1u8
        })?,
        "jsonl" => parse_jsonl_path(&path).map_err(|e| {
            eprintln!("{e}");
            1u8
        })?,
        "parquet" => {
            return fail(4, "analysis/limit: parquet not enabled in T0 (use csv/jsonl)");
        }
        other => return fail(2, &format!("analysis/invalid-args: bad format {other}")),
    };

    let max_rows: usize = env_u64("DSH_ANALYZE_MAX_ROWS", "INV_ANALYSIS_MAX_ROWS", 5_000_000) as usize;
    if table.rows.len() > max_rows {
        return fail(3, "analysis/limit: too many rows");
    }

    let id = dataset_id;
    if let Err(e) = crate::dataset::validate_dataset_id(&id) {
        return fail(2, &e);
    }
    let (bytes, _) = write_table_parquet_placeholder(workspace, &id, &table).map_err(|e| {
        eprintln!("{e}");
        1u8
    })?;
    let meta = Meta {
        dataset_id: id.clone(),
        row_count: table.rows.len(),
        columns: table.columns.clone(),
        source_kind: format,
        source_path: input,
        label,
        produced_by: "ingest".into(),
        parent_dataset_id: None,
    };
    save_meta(workspace, &meta).map_err(|e| {
        eprintln!("{e}");
        1u8
    })?;

    Ok(format!(
        "{{\"ok\":true,\"datasetId\":{},\"rowCount\":{},\"columnCount\":{},\"columns\":[{}],\"bytesOnDisk\":{}}}",
        json_string(&id),
        table.rows.len(),
        table.columns.len(),
        meta.columns_json(),
        bytes
    ))
}
