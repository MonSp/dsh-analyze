use std::fs;
use std::path::Path;

use crate::cli::{env_u64, fail, flag_value};
use crate::dataset::{
    decode_csv_bytes, ensure_within_workspace, json_string, new_dataset_id, parse_csv_bytes,
    parse_json_path, parse_jsonl_path, save_meta, write_table_parquet_placeholder, Meta,
};

pub(crate) fn cmd_ingest(workspace: &Path, args: &[String]) -> Result<String, u8> {
    let input = match flag_value(args, "--input") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --input required"),
    };
    let format = flag_value(args, "--format")
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_else(|| {
            let lower = input.to_ascii_lowercase();
            if lower.ends_with(".parquet") {
                "parquet".into()
            } else if lower.ends_with(".xlsm") {
                "xlsm".into()
            } else if lower.ends_with(".xlsx") {
                "xlsx".into()
            } else if lower.ends_with(".jsonl") {
                "jsonl".into()
            } else if lower.ends_with(".json") {
                "json".into()
            } else {
                "csv".into()
            }
        });
    let sheet = flag_value(args, "--sheet").map(|s| s.to_string());
    if sheet.is_some() && format != "xlsx" && format != "xlsm" {
        return fail(2, "analysis/invalid-args: --sheet only applies to xlsx");
    }
    let encoding = flag_value(args, "--encoding").map(|s| s.to_ascii_lowercase());
    if let Some(enc) = &encoding {
        if !matches!(enc.as_str(), "utf-8" | "gbk" | "gb18030" | "auto") {
            return fail(2, &format!("analysis/invalid-args: bad encoding {enc}"));
        }
        if format != "csv" {
            return fail(2, "analysis/invalid-args: --encoding only applies to csv");
        }
    }
    let encoding = encoding.unwrap_or_else(|| "utf-8".to_string());
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
        eprintln!(
            "analysis/cli-failed: cannot read file: {}",
            crate::dataset::io_read_reason(&e)
        );
        1u8
    })?;
    if meta_fs.len() > max_bytes {
        return fail(3, "analysis/limit: input too large");
    }

    let mut encoding_fallback = false;
    let lower_input = input.to_ascii_lowercase();
    let mut xlsx_sheet_skipped: usize = 0;
    let mut xlsx_sheet_names: Vec<String> = Vec::new();
    let table = match format.as_str() {
        "csv" => {
            // Whole-file read: metadata cap first (above), then re-check the
            // actual byte count; decode per --encoding before line parsing.
            let bytes = fs::read(&path).map_err(|e| {
                eprintln!(
                    "analysis/cli-failed: cannot read file: {}",
                    crate::dataset::io_read_reason(&e)
                );
                1u8
            })?;
            if bytes.len() as u64 > max_bytes {
                return fail(3, "analysis/limit: input too large");
            }
            let (decoded, fallback) = decode_csv_bytes(&bytes, &encoding).map_err(|e| {
                eprintln!("{e}");
                2u8
            })?;
            encoding_fallback = fallback;
            parse_csv_bytes(decoded.as_bytes()).map_err(|e| {
                eprintln!("{e}");
                1u8
            })?
        }
        "json" => parse_json_path(&path).map_err(|e| {
            eprintln!("{e}");
            if e.starts_with("analysis/invalid-args") {
                2u8
            } else {
                1u8
            }
        })?,
        "jsonl" => parse_jsonl_path(&path).map_err(|e| {
            eprintln!("{e}");
            1u8
        })?,
        "parquet" => crate::dataset::parse_parquet_path(&path).map_err(|e| {
            eprintln!("{e}");
            if e.starts_with("analysis/invalid-args") {
                2u8
            } else {
                1u8
            }
        })?,
        "xlsm" => {
            return fail(4, "analysis/limit: .xlsm not supported (save as .xlsx)");
        }
        "xlsx" => {
            if lower_input.ends_with(".xlsm") {
                return fail(4, "analysis/limit: .xlsm not supported (save as .xlsx)");
            }
            let (tbl, sheet_names) =
                crate::dataset::parse_xlsx_path(&path, sheet.as_deref()).map_err(|e| {
                    eprintln!("{e}");
                    if e.starts_with("analysis/limit") {
                        4u8
                    } else {
                        2u8
                    }
                })?;
            if sheet_names.len() > 1 {
                xlsx_sheet_skipped = sheet_names.len() - 1;
                xlsx_sheet_names = sheet_names;
            }
            tbl
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
    // R-33: hash the source file bytes (already size-capped above).
    let source_checksum = fs::read(&path)
        .map(|b| Some(crate::dataset::sha256_hex(&b)))
        .unwrap_or(None);
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
        parent_dataset_ids: Vec::new(),
        source_checksum,
    };
    save_meta(workspace, &meta).map_err(|e| {
        eprintln!("{e}");
        1u8
    })?;

    let mut warn_parts: Vec<String> = Vec::new();
    if encoding_fallback {
        warn_parts.push(
            "{\"code\":\"encoding-fallback\",\"encoding\":\"gb18030\"}".to_string(),
        );
    }
    if xlsx_sheet_skipped > 0 {
        let names: Vec<String> = xlsx_sheet_names.iter().map(|n| json_string(n)).collect();
        warn_parts.push(format!(
            "{{\"code\":\"sheet-skipped\",\"skipped\":{},\"sheets\":[{}]}}",
            xlsx_sheet_skipped,
            names.join(",")
        ));
    }
    let warnings = if warn_parts.is_empty() {
        String::new()
    } else {
        format!(",\"warnings\":[{}]", warn_parts.join(","))
    };
    Ok(format!(
        "{{\"ok\":true,\"datasetId\":{},\"rowCount\":{},\"columnCount\":{},\"columns\":[{}],\"bytesOnDisk\":{}{}}}",
        json_string(&id),
        table.rows.len(),
        table.columns.len(),
        meta.columns_json(),
        bytes,
        warnings
    ))
}
