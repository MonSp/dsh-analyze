mod agg;
mod dataset;
mod expr;
mod time;

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use dataset::{
    analysis_dir, datasets_dir, ensure_within_workspace, json_string, load_dataset, new_dataset_id,
    outputs_dir, parse_csv_path, parse_jsonl_path, save_meta, write_table_parquet_placeholder,
    Column, Meta, Table, Value,
};

const CAPABILITIES: &str = "\"ingest\",\"head\",\"schema\",\"describe\",\"agg\",\"list\",\"export\",\"rm\",\"values\",\"sample\",\"join\"";
const VERSION: &str = "0.1.2";

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
        return Ok(format!(
            "{{\"ok\":true,\"version\":\"{VERSION}\",\"capabilities\":[{CAPABILITIES}]}}"
        ));
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
        "values" => cmd_values(&workspace, rest),
        "sample" => cmd_sample(&workspace, rest),
        "join" => cmd_join(&workspace, rest),
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
    // Windows tools may write a UTF-8 BOM; trim() does not strip U+FEFF.
    let spec_text = spec_text.strip_prefix('\u{feff}').unwrap_or(&spec_text);
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
    let outcome = agg::run_agg(&table, &spec, default_limit, 5000).map_err(|e| {
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

    let parse_field = if outcome.parse_fail_count > 0 {
        format!(",\"parseFailCount\":{}", outcome.parse_fail_count)
    } else {
        String::new()
    };

    Ok(format!(
        "{{\"ok\":true,\"datasetId\":{},\"outDatasetId\":{},\"rowCount\":{},\"columns\":[{}],\"rows\":[{}],\"truncated\":{}{parse_field}}}",
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

fn cmd_values(workspace: &Path, args: &[String]) -> Result<String, u8> {
    let id = match flag_value(args, "--dataset-id") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --dataset-id required"),
    };
    let column = match flag_value(args, "--column") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --column required"),
    };
    let limit: usize = flag_value(args, "--limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(20)
        .clamp(1, 100);
    let (meta, table) = load_dataset(workspace, &id).map_err(|e| {
        eprintln!("{e}");
        if e.contains("unknown") {
            5u8
        } else {
            2u8
        }
    })?;
    let Some(ci) = table.columns.iter().position(|c| c.name == column) else {
        return fail(2, &format!("analysis/invalid-args: unknown column {column}"));
    };
    let dtype = table.columns[ci].dtype.clone();
    let mut null_count = 0usize;
    let mut counts: HashMap<Value, usize> = HashMap::new();
    for row in &table.rows {
        let v = row.get(ci).cloned().unwrap_or(Value::Null);
        if matches!(v, Value::Null) {
            null_count += 1;
        } else {
            *counts.entry(v).or_insert(0) += 1;
        }
    }
    let nunique = counts.len();
    let non_null = (meta.row_count.max(table.rows.len())).saturating_sub(null_count).max(1);
    let mut top: Vec<(Value, usize)> = counts.into_iter().collect();
    top.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.to_json().cmp(&b.0.to_json()))
    });
    top.truncate(limit);
    let top_json = top
        .iter()
        .map(|(v, c)| {
            format!(
                "{{\"value\":{},\"count\":{},\"ratio\":{}}}",
                v.to_json(),
                c,
                format!("{:.6}", (*c as f64) / (non_null as f64))
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    Ok(format!(
        "{{\"ok\":true,\"datasetId\":{},\"column\":{},\"dtype\":{},\"rowCount\":{},\"nullCount\":{},\"nunique\":{},\"limit\":{},\"top\":[{}]}}",
        json_string(&id),
        json_string(&column),
        json_string(&dtype),
        meta.row_count.max(table.rows.len()),
        null_count,
        nunique,
        limit,
        top_json
    ))
}

/// SplitMix64 deterministic PRNG for sampling.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        (self.next_u64() % n as u64) as usize
    }
}

fn cmd_sample(workspace: &Path, args: &[String]) -> Result<String, u8> {
    let id = match flag_value(args, "--dataset-id") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --dataset-id required"),
    };
    let n: usize = flag_value(args, "--n")
        .and_then(|s| s.parse().ok())
        .unwrap_or(10)
        .clamp(1, 20);
    let default_seed: u64 = env_u64("DSH_ANALYZE_DEFAULT_SAMPLE_SEED", "INV_ANALYSIS_DEFAULT_SAMPLE_SEED", 42);
    let seed: u64 = flag_value(args, "--seed")
        .and_then(|s| s.parse().ok())
        .unwrap_or(default_seed);
    let stratify = flag_value(args, "--stratify-by").map(|s| s.to_string());
    let (meta, table) = load_dataset(workspace, &id).map_err(|e| {
        eprintln!("{e}");
        if e.contains("unknown") {
            5u8
        } else {
            2u8
        }
    })?;
    let pop = table.rows.len();
    let indices: Vec<usize> = if let Some(col_name) = &stratify {
        let Some(si) = table.columns.iter().position(|c| &c.name == col_name) else {
            return fail(2, &format!("analysis/invalid-args: unknown column {col_name}"));
        };
        stratified_indices(&table, si, n, seed)
    } else {
        random_indices(pop, n, seed)
    };
    let cols: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();
    let rows_json = indices
        .iter()
        .map(|&i| {
            format!(
                "[{}]",
                table.rows[i]
                    .iter()
                    .map(|v| v.to_json())
                    .collect::<Vec<_>>()
                    .join(",")
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let method = if stratify.is_some() { "stratified" } else { "random" };
    let stratify_json = stratify
        .as_deref()
        .map(json_string)
        .unwrap_or_else(|| "null".into());
    Ok(format!(
        "{{\"ok\":true,\"datasetId\":{},\"rowCount\":{},\"columns\":[{}],\"rows\":[{}],\"sampling\":{{\"method\":\"{method}\",\"n\":{},\"seed\":{seed},\"stratifyBy\":{stratify_json},\"population\":{pop}}}}}",
        json_string(&id),
        meta.row_count.max(pop),
        cols.iter().map(|c| json_string(c)).collect::<Vec<_>>().join(","),
        rows_json,
        indices.len()
    ))
}

fn random_indices(pop: usize, n: usize, seed: u64) -> Vec<usize> {
    if n >= pop {
        return (0..pop).collect();
    }
    // Fisher-Yates partial shuffle
    let mut idx: Vec<usize> = (0..pop).collect();
    let mut rng = Rng::new(seed);
    for i in 0..n {
        let j = i + rng.below(pop - i);
        idx.swap(i, j);
    }
    idx.truncate(n);
    idx.sort_unstable();
    idx
}

fn stratified_indices(table: &Table, strata_col: usize, n: usize, seed: u64) -> Vec<usize> {
    let pop = table.rows.len();
    if n >= pop {
        return (0..pop).collect();
    }
    let mut groups: HashMap<Value, Vec<usize>> = HashMap::new();
    for (i, row) in table.rows.iter().enumerate() {
        let key = row.get(strata_col).cloned().unwrap_or(Value::Null);
        groups.entry(key).or_default().push(i);
    }
    // largest-remainder allocation
    let mut quotas: Vec<(Vec<usize>, usize, f64)> = groups
        .into_values()
        .map(|members| {
            let len = members.len();
            let share = (len as f64) * (n as f64) / (pop as f64);
            let floor = share.floor() as usize;
            (members, floor.min(len), share)
        })
        .collect();
    let mut assigned: usize = quotas.iter().map(|q| q.1).sum();
    // sort by fractional remainder desc for remainder distribution
    quotas.sort_by(|a, b| {
        let fa = a.2 - a.1 as f64;
        let fb = b.2 - b.1 as f64;
        fb.partial_cmp(&fa).unwrap_or(std::cmp::Ordering::Equal)
    });
    for q in quotas.iter_mut() {
        if assigned >= n {
            break;
        }
        if q.1 < q.0.len() {
            q.1 += 1;
            assigned += 1;
        }
    }
    // sample within each group with a stable derived seed
    let mut out = Vec::with_capacity(n);
    let mut group_seed = seed;
    for (members, take, _) in quotas {
        group_seed = group_seed
            .wrapping_mul(0x9e37_79b9_7f4a_7c15)
            .wrapping_add(members.len() as u64);
        let picked = random_indices(members.len(), take, group_seed);
        for p in picked {
            out.push(members[p]);
        }
    }
    out.sort_unstable();
    out
}

fn cmd_join(workspace: &Path, args: &[String]) -> Result<String, u8> {
    let left_id = match flag_value(args, "--left") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --left required"),
    };
    let right_id = match flag_value(args, "--right") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --right required"),
    };
    let on_raw = match flag_value(args, "--on") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --on required"),
    };
    let on: Vec<String> = on_raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if on.is_empty() {
        return fail(2, "analysis/invalid-args: --on must list at least one column");
    }
    let how = flag_value(args, "--how").map(|s| s.to_string()).unwrap_or_else(|| "inner".into());
    if how != "inner" && how != "left" {
        return fail(2, "analysis/invalid-args: --how must be inner|left");
    }
    let label = flag_value(args, "--out").map(|s| s.to_string());
    let max_rows: usize =
        env_u64("DSH_ANALYZE_MAX_ROWS", "INV_ANALYSIS_MAX_ROWS", 5_000_000) as usize;

    let (left_meta, left) = load_dataset(workspace, &left_id).map_err(|e| {
        eprintln!("{e}");
        if e.contains("unknown") {
            5u8
        } else {
            2u8
        }
    })?;
    let (_right_meta, right) = load_dataset(workspace, &right_id).map_err(|e| {
        eprintln!("{e}");
        if e.contains("unknown") {
            5u8
        } else {
            2u8
        }
    })?;
    let _ = left_meta;

    let mut left_key_idx = Vec::with_capacity(on.len());
    let mut right_key_idx = Vec::with_capacity(on.len());
    for col in &on {
        let li = left.columns.iter().position(|c| &c.name == col).ok_or_else(|| {
            eprintln!("analysis/invalid-args: unknown column {col} on left");
            2u8
        })?;
        let ri = right
            .columns
            .iter()
            .position(|c| &c.name == col)
            .ok_or_else(|| {
                eprintln!("analysis/invalid-args: unknown column {col} on right");
                2u8
            })?;
        left_key_idx.push(li);
        right_key_idx.push(ri);
    }

    // Column layout: left cols + right non-key cols (collision → _r)
    let mut out_cols: Vec<Column> = left.columns.clone();
    let mut names: Vec<String> = out_cols.iter().map(|c| c.name.clone()).collect();
    let mut right_payload: Vec<usize> = Vec::new();
    for (i, c) in right.columns.iter().enumerate() {
        if right_key_idx.contains(&i) {
            continue;
        }
        let mut name = c.name.clone();
        while names.contains(&name) {
            name = format!("{name}_r");
        }
        names.push(name.clone());
        out_cols.push(Column {
            name,
            dtype: c.dtype.clone(),
        });
        right_payload.push(i);
    }

    // Hash right by key
    let mut right_map: HashMap<Vec<Value>, Vec<usize>> = HashMap::new();
    for (ri, row) in right.rows.iter().enumerate() {
        let key: Vec<Value> = right_key_idx
            .iter()
            .map(|&ci| row.get(ci).cloned().unwrap_or(Value::Null))
            .collect();
        if key.iter().any(|v| matches!(v, Value::Null)) {
            continue;
        }
        right_map.entry(key).or_default().push(ri);
    }

    let mut out_rows: Vec<Vec<Value>> = Vec::new();
    for lrow in &left.rows {
        let key: Vec<Value> = left_key_idx
            .iter()
            .map(|&ci| lrow.get(ci).cloned().unwrap_or(Value::Null))
            .collect();
        let matched = if key.iter().any(|v| matches!(v, Value::Null)) {
            None
        } else {
            right_map.get(&key)
        };
        match matched {
            Some(ris) => {
                for &ri in ris {
                    let mut row = lrow.clone();
                    for &ci in &right_payload {
                        row.push(right.rows[ri].get(ci).cloned().unwrap_or(Value::Null));
                    }
                    out_rows.push(row);
                    if out_rows.len() > max_rows {
                        return fail(3, "analysis/limit: join result too large");
                    }
                }
            }
            None if how == "left" => {
                let mut row = lrow.clone();
                for _ in &right_payload {
                    row.push(Value::Null);
                }
                out_rows.push(row);
                if out_rows.len() > max_rows {
                    return fail(3, "analysis/limit: join result too large");
                }
            }
            None => {}
        }
    }

    let new_id = new_dataset_id();
    let result = Table {
        columns: out_cols,
        rows: out_rows,
    };
    let (bytes, _) = write_table_parquet_placeholder(workspace, &new_id, &result).map_err(|e| {
        eprintln!("{e}");
        1u8
    })?;
    let _ = bytes;
    let meta = Meta {
        dataset_id: new_id.clone(),
        row_count: result.rows.len(),
        columns: result.columns.clone(),
        source_kind: "join".into(),
        source_path: format!("join:{left_id}+{right_id}"),
        label,
        produced_by: "join".into(),
        parent_dataset_id: Some(left_id.clone()),
    };
    save_meta(workspace, &meta).map_err(|e| {
        eprintln!("{e}");
        1u8
    })?;
    Ok(format!(
        "{{\"ok\":true,\"datasetId\":{},\"rowCount\":{},\"columnCount\":{},\"columns\":[{}],\"leftDatasetId\":{},\"rightDatasetId\":{},\"how\":{}}}",
        json_string(&new_id),
        result.rows.len(),
        result.columns.len(),
        meta.columns_json(),
        json_string(&left_id),
        json_string(&right_id),
        json_string(&how)
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
