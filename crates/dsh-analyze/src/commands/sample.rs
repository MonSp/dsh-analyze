use std::collections::HashMap;
use std::path::Path;

use crate::cli::{env_u64, fail, flag_value};
use crate::dataset::{json_string, load_dataset, Table, Value};

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

pub(crate) fn cmd_sample(workspace: &Path, args: &[String]) -> Result<String, u8> {
    let id = match flag_value(args, "--dataset-id") {
        Some(v) => v.to_string(),
        None => return fail(2, "analysis/invalid-args: --dataset-id required"),
    };
    let n: usize = flag_value(args, "--n")
        .and_then(|s| s.parse().ok())
        .unwrap_or(10)
        .clamp(1, 20);
    let default_seed: u64 = env_u64(
        "DSH_ANALYZE_DEFAULT_SAMPLE_SEED",
        "INV_ANALYSIS_DEFAULT_SAMPLE_SEED",
        42,
    );
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
