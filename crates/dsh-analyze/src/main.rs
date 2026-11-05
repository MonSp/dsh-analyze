mod agg;
mod cli;
mod commands;
mod dataset;
mod expr;
mod time;

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use cli::fail;

const CAPABILITIES: &str = "\"ingest\",\"head\",\"schema\",\"describe\",\"agg\",\"list\",\"export\",\"rm\",\"values\",\"sample\",\"join\",\"profile\",\"dedup\",\"outliers\",\"corr\",\"crosstab\",\"series\",\"union\",\"diff\",\"pivot\"";
const VERSION: &str = "0.1.11";

const USAGE: &str = "\
dsh-analyze — workspace-confined dataset analysis (stdout: one JSON line per run)

Usage: dsh-analyze [--workspace <dir>] [--pretty] <command> [flags]

Commands:
  ingest    --input <path> [--format csv|json|jsonl] [--encoding utf-8|gbk|gb18030|auto] [--dataset-id <id>] [--label <text>]
  head      --dataset-id <id> [--n <N>]                       (default 5, max 100)
  schema    --dataset-id <id>
  describe  --dataset-id <id> [--columns a,b]
  values    --dataset-id <id> --column <c> [--limit <N>]      (default 20, max 100)
  sample    --dataset-id <id> [--n <N>] [--seed <N>] [--stratify-by <c>]   (default 10, max 200)
  join      --left <id> --right <id> --on key[,key]…          (key := name | leftName:rightName)
  agg       --dataset-id <id> --spec <json|path> [--out <label>]
  profile   --dataset-id <id>
  dedup     --dataset-id <id> [--key-columns a,b] [--sample-limit <N>]
  outliers  --dataset-id <id> --columns a,b [--method iqr|zscore] [--out <label>]
  corr      --dataset-id <id> --columns a,b,…                (pairs report n and df)
  crosstab  --dataset-id <id> --rows <c> --cols <c> [--values <c>] [--agg sum|mean|min|max|count]
  series    --dataset-id <id> --order-by <c> --ops <json|path> [--out <label>]
  union     --left <id> --right <id> [--columns-policy strict|intersect] [--out <label>]
  diff      --left <id> --right <id> [--on k1[:k2],…] [--sample-limit <N>]
  pivot     --dataset-id <id> --index <col,…> --columns <col> --values <col> --agg sum|mean|min|max|count [--out <label>]
  list      [--label-like <substr>] [--produced-by <name>] [--since <YYYY-MM-DD[THH:MM:SS]>]
  export    --dataset-id <id> --output <path> [--format csv|xlsx|parquet]
  rm        --dataset-id <id>
  version

Key flags:
  --ops accepts a JSON array OR a workspace path; --spec likewise (a JSON object OR a workspace path).
  --format csv|json|jsonl|parquet (ingest); export --format csv|xlsx|parquet (default csv).
  --encoding utf-8|gbk|gb18030|auto (ingest csv only; default utf-8).
  xlsx ingest still reports analysis/limit (convert to csv/jsonl).
  Errors go to stderr as analysis/* messages; exit codes: 0 ok, 1 cli-failed, 2 invalid args/path,
  3 limit, 4 unsupported format limit, 5 unknown dataset.
";

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
        return Ok(USAGE.to_string());
    }
    let cmd = args[i].clone();
    let rest = &args[i + 1..];

    if cmd == "version" || cmd == "--version" {
        return Ok(format!(
            "{{\"ok\":true,\"version\":\"{VERSION}\",\"capabilities\":[{CAPABILITIES}]}}"
        ));
    }
    if cmd == "--help" || cmd == "-h" || cmd == "help" {
        return Ok(USAGE.to_string());
    }

    let Some(workspace) = workspace else {
        return fail(2, "analysis/invalid-args: --workspace required");
    };
    if !workspace.exists() {
        return fail(2, "analysis/invalid-args: workspace missing");
    }
    let _ = pretty;

    match cmd.as_str() {
        "ingest" => commands::cmd_ingest(&workspace, rest),
        "head" => commands::cmd_head(&workspace, rest),
        "schema" => commands::cmd_schema(&workspace, rest),
        "describe" => commands::cmd_describe(&workspace, rest),
        "values" => commands::cmd_values(&workspace, rest),
        "sample" => commands::cmd_sample(&workspace, rest),
        "join" => commands::cmd_join(&workspace, rest),
        "agg" => commands::cmd_agg(&workspace, rest),
        "profile" => commands::cmd_profile(&workspace, rest),
        "dedup" => commands::cmd_dedup(&workspace, rest),
        "outliers" => commands::cmd_outliers(&workspace, rest),
        "corr" => commands::cmd_corr(&workspace, rest),
        "crosstab" => commands::cmd_crosstab(&workspace, rest),
        "series" => commands::cmd_series(&workspace, rest),
        "union" => commands::cmd_union(&workspace, rest),
        "diff" => commands::cmd_diff(&workspace, rest),
        "pivot" => commands::cmd_pivot(&workspace, rest),
        "list" => commands::cmd_list(&workspace, rest),
        "export" => commands::cmd_export(&workspace, rest),
        "rm" => commands::cmd_rm(&workspace, rest),
        other => fail(2, &format!("analysis/invalid-args: unknown command {other}")),
    }
}
