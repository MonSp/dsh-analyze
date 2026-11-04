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

/// A workspace path candidate: no whitespace and no filename-illegal characters
/// (after an optional `C:\` drive prefix). `a:b:c`, `diff:amount:d`, `not json`
/// are therefore NOT paths and get stable "must be JSON … or a workspace path"
/// errors instead of leaked OS messages.
pub(crate) fn looks_like_workspace_path(s: &str) -> bool {
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
