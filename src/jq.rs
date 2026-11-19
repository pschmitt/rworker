//! The few jq semantics the client relies on: truthiness, `//`, string
//! interpolation and `@tsv`, value ordering, and `column -t` alignment.

use serde_json::{Map, Number, Value};
use std::cmp::Ordering;
use unicode_width::UnicodeWidthStr;

pub static NULL: Value = Value::Null;

/// `.key`: null when missing or not an object.
pub fn get<'a>(v: &'a Value, key: &str) -> &'a Value {
    v.get(key).unwrap_or(&NULL)
}

/// `.a.b`.
pub fn path<'a>(v: &'a Value, keys: &[&str]) -> &'a Value {
    keys.iter().fold(v, |cur, k| get(cur, k))
}

pub fn truthy(v: &Value) -> bool {
    !matches!(v, Value::Null | Value::Bool(false))
}

/// `a // b`.
pub fn alt<'a>(a: &'a Value, b: &'a Value) -> &'a Value {
    if truthy(a) { a } else { b }
}

/// `(v // "")`, as text.
pub fn or_empty(v: &Value) -> String {
    if truthy(v) { raw(v) } else { String::new() }
}

/// `(v // "x")` as text.
pub fn or(v: &Value, default: &str) -> String {
    if truthy(v) {
        raw(v)
    } else {
        default.to_string()
    }
}

pub fn num(n: &Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    let f = n.as_f64().unwrap_or(0.0);
    fmt_f64(f)
}

pub fn fmt_f64(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 1e17 {
        format!("{}", f as i64)
    } else {
        format!("{f}")
    }
}

/// What `jq -r` prints for a value, and what `"\(v)"` interpolates.
pub fn raw(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => num(n),
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// One `@tsv` field.
pub fn tsv(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s
            .replace('\\', "\\\\")
            .replace('\t', "\\t")
            .replace('\n', "\\n")
            .replace('\r', "\\r"),
        other => raw(other),
    }
}

pub fn as_f64(v: &Value) -> f64 {
    v.as_f64().unwrap_or(0.0)
}

fn rank(v: &Value) -> u8 {
    match v {
        Value::Null => 0,
        Value::Bool(false) => 1,
        Value::Bool(true) => 2,
        Value::Number(_) => 3,
        Value::String(_) => 4,
        Value::Array(_) => 5,
        Value::Object(_) => 6,
    }
}

/// jq's total order of values (`sort_by`, `<`, `>`).
pub fn cmp(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            let (x, y) = (x.as_f64().unwrap_or(0.0), y.as_f64().unwrap_or(0.0));
            x.partial_cmp(&y).unwrap_or(Ordering::Equal)
        }
        (Value::String(x), Value::String(y)) => x.cmp(y),
        (Value::Array(x), Value::Array(y)) => {
            for (p, q) in x.iter().zip(y) {
                let o = cmp(p, q);
                if o != Ordering::Equal {
                    return o;
                }
            }
            x.len().cmp(&y.len())
        }
        (Value::Object(x), Value::Object(y)) => {
            let mut kx: Vec<&String> = x.keys().collect();
            let mut ky: Vec<&String> = y.keys().collect();
            kx.sort();
            ky.sort();
            match kx.cmp(&ky) {
                Ordering::Equal => {
                    for k in kx {
                        let o = cmp(&x[k], &y[k]);
                        if o != Ordering::Equal {
                            return o;
                        }
                    }
                    Ordering::Equal
                }
                o => o,
            }
        }
        _ => rank(a).cmp(&rank(b)),
    }
}

/// `(v // 0) > n` for a number n, in jq's ordering.
pub fn gt_num(v: &Value, n: f64) -> bool {
    let v = alt(v, &NULL);
    let v = if truthy(v) { v.clone() } else { Value::from(0) };
    cmp(&v, &Value::from(n)) == Ordering::Greater
}

/// The elements `.[]` iterates: an array's items or an object's values.
pub fn items(v: &Value) -> Option<Vec<&Value>> {
    match v {
        Value::Array(a) => Some(a.iter().collect()),
        Value::Object(o) => Some(o.values().collect()),
        _ => None,
    }
}

/// `. + {k: v, ...}` for objects.
pub fn merge(base: &Value, extra: &[(&str, Value)]) -> Value {
    let mut m = match base {
        Value::Object(m) => m.clone(),
        _ => Map::new(),
    };
    for (k, v) in extra {
        m.insert((*k).to_string(), v.clone());
    }
    Value::Object(m)
}

pub fn compact(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

pub fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}

/// jq's `%` (both sides truncated to integers).
pub fn modulo(a: f64, b: f64) -> f64 {
    let (a, b) = (a.trunc() as i64, b.trunc() as i64);
    if b == 0 { 0.0 } else { (a % b) as f64 }
}

/// `column -t -s $'\t'` (util-linux): empty lines dropped, short rows filled
/// with empty cells, every column but the last padded to its widest cell,
/// two spaces between columns.
pub fn column(text: &str) -> String {
    let rows: Vec<Vec<&str>> = text
        .split('\n')
        .filter(|l| !l.is_empty())
        .map(|l| l.split('\t').collect())
        .collect();
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0usize; cols];
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(UnicodeWidthStr::width(*cell));
        }
    }
    let mut out = String::new();
    for row in &rows {
        let mut line = String::new();
        for (i, width) in widths.iter().enumerate() {
            let cell = row.get(i).copied().unwrap_or("");
            line.push_str(cell);
            if i + 1 < cols {
                let pad = width - UnicodeWidthStr::width(cell);
                line.push_str(&" ".repeat(pad + 2));
            }
        }
        out.push_str(&line);
        out.push('\n');
    }
    // $(column ...) strips the final newline.
    out.pop();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_matches_util_linux() {
        assert_eq!(column("a\t\tb\nccc\td\te"), "a       b\nccc  d  e");
        assert_eq!(column("a\tb\nccc\t"), "a    b\nccc  ");
        assert_eq!(column("a\tb\n\nccc\td"), "a    b\nccc  d");
        assert_eq!(column("a\tb\tc\nd\te"), "a  b  c\nd  e  ");
        assert_eq!(column("x"), "x");
    }

    #[test]
    fn raw_and_tsv() {
        assert_eq!(raw(&Value::Null), "null");
        assert_eq!(raw(&serde_json::json!(1763581234)), "1763581234");
        assert_eq!(raw(&serde_json::json!(1.5)), "1.5");
        assert_eq!(raw(&serde_json::json!({"a": [1]})), r#"{"a":[1]}"#);
        assert_eq!(tsv(&serde_json::json!("a\tb")), "a\\tb");
        assert_eq!(tsv(&Value::Null), "");
    }

    #[test]
    fn ordering() {
        assert!(gt_num(&serde_json::json!(5), 4.0));
        assert!(!gt_num(&Value::Null, 4.0));
        assert!(gt_num(&serde_json::json!("x"), 4.0));
    }
}
