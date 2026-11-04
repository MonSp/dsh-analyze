//! Whitelist arithmetic expressions for derived columns (mutate).
//! Grammar: numbers, string literals, identifiers, + - * /, parentheses,
//! unary minus, and the whitelist calls to_int/to_str/to_date.
//! No arbitrary function calls or assignment.

use crate::dataset::{Table, Value};
use crate::time::{parse_with_fmt, validate_date_fmt};

#[derive(Debug, Clone)]
pub enum Expr {
    Num(f64),
    Str(String),
    Ident(String),
    Neg(Box<Expr>),
    Add(Box<Expr>, Box<Expr>),
    Sub(Box<Expr>, Box<Expr>),
    Mul(Box<Expr>, Box<Expr>),
    Div(Box<Expr>, Box<Expr>),
    ToInt(Box<Expr>),
    ToStr(Box<Expr>),
    ToDate(Box<Expr>, String),
}

/// Static expression type used for pre-eval mutate type checking (R-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExprType {
    Number,
    String,
    Boolean,
    Date,
}

impl ExprType {
    pub fn name(self) -> &'static str {
        match self {
            ExprType::Number => "number",
            ExprType::String => "string",
            ExprType::Boolean => "boolean",
            ExprType::Date => "date",
        }
    }

    pub fn from_dtype(dtype: &str) -> ExprType {
        match dtype {
            "i64" | "f64" => ExprType::Number,
            "bool" => ExprType::Boolean,
            "date" => ExprType::Date,
            _ => ExprType::String,
        }
    }
}

/// Runtime evaluation counters feeding the agg `warnings[]` entries.
/// Counting is per output row: a row counts at most once per code, and only
/// runtime failures count — Null propagation does not (S2.5).
#[derive(Debug, Default, Clone, Copy)]
pub struct EvalCounters {
    /// Rows whose output cell is Null due to an arithmetic runtime failure
    /// (divide-by-zero). Null propagation does not count (R-20 / S2.5).
    pub arith_null: u64,
    /// Rows whose output cell is Null due to a failed conversion of a
    /// non-null value. `to_int(null)` is propagation and does not count.
    pub cast_null: u64,
    /// Per-row scratch: arithmetic runtime failure seen while evaluating the
    /// current row.
    row_arith_fail: bool,
    /// Per-row scratch: non-null cast failed while evaluating the current row.
    row_cast_fail: bool,
}

impl EvalCounters {
    /// Start collecting for one output row.
    pub fn begin_row(&mut self) {
        self.row_arith_fail = false;
        self.row_cast_fail = false;
    }

    /// Roll the row's failures into the totals (a row counts once per code).
    pub fn end_row(&mut self) {
        if self.row_arith_fail {
            self.arith_null += 1;
        }
        if self.row_cast_fail {
            self.cast_null += 1;
        }
    }

    fn note_arith_fail(&mut self) {
        self.row_arith_fail = true;
    }

    fn note_cast_fail(&mut self) {
        self.row_cast_fail = true;
    }
}

pub fn validate_as_name(name: &str) -> bool {
    let b = name.as_bytes();
    if b.is_empty() || b.len() > 64 {
        return false;
    }
    let first = b[0];
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return false;
    }
    b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

pub fn parse_expr(input: &str) -> Result<Expr, String> {
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0usize;
    let e = parse_add(&chars, &mut i)?;
    skip_ws(&chars, &mut i);
    if i < chars.len() {
        return Err(format!(
            "analysis/invalid-args: unexpected token in expr at position {i}"
        ));
    }
    Ok(e)
}

fn skip_ws(chars: &[char], i: &mut usize) {
    while *i < chars.len() && chars[*i].is_whitespace() {
        *i += 1;
    }
}

fn peek(chars: &[char], i: &usize) -> Option<char> {
    chars.get(*i).copied()
}

fn parse_add(chars: &[char], i: &mut usize) -> Result<Expr, String> {
    let mut left = parse_mul(chars, i)?;
    loop {
        skip_ws(chars, i);
        match peek(chars, i) {
            Some('+') => {
                *i += 1;
                let r = parse_mul(chars, i)?;
                left = Expr::Add(Box::new(left), Box::new(r));
            }
            Some('-') => {
                *i += 1;
                let r = parse_mul(chars, i)?;
                left = Expr::Sub(Box::new(left), Box::new(r));
            }
            _ => return Ok(left),
        }
    }
}

fn parse_mul(chars: &[char], i: &mut usize) -> Result<Expr, String> {
    let mut left = parse_unary(chars, i)?;
    loop {
        skip_ws(chars, i);
        match peek(chars, i) {
            Some('*') => {
                *i += 1;
                let r = parse_unary(chars, i)?;
                left = Expr::Mul(Box::new(left), Box::new(r));
            }
            Some('/') => {
                *i += 1;
                let r = parse_unary(chars, i)?;
                left = Expr::Div(Box::new(left), Box::new(r));
            }
            _ => return Ok(left),
        }
    }
}

fn parse_unary(chars: &[char], i: &mut usize) -> Result<Expr, String> {
    skip_ws(chars, i);
    if peek(chars, i) == Some('-') {
        *i += 1;
        let inner = parse_unary(chars, i)?;
        return Ok(Expr::Neg(Box::new(inner)));
    }
    parse_primary(chars, i)
}

fn parse_string_lit(chars: &[char], i: &mut usize) -> Result<String, String> {
    // cursor at opening quote
    *i += 1;
    let mut out = String::new();
    while let Some(c) = peek(chars, i) {
        *i += 1;
        match c {
            '"' => return Ok(out),
            '\\' => {
                let n = peek(chars, i).ok_or("analysis/invalid-args: bad escape in expr")?;
                *i += 1;
                out.push(match n {
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    other => other,
                });
            }
            other => out.push(other),
        }
    }
    Err("analysis/invalid-args: unterminated string in expr".into())
}

fn parse_primary(chars: &[char], i: &mut usize) -> Result<Expr, String> {
    skip_ws(chars, i);
    match peek(chars, i) {
        Some('(') => {
            *i += 1;
            let e = parse_add(chars, i)?;
            skip_ws(chars, i);
            if peek(chars, i) != Some(')') {
                return Err("analysis/invalid-args: expected ')' in expr".into());
            }
            *i += 1;
            Ok(e)
        }
        Some('"') => Ok(Expr::Str(parse_string_lit(chars, i)?)),
        Some(c) if c.is_ascii_digit() || c == '.' => {
            let start = *i;
            while let Some(c) = peek(chars, i) {
                if c.is_ascii_digit() || c == '.' {
                    *i += 1;
                } else {
                    break;
                }
            }
            let tok: String = chars[start..*i].iter().collect();
            tok.parse::<f64>()
                .map(Expr::Num)
                .map_err(|_| format!("analysis/invalid-args: bad number literal {tok}"))
        }
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {
            let start = *i;
            while let Some(c) = peek(chars, i) {
                if c.is_ascii_alphanumeric() || c == '_' {
                    *i += 1;
                } else {
                    break;
                }
            }
            let name: String = chars[start..*i].iter().collect();
            skip_ws(chars, i);
            if peek(chars, i) != Some('(') {
                return Ok(Expr::Ident(name));
            }
            parse_call(name, chars, i)
        }
        Some(c) => Err(format!(
            "analysis/invalid-args: illegal character '{c}' in expr"
        )),
        None => Err("analysis/invalid-args: unexpected end of expr".into()),
    }
}

fn parse_call(name: String, chars: &[char], i: &mut usize) -> Result<Expr, String> {
    match name.as_str() {
        "to_int" | "to_str" | "to_date" => {}
        other => {
            return Err(format!(
                "analysis/invalid-args: unknown function {other}; expected to_int|to_str|to_date"
            ))
        }
    }
    *i += 1; // consume '('
    skip_ws(chars, i);
    let arg = parse_add(chars, i)?;
    skip_ws(chars, i);
    let mut fmt = None;
    if peek(chars, i) == Some(',') {
        *i += 1;
        skip_ws(chars, i);
        if peek(chars, i) != Some('"') {
            return Err("analysis/invalid-args: to_date format must be a string literal".into());
        }
        fmt = Some(parse_string_lit(chars, i)?);
        skip_ws(chars, i);
    }
    if peek(chars, i) != Some(')') {
        return Err(format!("analysis/invalid-args: expected ')' after {name}"));
    }
    *i += 1;
    match name.as_str() {
        "to_int" => {
            if fmt.is_some() {
                return Err("analysis/invalid-args: to_int takes one argument".into());
            }
            Ok(Expr::ToInt(Box::new(arg)))
        }
        "to_str" => {
            if fmt.is_some() {
                return Err("analysis/invalid-args: to_str takes one argument".into());
            }
            Ok(Expr::ToStr(Box::new(arg)))
        }
        _ => {
            let fmt = fmt.ok_or("analysis/invalid-args: to_date requires a format")?;
            validate_date_fmt(&fmt)?;
            Ok(Expr::ToDate(Box::new(arg), fmt))
        }
    }
}

/// Statically type-check `e` against column dtypes (R-1). Returns the
/// expression result type; arithmetic on non-numeric operands is a type error.
/// `as_name` is only used for the `mutate {as}: …` error prefix.
pub fn check_types(
    e: &Expr,
    col_types: &[(String, String)],
    as_name: &str,
) -> Result<ExprType, String> {
    match e {
        Expr::Num(_) => Ok(ExprType::Number),
        Expr::Str(_) => Ok(ExprType::String),
        Expr::Ident(name) => {
            let (_, dtype) = col_types
                .iter()
                .find(|(n, _)| n == name)
                .ok_or_else(|| format!("analysis/invalid-args: unknown column in expr: {name}"))?;
            Ok(ExprType::from_dtype(dtype))
        }
        Expr::Neg(inner) => {
            let t = check_types(inner, col_types, as_name)?;
            if t != ExprType::Number {
                return Err(format!(
                    "analysis/invalid-args: mutate {as_name}: cannot negate {}",
                    t.name()
                ));
            }
            Ok(ExprType::Number)
        }
        Expr::Add(a, b) => bin_type(a, b, col_types, as_name, "add", true),
        Expr::Sub(a, b) => bin_type(a, b, col_types, as_name, "subtract", true),
        Expr::Mul(a, b) => bin_type(a, b, col_types, as_name, "multiply", false),
        Expr::Div(a, b) => bin_type(a, b, col_types, as_name, "divide", false),
        Expr::ToInt(inner) => {
            check_types(inner, col_types, as_name)?;
            Ok(ExprType::Number)
        }
        Expr::ToStr(inner) => {
            check_types(inner, col_types, as_name)?;
            Ok(ExprType::String)
        }
        Expr::ToDate(inner, _) => {
            let t = check_types(inner, col_types, as_name)?;
            if t != ExprType::String {
                return Err(format!(
                    "analysis/invalid-args: mutate {as_name}: to_date value must be string"
                ));
            }
            Ok(ExprType::Date)
        }
    }
}

fn bin_type(
    a: &Expr,
    b: &Expr,
    col_types: &[(String, String)],
    as_name: &str,
    verb: &str,
    rhs_first: bool,
) -> Result<ExprType, String> {
    let lt = check_types(a, col_types, as_name)?;
    let rt = check_types(b, col_types, as_name)?;
    if lt == ExprType::Number && rt == ExprType::Number {
        return Ok(ExprType::Number);
    }
    let msg = if rhs_first {
        // `+` / `-`: cannot add {rhs} to {lhs} / cannot subtract {rhs} from {lhs}
        match verb {
            "add" => format!("cannot add {} to {}", rt.name(), lt.name()),
            _ => format!("cannot subtract {} from {}", rt.name(), lt.name()),
        }
    } else {
        // `*` / `/`: cannot multiply {lhs} by {rhs} / cannot divide {lhs} by {rhs}
        match verb {
            "multiply" => format!("cannot multiply {} by {}", lt.name(), rt.name()),
            _ => format!("cannot divide {} by {}", lt.name(), rt.name()),
        }
    };
    Err(format!("analysis/invalid-args: mutate {as_name}: {msg}"))
}

/// Evaluate against a working table row. `col_index` maps original column names.
/// Unknown identifier → error (caught before any write).
pub fn eval_expr(
    e: &Expr,
    row: &[Value],
    col_index: &[(String, usize)],
    counters: &mut EvalCounters,
) -> Result<Value, String> {
    match e {
        Expr::Num(n) => Ok(Value::F64(*n)),
        Expr::Str(s) => Ok(Value::Str(s.clone())),
        Expr::Ident(name) => {
            let (_, idx) = col_index
                .iter()
                .find(|(n, _)| n == name)
                .ok_or_else(|| format!("analysis/invalid-args: unknown column in expr: {name}"))?;
            Ok(row.get(*idx).cloned().unwrap_or(Value::Null))
        }
        Expr::Neg(inner) => {
            let v = eval_expr(inner, row, col_index, counters)?;
            // Null propagates (not a failure); numeric negate cannot fail.
            Ok(match v.as_f64() {
                Some(n) => Value::F64(-n),
                None => Value::Null,
            })
        }
        Expr::Add(a, b) => arith(a, b, row, col_index, counters, |x, y| x + y),
        Expr::Sub(a, b) => arith(a, b, row, col_index, counters, |x, y| x - y),
        Expr::Mul(a, b) => arith(a, b, row, col_index, counters, |x, y| x * y),
        Expr::Div(a, b) => {
            let va = eval_expr(a, row, col_index, counters)?;
            let vb = eval_expr(b, row, col_index, counters)?;
            match (va.as_f64(), vb.as_f64()) {
                (Some(x), Some(y)) if y != 0.0 => Ok(Value::F64(x / y)),
                (Some(_), Some(_)) => {
                    // Divide by zero is a runtime failure; Null operands propagate.
                    counters.note_arith_fail();
                    Ok(Value::Null)
                }
                _ => Ok(Value::Null),
            }
        }
        Expr::ToInt(inner) => {
            let v = eval_expr(inner, row, col_index, counters)?;
            if matches!(v, Value::Null) {
                return Ok(Value::Null);
            }
            match cast_to_int(&v) {
                Some(n) => Ok(Value::I64(n)),
                None => {
                    counters.note_cast_fail();
                    Ok(Value::Null)
                }
            }
        }
        Expr::ToStr(inner) => {
            let v = eval_expr(inner, row, col_index, counters)?;
            if matches!(v, Value::Null) {
                return Ok(Value::Null);
            }
            match cast_to_str(&v) {
                Some(s) => Ok(Value::Str(s)),
                None => {
                    counters.note_cast_fail();
                    Ok(Value::Null)
                }
            }
        }
        Expr::ToDate(inner, fmt) => {
            let v = eval_expr(inner, row, col_index, counters)?;
            if matches!(v, Value::Null) {
                return Ok(Value::Null);
            }
            match cast_to_date(&v, fmt) {
                Some(s) => Ok(Value::Str(s)),
                None => {
                    counters.note_cast_fail();
                    Ok(Value::Null)
                }
            }
        }
    }
}

fn cast_to_int(v: &Value) -> Option<i64> {
    match v {
        Value::Null => None,
        Value::I64(i) => Some(*i),
        Value::F64(f) => {
            if f.is_finite() && f.fract() == 0.0 {
                i64::try_from(*f as i128).ok()
            } else {
                None
            }
        }
        Value::Bool(b) => Some(if *b { 1 } else { 0 }),
        Value::Str(s) => {
            let t = s.trim();
            if let Ok(i) = t.parse::<i64>() {
                return Some(i);
            }
            // whole floats like "12.0" cast; "12.5" / "abc" fail
            t.parse::<f64>().ok().and_then(|f| {
                if f.is_finite() && f.fract() == 0.0 {
                    i64::try_from(f as i128).ok()
                } else {
                    None
                }
            })
        }
    }
}

fn cast_to_str(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::Str(s) => Some(s.clone()),
        Value::I64(i) => Some(i.to_string()),
        Value::F64(f) => {
            if f.is_finite() {
                Some(format!("{f}"))
            } else {
                None
            }
        }
        Value::Bool(b) => Some(if *b { "true" } else { "false" }.to_string()),
    }
}

fn cast_to_date(v: &Value, fmt: &str) -> Option<String> {
    let s = match v {
        Value::Null => return None,
        Value::Str(s) => s.as_str(),
        _ => return None,
    };
    let t = parse_with_fmt(s.trim(), fmt)?;
    Some(t.to_iso(validate_date_fmt(fmt).unwrap_or(false)))
}

fn arith(
    a: &Expr,
    b: &Expr,
    row: &[Value],
    col_index: &[(String, usize)],
    counters: &mut EvalCounters,
    f: impl Fn(f64, f64) -> f64,
) -> Result<Value, String> {
    let va = eval_expr(a, row, col_index, counters)?;
    let vb = eval_expr(b, row, col_index, counters)?;
    // Null operands propagate (not a runtime failure); + - * cannot fail otherwise.
    Ok(match (va.as_f64(), vb.as_f64()) {
        (Some(x), Some(y)) => Value::F64(f(x, y)),
        _ => Value::Null,
    })
}

/// Helper used by tests / callers that only need a static table check.
#[allow(dead_code)]
pub fn eval_on_table(e: &Expr, table: &Table, row_idx: usize) -> Result<Value, String> {
    let index: Vec<(String, usize)> = table
        .columns
        .iter()
        .enumerate()
        .map(|(i, c)| (c.name.clone(), i))
        .collect();
    let row = &table.rows[row_idx];
    let mut counters = EvalCounters::default();
    eval_expr(e, row, &index, &mut counters)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn types(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn r1_add_number_to_string_rejected() {
        let e = parse_expr("region + 1").unwrap();
        let err = check_types(&e, &types(&[("region", "utf8")]), "x").unwrap_err();
        assert!(
            err.contains("mutate x: cannot add number to string"),
            "err={err}"
        );
    }

    #[test]
    fn r1_other_arith_verbs() {
        let cols = types(&[("region", "utf8"), ("flag", "bool"), ("day", "date"), ("n", "i64")]);
        let cases = [
            ("region - 1", "cannot subtract number from string"),
            ("1 - region", "cannot subtract string from number"),
            ("region * 2", "cannot multiply string by number"),
            ("2 * region", "cannot multiply number by string"),
            ("region / 2", "cannot divide string by number"),
            ("flag + 1", "cannot add number to boolean"),
            ("day + 1", "cannot add number to date"),
            ("-region", "cannot negate string"),
            ("-flag", "cannot negate boolean"),
        ];
        for (src, want) in cases {
            let e = parse_expr(src).unwrap();
            let err = check_types(&e, &cols, "x").unwrap_err();
            assert!(err.contains(want), "src={src} err={err}");
        }
    }

    #[test]
    fn numeric_mutate_still_ok() {
        let e = parse_expr("n * 2 + 1").unwrap();
        assert_eq!(
            check_types(&e, &types(&[("n", "i64")]), "x").unwrap(),
            ExprType::Number
        );
    }

    #[test]
    fn casts_typecheck() {
        let cols = types(&[("n", "i64"), ("s", "utf8"), ("d", "date")]);
        let e = parse_expr("to_int(\"12\")").unwrap();
        assert_eq!(check_types(&e, &cols, "x").unwrap(), ExprType::Number);
        let e = parse_expr("to_str(n)").unwrap();
        assert_eq!(check_types(&e, &cols, "x").unwrap(), ExprType::String);
        let e = parse_expr("to_date(\"2026/10/02\", \"%Y/%m/%d\")").unwrap();
        assert_eq!(check_types(&e, &cols, "x").unwrap(), ExprType::Date);
        // date arithmetic is a type error
        let e = parse_expr("to_date(\"2026-10-02\", \"%Y-%m-%d\") + 1").unwrap();
        let err = check_types(&e, &cols, "x").unwrap_err();
        assert!(err.contains("cannot add number to date"), "err={err}");
        // to_date value operand must be string-static-typed
        let e = parse_expr("to_date(n, \"%Y\")").unwrap();
        let err = check_types(&e, &cols, "x").unwrap_err();
        assert!(err.contains("to_date value must be string"), "err={err}");
    }

    #[test]
    fn cast_and_arith_nulls_counted() {
        let cols = vec![
            ("a".to_string(), 0usize),
            ("s".to_string(), 1usize),
        ];
        let row = vec![Value::I64(1), Value::Str("abc".into())];
        let e = parse_expr("to_int(s)").unwrap();
        let mut c = EvalCounters::default();
        c.begin_row();
        assert_eq!(eval_expr(&e, &row, &cols, &mut c).unwrap(), Value::Null);
        c.end_row();
        assert_eq!(c.cast_null, 1);
        assert_eq!(c.arith_null, 0);
        let e = parse_expr("a / 0").unwrap();
        let mut c = EvalCounters::default();
        c.begin_row();
        assert_eq!(eval_expr(&e, &row, &cols, &mut c).unwrap(), Value::Null);
        c.end_row();
        assert_eq!(c.arith_null, 1);
    }

    #[test]
    fn propagation_not_counted_and_rows_count_once() {
        let cols = vec![
            ("a".to_string(), 0usize),
            ("s".to_string(), 1usize),
        ];
        // Null propagation in arithmetic is silent.
        let row = vec![Value::Null, Value::Str("10".into())];
        let e = parse_expr("a + 1").unwrap();
        let mut c = EvalCounters::default();
        c.begin_row();
        assert_eq!(eval_expr(&e, &row, &cols, &mut c).unwrap(), Value::Null);
        c.end_row();
        assert_eq!(c.arith_null, 0);
        // to_int(null) is propagation, not a cast failure.
        let e = parse_expr("to_int(a)").unwrap();
        let mut c = EvalCounters::default();
        c.begin_row();
        assert_eq!(eval_expr(&e, &row, &cols, &mut c).unwrap(), Value::Null);
        c.end_row();
        assert_eq!(c.cast_null, 0);
        // Two failing nodes in one row count once.
        let row = vec![Value::I64(1), Value::Str("abc".into())];
        let e = parse_expr("(a / 0) + (a / 0)").unwrap();
        let mut c = EvalCounters::default();
        c.begin_row();
        assert_eq!(eval_expr(&e, &row, &cols, &mut c).unwrap(), Value::Null);
        c.end_row();
        assert_eq!(c.arith_null, 1);
        let e = parse_expr("to_int(s) + to_int(s)").unwrap();
        let mut c = EvalCounters::default();
        c.begin_row();
        assert_eq!(eval_expr(&e, &row, &cols, &mut c).unwrap(), Value::Null);
        c.end_row();
        assert_eq!(c.cast_null, 1);
        assert_eq!(c.arith_null, 0);
    }
}
