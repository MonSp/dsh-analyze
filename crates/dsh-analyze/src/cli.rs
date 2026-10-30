use std::env;

/// Print message to stderr and return exit code.
pub(crate) fn fail(code: u8, msg: &str) -> Result<String, u8> {
    eprintln!("{msg}");
    Err(code)
}

/// Read a u64 env var: prefer `DSH_ANALYZE_*`, fall back to legacy `INV_ANALYSIS_*`.
pub(crate) fn env_u64(key: &str, legacy_key: &str, default: u64) -> u64 {
    env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .or_else(|| env::var(legacy_key).ok().and_then(|s| s.parse().ok()))
        .unwrap_or(default)
}

pub(crate) fn flag_value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == name {
            return args.get(i + 1).map(|s| s.as_str());
        }
        i += 1;
    }
    None
}
