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

const CAPABILITIES: &str = "\"ingest\",\"head\",\"schema\",\"describe\",\"agg\",\"list\",\"export\",\"rm\",\"values\",\"sample\",\"join\",\"profile\",\"dedup\",\"outliers\",\"corr\",\"crosstab\",\"series\"";
const VERSION: &str = "0.1.5";

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
        "list" => commands::cmd_list(&workspace),
        "export" => commands::cmd_export(&workspace, rest),
        "rm" => commands::cmd_rm(&workspace, rest),
        other => fail(2, &format!("analysis/invalid-args: unknown command {other}")),
    }
}
