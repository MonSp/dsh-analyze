mod agg;
mod dataset;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use dataset::{
    analysis_dir, datasets_dir, ensure_within_workspace, json_string, load_dataset, new_dataset_id,
    outputs_dir, parse_csv_path, parse_jsonl_path, save_meta, write_table_parquet_placeholder, Meta,
    Value,
};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match run(&args) {
        Ok(line) => {
            println!("{line}");
            ExitCode::SUCCESS
        }
        Err(code) => {
            // message already printed to stderr by fail()
            ExitCode::from(code)
        }
    }
}

fn fail(code: u8, msg: &str) -> Result<String, u8> {
    eprintln!("{msg}");
    Err(code)
}

/// Read a u64 env var: prefer `DSH_ANALYZE_*`, fall back to legacy `INV_ANALYSIS_*`.
fn env_u64(key: &str, legacy_key: &str, default: u64) -> u64 {
    env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .or_else(|| env::var(legacy_key).ok().and_then(|s| s.parse().ok()))
        .unwrap_or(default)
}

fn run(args: &[String]) -> Result<String, u8> {
    let mut workspace: Option<PathBuf> = None;
    let mut pretty = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--workspace" => {
                i += 1;
                workspace = args.get(i).map(PathBuf::from);
            }
            "--pretty" => pretty = true,
            _ => break,
        }
        i += 1;
    }
    if i >= args.len() {
        return fail(2, "analysis/invalid-args: missing command");
    }
    let cmd = args[i].clone();
    let rest = &args[i + 1..];

    if cmd == "version" || cmd == "--version" {
        return Ok(
            "{\"ok\":true,\"version\":\"0.1.0\",\"capabilities\":[\"ingest\",\"head\",\"schema\",\"describe\",\"agg\",\"list\",\"export\",\"rm\"]}"
                .into(),
        );
    }

    let Some(workspace) = workspace else {
        return fail(2, "analysis/invalid-args: --workspace required");
    };
    if !workspace.exists() {
        return fail(2, "analysis/invalid-args: workspace missing");
    }
    let _ = pretty;

    match cmd.as_str() {
        "ingest" => cmd_ingest(&workspace, rest),
        "head" => cmd_head(&workspace, rest),
        "schema" => cmd_schema(&workspace, rest),
        "describe" => cmd_describe(&workspace, rest),
        "agg" => cmd_agg(&workspace, rest),
        "list" => cmd_list(&workspace),
        "export" => cmd_export(&workspace, rest),
        "rm" => cmd_rm(&workspace, rest),
        other => fail(2, &format!("analysis/invalid-args: unknown command {other}")),
    }
}

fn flag_value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == name {
            return args.get(i + 1).map(|s| s.as_str());
        }
        i += 1;
    }
    None
}

fn cmd_ingest(workspace: &Path, args: &[String]) -> Result<String, u8> {
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
    let max_bytes: u64 = env_u64("DSH_ANALYZE_MAX_INGEST_BYTES", "INV_ANALYSIS_MAX_INGEST_BYTES", 256 * 1024 * 1024);
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
    if let Err(e) = dataset::validate_dataset_id(&id) {
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

fn cmd_head(workspace: &Path, args: &[String]) -> Result<String, u8> {
    let id = match flag_value(args, "--dataset-id") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --dataset-id required"),
    };
    let n: usize = flag_value(args, "--n")
        .and_then(|s| s.parse().ok())
        .unwrap_or(5)
        .clamp(1, 20);
    let (meta, table) = load_dataset(workspace, &id).map_err(|e| {
        eprintln!("{e}");
        if e.contains("unknown") {
            5u8
        } else {
            2u8
        }
    })?;
    let cols: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();
    let rows_json = table
        .rows
        .iter()
        .take(n)
        .map(|row| {
            let cells = row
                .iter()
                .map(|v| v.to_json())
                .collect::<Vec<_>>()
                .join(",");
            format!("[{cells}]")
        })
        .collect::<Vec<_>>()
        .join(",");
    Ok(format!(
        "{{\"ok\":true,\"datasetId\":{},\"rowCount\":{},\"columns\":[{}],\"rows\":[{}]}}",
        json_string(&id),
        meta.row_count,
        cols.iter().map(|c| json_string(c)).collect::<Vec<_>>().join(","),
        rows_json
    ))
}

fn cmd_schema(workspace: &Path, args: &[String]) -> Result<String, u8> {
    let id = match flag_value(args, "--dataset-id") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --dataset-id required"),
    };
    let (meta, table) = load_dataset(workspace, &id).map_err(|e| {
        eprintln!("{e}");
        if e.contains("unknown") {
            5u8
        } else {
            2u8
        }
    })?;
    let _ = table;
    Ok(format!(
        "{{\"ok\":true,\"datasetId\":{},\"rowCount\":{},\"columns\":[{}]}}",
        json_string(&id),
        meta.row_count,
        meta.columns_json()
    ))
}

fn cmd_describe(workspace: &Path, args: &[String]) -> Result<String, u8> {
    let id = match flag_value(args, "--dataset-id") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --dataset-id required"),
    };
    let columns_filter = flag_value(args, "--columns")
        .map(|s| s.split(',').map(|x| x.trim().to_string()).collect::<Vec<_>>());
    let (_meta, table) = load_dataset(workspace, &id).map_err(|e| {
        eprintln!("{e}");
        if e.contains("unknown") {
            5u8
        } else {
            2u8
        }
    })?;
    let mut stats = Vec::new();
    for (i, col) in table.columns.iter().enumerate() {
        if let Some(filter) = &columns_filter {
            if !filter.iter().any(|c| c == &col.name) {
                continue;
            }
        }
        let mut nulls = 0usize;
        let mut nums: Vec<f64> = Vec::new();
        for row in &table.rows {
            let v = row.get(i).cloned().unwrap_or(Value::Null);
            match v {
                Value::Null => nulls += 1,
                other => {
                    if let Some(n) = other.as_f64() {
                        nums.push(n);
                    }
                }
            }
        }
        let mut min = Value::Null;
        let mut max = Value::Null;
        let mut mean = Value::Null;
        let mut std = Value::Null;
        let mut q25 = Value::Null;
        let mut q50 = Value::Null;
        let mut q75 = Value::Null;
        if !nums.is_empty() {
            let mut s = nums.clone();
            s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            min = Value::F64(s[0]);
            max = Value::F64(s[s.len() - 1]);
            mean = Value::F64(s.iter().sum::<f64>() / s.len() as f64);
            if s.len() > 1 {
                let m = mean.as_f64().unwrap();
                let var = s.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (s.len() - 1) as f64;
                std = Value::F64(var.sqrt());
            }
            let pct = |p: f64| -> f64 {
                let idx = ((s.len() as f64 - 1.0) * p).round() as usize;
                s[idx.min(s.len() - 1)]
            };
            q25 = Value::F64(pct(0.25));
            q50 = Value::F64(pct(0.50));
            q75 = Value::F64(pct(0.75));
        }
        stats.push(format!(
            "{{\"name\":{},\"dtype\":{},\"nullCount\":{},\"min\":{},\"max\":{},\"mean\":{},\"std\":{},\"q25\":{},\"q50\":{},\"q75\":{}}}",
            json_string(&col.name),
            json_string(&col.dtype),
            nulls,
            min.to_json(),
            max.to_json(),
            mean.to_json(),
            std.to_json(),
            q25.to_json(),
            q50.to_json(),
            q75.to_json()
        ));
    }
    Ok(format!(
        "{{\"ok\":true,\"datasetId\":{},\"stats\":[{}]}}",
        json_string(&id),
        stats.join(",")
    ))
}

fn cmd_agg(workspace: &Path, args: &[String]) -> Result<String, u8> {
    let id = match flag_value(args, "--dataset-id") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --dataset-id required"),
    };
    let spec_raw = match flag_value(args, "--spec") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --spec required"),
    };
    // Spec may be inline JSON object or a workspace-relative file path (interface spec §4).
    let spec_text = if spec_raw.trim_start().starts_with('{') {
        spec_raw
    } else {
        let spec_path = ensure_within_workspace(workspace, Path::new(&spec_raw)).map_err(|e| {
            eprintln!("{e}");
            2u8
        })?;
        fs::read_to_string(&spec_path).map_err(|e| {
            eprintln!("analysis/invalid-args: cannot read spec file: {e}");
            2u8
        })?
    };
    let out = flag_value(args, "--out").map(|s| s.to_string());
    let default_limit: usize = env_u64("DSH_ANALYZE_DEFAULT_ROW_LIMIT", "INV_ANALYSIS_DEFAULT_ROW_LIMIT", 1000) as usize;
    let (_meta, table) = load_dataset(workspace, &id).map_err(|e| {
        eprintln!("{e}");
        if e.contains("unknown") {
            5u8
        } else {
            2u8
        }
    })?;
    let spec = agg::parse_spec_json(&spec_text).map_err(|e| {
        eprintln!("{e}");
        2u8
    })?;
    let result = agg::run_agg(&table, &spec, default_limit, 5000).map_err(|e| {
        eprintln!("{e}");
        2u8
    })?;

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
        };
        save_meta(workspace, &meta).map_err(|e| {
            eprintln!("{e}");
            1u8
        })?;
        out_dataset_id = new_id;
    }

    let budget_rows = 20;
    let include_rows = out_dataset_id.is_empty() && result.rows.len() <= budget_rows;
    let truncated =
        !include_rows && (out_dataset_id.is_empty() || result.rows.len() > budget_rows);
    let cols: Vec<String> = result.columns.iter().map(|c| c.name.clone()).collect();
    let rows_json = if include_rows {
        result
            .rows
            .iter()
            .map(|row| {
                format!(
                    "[{}]",
                    row.iter().map(|v| v.to_json()).collect::<Vec<_>>().join(",")
                )
            })
            .collect::<Vec<_>>()
            .join(",")
    } else {
        String::new()
    };

    Ok(format!(
        "{{\"ok\":true,\"datasetId\":{},\"outDatasetId\":{},\"rowCount\":{},\"columns\":[{}],\"rows\":[{}],\"truncated\":{}}}",
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

fn cmd_list(workspace: &Path) -> Result<String, u8> {
    let items = dataset::list_dataset_summaries(workspace).map_err(|e| {
        eprintln!("{e}");
        1u8
    })?;
    Ok(format!(
        "{{\"ok\":true,\"datasets\":[{}]}}",
        items.join(",")
    ))
}

fn cmd_export(workspace: &Path, args: &[String]) -> Result<String, u8> {
    let id = match flag_value(args, "--dataset-id") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --dataset-id required"),
    };
    let output = match flag_value(args, "--output") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --output required"),
    };
    let (_meta, table) = load_dataset(workspace, &id).map_err(|e| {
        eprintln!("{e}");
        if e.contains("unknown") {
            5u8
        } else {
            2u8
        }
    })?;
    let out_path = ensure_within_workspace(workspace, Path::new(&output)).map_err(|e| {
        eprintln!("{e}");
        2u8
    })?;
    if let Some(parent) = out_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    // force under outputs/ for T0
    let outputs = outputs_dir(workspace);
    let _ = fs::create_dir_all(&outputs);
    let final_path = if out_path.starts_with(&outputs) {
        out_path
    } else {
        outputs.join(out_path.file_name().unwrap_or_default())
    };
    if !final_path.starts_with(&outputs) && !final_path.starts_with(workspace) {
        return fail(2, "analysis/invalid-path: export outside workspace");
    }
    let mut csv = String::new();
    csv.push_str(
        &table
            .columns
            .iter()
            .map(|c| c.name.clone())
            .collect::<Vec<_>>()
            .join(","),
    );
    csv.push('\n');
    for row in &table.rows {
        csv.push_str(
            &row
                .iter()
                .map(|v| match v {
                    Value::Null => String::new(),
                    Value::Str(s) => s.replace(',', "\\,"),
                    other => other.to_json(),
                })
                .collect::<Vec<_>>()
                .join(","),
        );
        csv.push('\n');
    }
    fs::write(&final_path, csv).map_err(|e| {
        eprintln!("analysis/cli-failed: {e}");
        1u8
    })?;
    let bytes = fs::metadata(&final_path).map(|m| m.len()).unwrap_or(0);
    let rel = final_path
        .strip_prefix(workspace)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| final_path.display().to_string());
    Ok(format!(
        "{{\"ok\":true,\"outputPath\":{},\"rowCount\":{},\"bytes\":{}}}",
        json_string(&rel),
        table.rows.len(),
        bytes
    ))
}

fn cmd_rm(workspace: &Path, args: &[String]) -> Result<String, u8> {
    let id = match flag_value(args, "--dataset-id") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --dataset-id required"),
    };
    if let Err(e) = dataset::validate_dataset_id(&id) {
        return fail(2, &e);
    }
    let dir = datasets_dir(workspace);
    let meta = dir.join(format!("{id}.meta.json"));
    let data = dir.join(format!("{id}.csv"));
    if !meta.exists() {
        return fail(5, "analysis/unknown-dataset");
    }
    let _ = fs::remove_file(&meta);
    let _ = fs::remove_file(&data);
    let _ = analysis_dir(workspace);
    Ok(format!(
        "{{\"ok\":true,\"datasetId\":{},\"removed\":true}}",
        json_string(&id)
    ))
}
