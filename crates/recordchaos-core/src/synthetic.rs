//! A heuristic warning for values that look like real identifiers. It does
//! not prove that a bundle is de-identified; it only catches obvious slips.

use serde_json::Value;

/// One suspicious value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suspect {
    pub path: String,
    pub kind: &'static str,
    pub sample: String,
}

fn is_digit(b: u8) -> bool {
    b.is_ascii_digit()
}

/// `ddd-dd-dddd` with no digit on either side.
fn has_ssn_shape(s: &str) -> bool {
    let b = s.as_bytes();
    b.windows(11).enumerate().any(|(i, w)| {
        let shape = w.iter().enumerate().all(|(j, c)| {
            if j == 3 || j == 6 {
                *c == b'-'
            } else {
                is_digit(*c)
            }
        });
        let before = i > 0 && b.get(i - 1).copied().is_some_and(is_digit);
        let after = b.get(i + 11).copied().is_some_and(is_digit);
        shape && !before && !after
    })
}

/// Ten or more digits once spaces, dots, dashes, brackets and a leading +
/// are removed, in a value that is mostly phone characters.
fn has_phone_shape(s: &str) -> bool {
    let allowed = |c: char| c.is_ascii_digit() || " .-()+".contains(c);
    if s.is_empty() || !s.chars().all(allowed) {
        return false;
    }
    let digits = s.chars().filter(char::is_ascii_digit).count();
    (10..=15).contains(&digits)
}

fn email_domain(s: &str) -> Option<&str> {
    let (local, domain) = s.split_once('@')?;
    if local.is_empty() || !domain.contains('.') || domain.contains(' ') || local.contains(' ') {
        return None;
    }
    Some(domain)
}

fn is_example_domain(domain: &str) -> bool {
    let d = domain.to_ascii_lowercase();
    ["example.org", "example.com", "example.net"]
        .iter()
        .any(|e| d == *e || d.ends_with(&format!(".{e}")))
}

fn walk(v: &Value, path: &str, out: &mut Vec<Suspect>) {
    match v {
        Value::String(s) => {
            let sample: String = s.chars().take(40).collect();
            if has_ssn_shape(s) {
                out.push(Suspect {
                    path: path.to_string(),
                    kind: "SSN-shaped number",
                    sample,
                });
            } else if has_phone_shape(s) {
                out.push(Suspect {
                    path: path.to_string(),
                    kind: "phone number",
                    sample,
                });
            } else if let Some(d) = email_domain(s) {
                if !is_example_domain(d) {
                    out.push(Suspect {
                        path: path.to_string(),
                        kind: "email outside example.org",
                        sample,
                    });
                }
            }
        }
        Value::Array(a) => {
            for (i, x) in a.iter().enumerate() {
                walk(x, &format!("{path}/{i}"), out);
            }
        }
        Value::Object(m) => {
            for (k, x) in m {
                walk(x, &crate::json::ptr_join(path, k), out);
            }
        }
        _ => {}
    }
}

/// Values in a bundle that look like real identifiers.
pub fn scan(bundle: &Value) -> Vec<Suspect> {
    let mut out = Vec::new();
    walk(bundle, "", &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn flags_real_looking_values() {
        let b = json!({"a": "123-45-6789", "b": "+1 (555) 010-2030", "c": "jane@hospital.test.io", "d": "a@example.org"});
        let kinds: Vec<&str> = scan(&b).iter().map(|s| s.kind).collect();
        assert_eq!(
            kinds,
            vec![
                "SSN-shaped number",
                "phone number",
                "email outside example.org"
            ]
        );
    }

    #[test]
    fn ignores_dates_codes_and_example_emails() {
        let b = json!({"d": "2026-01-04T08:30:00Z", "c": "synthetic-glucose", "e": "pat@example.org", "v": "A-0001"});
        assert!(scan(&b).is_empty());
    }
}
