//! JSON helpers: canonical output, JSON Pointer tokens, hashing, TOML conversion
//! and number handling that is safe with `arbitrary_precision`.

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// Canonical text for a JSON value: 2-space indent, LF line ends, one trailing
/// newline. Key order is the insertion order (`preserve_order`).
pub fn canonical(value: &Value) -> Result<String> {
    let mut text = serde_json::to_string_pretty(value)
        .map_err(|e| Error::internal(format!("cannot serialise JSON: {e}")))?;
    text.push('\n');
    Ok(text)
}

/// Canonical text for any serialisable value.
pub fn canonical_of<T: serde::Serialize>(value: &T) -> Result<String> {
    let mut text = serde_json::to_string_pretty(value)
        .map_err(|e| Error::internal(format!("cannot serialise JSON: {e}")))?;
    text.push('\n');
    Ok(text)
}

/// Compact one-line JSON text, for messages.
pub fn compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| String::from("<unprintable>"))
}

/// Parse JSON text with a size bound. serde_json also bounds nesting depth at
/// 128 levels by default, so deep input fails cleanly instead of overflowing.
pub fn parse_bounded(text: &str, max_bytes: usize, what: &str) -> Result<Value> {
    if text.len() > max_bytes {
        return Err(Error::input(format!(
            "{what} is {} bytes, over the limit of {max_bytes} bytes",
            text.len()
        )));
    }
    serde_json::from_str(text).map_err(|e| Error::input(format!("{what} is not valid JSON: {e}")))
}

/// Escape one JSON Pointer reference token (RFC 6901).
pub fn escape_token(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

/// Unescape one JSON Pointer reference token (RFC 6901).
pub fn unescape_token(token: &str) -> String {
    token.replace("~1", "/").replace("~0", "~")
}

/// Join a pointer and a token.
pub fn ptr_join(base: &str, token: &str) -> String {
    format!("{base}/{}", escape_token(token))
}

/// Join a pointer and an array index.
pub fn ptr_index(base: &str, index: usize) -> String {
    format!("{base}/{index}")
}

/// Lower-case hex SHA-256 of some bytes.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest.iter() {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Short form of a hex hash for display: first 4 and last 4 characters.
pub fn short_hash(hex: &str) -> String {
    if hex.len() <= 8 {
        return hex.to_string();
    }
    format!("{}…{}", &hex[..4], &hex[hex.len() - 4..])
}

/// FNV-1a 64-bit hash, used for fingerprints. Stable on every platform.
pub fn fnv1a64(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Convert a TOML value to JSON. Datetimes become strings.
pub fn toml_to_json(value: &toml::Value) -> Value {
    match value {
        toml::Value::String(s) => Value::String(s.clone()),
        toml::Value::Integer(i) => Value::from(*i),
        toml::Value::Float(f) => float_value(*f),
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Datetime(d) => Value::String(d.to_string()),
        toml::Value::Array(items) => Value::Array(items.iter().map(toml_to_json).collect()),
        toml::Value::Table(table) => {
            let mut map = Map::new();
            for (k, v) in table {
                map.insert(k.clone(), toml_to_json(v));
            }
            Value::Object(map)
        }
    }
}

/// Convert JSON to TOML. Nulls are dropped (TOML has no null).
pub fn json_to_toml(value: &Value) -> Option<toml::Value> {
    match value {
        Value::Null => None,
        Value::Bool(b) => Some(toml::Value::Boolean(*b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(toml::Value::Integer(i))
            } else {
                n.as_f64().map(toml::Value::Float)
            }
        }
        Value::String(s) => Some(toml::Value::String(s.clone())),
        Value::Array(items) => Some(toml::Value::Array(
            items.iter().filter_map(json_to_toml).collect(),
        )),
        Value::Object(map) => {
            let mut table = toml::Table::new();
            for (k, v) in map {
                if let Some(t) = json_to_toml(v) {
                    table.insert(k.clone(), t);
                }
            }
            Some(toml::Value::Table(table))
        }
    }
}

/// A JSON number from an f64, via its shortest decimal text, so that the
/// result is the same with or without `arbitrary_precision`.
pub fn float_value(f: f64) -> Value {
    if !f.is_finite() {
        return Value::Null;
    }
    let text = format!("{f}");
    serde_json::from_str::<Value>(&text).unwrap_or(Value::Null)
}

/// A JSON number with a fixed number of decimals, e.g. `6.1` or `500`.
pub fn decimal_value(f: f64, decimals: usize) -> Value {
    if !f.is_finite() {
        return Value::Null;
    }
    let text = format!("{f:.decimals$}");
    serde_json::from_str::<Value>(&text).unwrap_or(Value::Null)
}

/// Number of decimals in a JSON number as written (`1.50` has 2).
/// With `arbitrary_precision` the original text is kept, so this is exact.
pub fn decimals_of(n: &serde_json::Number) -> usize {
    let text = n.to_string();
    let mantissa = text.split(['e', 'E']).next().unwrap_or("");
    match mantissa.split_once('.') {
        Some((_, frac)) => frac.len(),
        None => 0,
    }
}

/// Numeric value of a JSON value, if it is a number.
pub fn as_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

/// True when two JSON values are equal, comparing numbers by value
/// (so `6.1` equals `6.10`).
pub fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_f64(), y.as_f64()) {
            (Some(p), Some(q)) => (p - q).abs() <= 1e-9 * p.abs().max(q.abs()).max(1.0),
            _ => x.to_string() == y.to_string(),
        },
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(p, q)| values_equal(p, q))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| values_equal(v, w)))
        }
        _ => a == b,
    }
}

/// Read a string argument with a default.
pub fn arg_str(args: &Map<String, Value>, key: &str, default: &str) -> String {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| default.to_string())
}

/// Read an integer argument with a default.
pub fn arg_i64(args: &Map<String, Value>, key: &str, default: i64) -> i64 {
    args.get(key).and_then(Value::as_i64).unwrap_or(default)
}

/// Read a float argument with a default.
pub fn arg_f64(args: &Map<String, Value>, key: &str, default: f64) -> f64 {
    args.get(key).and_then(as_f64).unwrap_or(default)
}

/// Short human text for a JSON value, for reports: strings unquoted.
pub fn display_value(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::from("nothing"),
        other => {
            let text = compact(other);
            if text.chars().count() > 160 {
                let cut: String = text.chars().take(157).collect();
                format!("{cut}...")
            } else {
                text
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pointer_tokens_round_trip() {
        assert_eq!(escape_token("a/b~c"), "a~1b~0c");
        assert_eq!(unescape_token("a~1b~0c"), "a/b~c");
    }

    #[test]
    fn decimals_are_kept_as_written() {
        let v: Value = serde_json::from_str("1.50").unwrap();
        match v {
            Value::Number(n) => assert_eq!(decimals_of(&n), 2),
            _ => panic!("not a number"),
        }
        assert_eq!(compact(&decimal_value(7.2 - 1.1, 1)), "6.1");
    }

    #[test]
    fn numbers_compare_by_value() {
        let a: Value = serde_json::from_str("6.10").unwrap();
        assert!(values_equal(&a, &json!(6.1)));
        assert!(!values_equal(&a, &json!(7.2)));
    }

    #[test]
    fn fnv_is_stable() {
        assert_eq!(fnv1a64(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64("a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn toml_json_round_trip() {
        let t: toml::Value = toml::from_str::<toml::Table>("a = 1\nb = 2.5\nc = \"x\"")
            .map(toml::Value::Table)
            .unwrap();
        let j = toml_to_json(&t);
        assert_eq!(compact(&j), r#"{"a":1,"b":2.5,"c":"x"}"#);
        assert_eq!(json_to_toml(&j), Some(t));
    }

    #[test]
    fn short_hash_keeps_ends() {
        assert_eq!(short_hash("3f9a00000000c21e"), "3f9a…c21e");
    }
}
