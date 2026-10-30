//! Whitelist arithmetic expressions for derived columns (mutate).
//! Grammar: numbers, identifiers, + - * /, parentheses, unary minus.
//! No function calls, strings, or assignment.

use crate::dataset::{Table, Value};

#[derive(Debug, Clone)]
pub enum Expr {
    Num(f64),
    Ident(String),
    Neg(Box<Expr>),
    Add(Box<Expr>, Box<Expr>),
    Sub(Box<Expr>, Box<Expr>),
    Mul(Box<Expr>, Box<Expr>),
    Div(Box<Expr>, Box<Expr>),
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
            Ok(Expr::Ident(name))
        }
        Some(c) => Err(format!(
            "analysis/invalid-args: illegal character '{c}' in expr"
        )),
        None => Err("analysis/invalid-args: unexpected end of expr".into()),
    }
}

/// Evaluate against a working table row. `col_index` maps original column names.
/// Unknown identifier → error (caught before any write).
pub fn eval_expr(
    e: &Expr,
    row: &[Value],
    col_index: &[(String, usize)],
) -> Result<Value, String> {
    match e {
        Expr::Num(n) => Ok(Value::F64(*n)),
        Expr::Ident(name) => {
            let (_, idx) = col_index
                .iter()
                .find(|(n, _)| n == name)
                .ok_or_else(|| format!("analysis/invalid-args: unknown column in expr: {name}"))?;
            Ok(row.get(*idx).cloned().unwrap_or(Value::Null))
        }
        Expr::Neg(inner) => {
            let v = eval_expr(inner, row, col_index)?;
            Ok(match v.as_f64() {
                Some(n) => Value::F64(-n),
                None => Value::Null,
            })
        }
        Expr::Add(a, b) => arith(a, b, row, col_index, |x, y| x + y),
        Expr::Sub(a, b) => arith(a, b, row, col_index, |x, y| x - y),
        Expr::Mul(a, b) => arith(a, b, row, col_index, |x, y| x * y),
        Expr::Div(a, b) => {
            let va = eval_expr(a, row, col_index)?;
            let vb = eval_expr(b, row, col_index)?;
            match (va.as_f64(), vb.as_f64()) {
                (Some(x), Some(y)) if y != 0.0 => Ok(Value::F64(x / y)),
                _ => Ok(Value::Null),
            }
        }
    }
}

fn arith(
    a: &Expr,
    b: &Expr,
    row: &[Value],
    col_index: &[(String, usize)],
    f: impl Fn(f64, f64) -> f64,
) -> Result<Value, String> {
    let va = eval_expr(a, row, col_index)?;
    let vb = eval_expr(b, row, col_index)?;
    Ok(match (va.as_f64(), vb.as_f64()) {
        (Some(x), Some(y)) => Value::F64(f(x, y)),
        _ => Value::Null,
    })
}

/// Ensure every identifier in `e` exists in `available` (column names).
pub fn check_idents(e: &Expr, available: &[String]) -> Result<(), String> {
    match e {
        Expr::Num(_) => Ok(()),
        Expr::Ident(name) => {
            if available.iter().any(|c| c == name) {
                Ok(())
            } else {
                Err(format!("analysis/invalid-args: unknown column in expr: {name}"))
            }
        }
        Expr::Neg(inner) => check_idents(inner, available),
        Expr::Add(a, b) | Expr::Sub(a, b) | Expr::Mul(a, b) | Expr::Div(a, b) => {
            check_idents(a, available)?;
            check_idents(b, available)
        }
    }
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
    eval_expr(e, row, &index)
}
