//! Bundle loading, canonical writing and JSON Patch (RFC 6902) recording.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Limits;
use crate::error::{Error, Result, read_text};
use crate::json::{self, unescape_token};

/// A loaded base bundle.
#[derive(Clone, Debug)]
pub struct LoadedBundle {
    pub value: Value,
    /// Path as given by the user, for messages and reports.
    pub source: String,
    /// SHA-256 of the file bytes.
    pub sha256: String,
}

/// Load a bundle file, enforcing the size and resource limits.
pub fn load(path: &Path, limits: &Limits) -> Result<LoadedBundle> {
    let text = read_text(path, limits.max_bundle_bytes)?;
    let value = parse(&text, limits, &path.display().to_string())?;
    Ok(LoadedBundle {
        value,
        source: crate::scenario::display_path(path),
        sha256: json::sha256_hex(text.as_bytes()),
    })
}

/// Parse bundle text and check that it is a Bundle within the limits.
pub fn parse(text: &str, limits: &Limits, what: &str) -> Result<Value> {
    let value = json::parse_bounded(text, limits.max_bundle_bytes, what)?;
    check_shape(&value, limits, what)?;
    Ok(value)
}

/// Check that a value looks like a Bundle with an entry array.
pub fn check_shape(value: &Value, limits: &Limits, what: &str) -> Result<()> {
    if value.get("resourceType").and_then(Value::as_str) != Some("Bundle") {
        return Err(Error::input(format!(
            "{what} is not a FHIR Bundle (resourceType must be \"Bundle\")"
        )));
    }
    match value.get("entry") {
        None => {}
        Some(Value::Array(entries)) => {
            if entries.len() > limits.max_resources {
                return Err(Error::input(format!(
                    "{what} has {} entries, over the limit of {}",
                    entries.len(),
                    limits.max_resources
                )));
            }
        }
        Some(_) => {
            return Err(Error::input(format!(
                "{what}: Bundle.entry must be an array"
            )));
        }
    }
    Ok(())
}

/// The bundle's entries, or an empty slice.
pub fn entries(bundle: &Value) -> &[Value] {
    bundle
        .get("entry")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// The resource of an entry, if any.
pub fn resource(entry: &Value) -> Option<&Value> {
    entry.get("resource").filter(|r| r.is_object())
}

/// `Type/id` for a resource, when it has both.
pub fn resource_key(resource: &Value) -> Option<String> {
    let t = resource.get("resourceType")?.as_str()?;
    let id = resource.get("id")?.as_str()?;
    Some(format!("{t}/{id}"))
}

/// Index of the entry whose resource has the given `Type/id`.
pub fn find_entry(bundle: &Value, key: &str) -> Option<usize> {
    entries(bundle)
        .iter()
        .position(|e| resource(e).and_then(resource_key).as_deref() == Some(key))
}

/// Pointer to an entry's resource.
pub fn resource_ptr(index: usize) -> String {
    format!("/entry/{index}/resource")
}

/// The first coding code of a CodeableConcept-valued field.
pub fn first_code(resource: &Value, field: &str) -> Option<String> {
    resource
        .get(field)?
        .get("coding")?
        .as_array()?
        .iter()
        .find_map(|c| c.get("code").and_then(Value::as_str))
        .map(str::to_string)
}

/// True when any coding of the field has the given code.
pub fn has_code(resource: &Value, field: &str, code: &str) -> bool {
    resource
        .get(field)
        .and_then(|c| c.get("coding"))
        .and_then(Value::as_array)
        .is_some_and(|codings| {
            codings
                .iter()
                .any(|c| c.get("code").and_then(Value::as_str) == Some(code))
        })
}

/// Number of resources in a bundle.
pub fn resource_count(bundle: &Value) -> usize {
    entries(bundle)
        .iter()
        .filter(|e| resource(e).is_some())
        .count()
}

// ---------------------------------------------------------------------------
// JSON Patch

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PatchKind {
    Add,
    Remove,
    Replace,
    Move,
}

/// One RFC 6902 operation. Only add, remove, replace and move are produced.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PatchOp {
    pub op: PatchKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
}

/// Why a patch operation could not be applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatchError(pub String);

impl std::fmt::Display for PatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JSON Patch error: {}", self.0)
    }
}

fn split_pointer(path: &str) -> std::result::Result<Vec<String>, PatchError> {
    if path.is_empty() {
        return Ok(Vec::new());
    }
    if !path.starts_with('/') {
        return Err(PatchError(format!("pointer {path:?} must start with /")));
    }
    Ok(path[1..].split('/').map(unescape_token).collect())
}

fn parent_mut<'a>(
    doc: &'a mut Value,
    tokens: &[String],
) -> std::result::Result<&'a mut Value, PatchError> {
    let mut cur = doc;
    for t in tokens {
        cur = match cur {
            Value::Object(map) => map
                .get_mut(t)
                .ok_or_else(|| PatchError(format!("no member {t:?}")))?,
            Value::Array(items) => {
                let idx: usize = t
                    .parse()
                    .map_err(|_| PatchError(format!("bad array index {t:?}")))?;
                items
                    .get_mut(idx)
                    .ok_or_else(|| PatchError(format!("index {idx} out of range")))?
            }
            _ => return Err(PatchError(format!("cannot descend into a scalar at {t:?}"))),
        };
    }
    Ok(cur)
}

fn add_at(doc: &mut Value, path: &str, value: Value) -> std::result::Result<(), PatchError> {
    let tokens = split_pointer(path)?;
    let Some((last, parents)) = tokens.split_last() else {
        *doc = value;
        return Ok(());
    };
    match parent_mut(doc, parents)? {
        Value::Object(map) => {
            map.insert(last.clone(), value);
            Ok(())
        }
        Value::Array(items) => {
            if last == "-" {
                items.push(value);
                return Ok(());
            }
            let idx: usize = last
                .parse()
                .map_err(|_| PatchError(format!("bad array index {last:?}")))?;
            if idx > items.len() {
                return Err(PatchError(format!("index {idx} out of range for add")));
            }
            items.insert(idx, value);
            Ok(())
        }
        _ => Err(PatchError(format!("cannot add under a scalar at {path}"))),
    }
}

fn remove_at(doc: &mut Value, path: &str) -> std::result::Result<Value, PatchError> {
    let tokens = split_pointer(path)?;
    let Some((last, parents)) = tokens.split_last() else {
        return Err(PatchError(String::from("cannot remove the whole document")));
    };
    match parent_mut(doc, parents)? {
        // shift_remove keeps the order of the remaining keys.
        Value::Object(map) => map
            .shift_remove(last.as_str())
            .ok_or_else(|| PatchError(format!("no member {last:?} to remove"))),
        Value::Array(items) => {
            let idx: usize = last
                .parse()
                .map_err(|_| PatchError(format!("bad array index {last:?}")))?;
            if idx >= items.len() {
                return Err(PatchError(format!("index {idx} out of range for remove")));
            }
            Ok(items.remove(idx))
        }
        _ => Err(PatchError(format!(
            "cannot remove under a scalar at {path}"
        ))),
    }
}

fn replace_at(doc: &mut Value, path: &str, value: Value) -> std::result::Result<Value, PatchError> {
    let tokens = split_pointer(path)?;
    let target = parent_mut(doc, &tokens)?;
    Ok(std::mem::replace(target, value))
}

/// Apply a patch to a copy of a document.
pub fn apply_patch(doc: &Value, ops: &[PatchOp]) -> std::result::Result<Value, PatchError> {
    let mut out = doc.clone();
    for op in ops {
        match op.op {
            PatchKind::Add => {
                let v = op
                    .value
                    .clone()
                    .ok_or_else(|| PatchError(String::from("add without value")))?;
                add_at(&mut out, &op.path, v)?;
            }
            PatchKind::Remove => {
                remove_at(&mut out, &op.path)?;
            }
            PatchKind::Replace => {
                let v = op
                    .value
                    .clone()
                    .ok_or_else(|| PatchError(String::from("replace without value")))?;
                replace_at(&mut out, &op.path, v)?;
            }
            PatchKind::Move => {
                let from = op
                    .from
                    .as_deref()
                    .ok_or_else(|| PatchError(String::from("move without from")))?;
                let v = remove_at(&mut out, from)?;
                add_at(&mut out, &op.path, v)?;
            }
        }
    }
    Ok(out)
}

/// Edits a document and records every change as a JSON Patch operation.
#[derive(Clone, Debug)]
pub struct Editor {
    doc: Value,
    ops: Vec<PatchOp>,
}

impl Editor {
    pub fn new(doc: Value) -> Self {
        Self {
            doc,
            ops: Vec::new(),
        }
    }

    pub fn doc(&self) -> &Value {
        &self.doc
    }

    pub fn get(&self, path: &str) -> Option<&Value> {
        self.doc.pointer(path)
    }

    pub fn add(&mut self, path: &str, value: Value) -> std::result::Result<(), PatchError> {
        add_at(&mut self.doc, path, value.clone())?;
        self.ops.push(PatchOp {
            op: PatchKind::Add,
            from: None,
            path: path.to_string(),
            value: Some(value),
        });
        Ok(())
    }

    pub fn remove(&mut self, path: &str) -> std::result::Result<Value, PatchError> {
        let old = remove_at(&mut self.doc, path)?;
        self.ops.push(PatchOp {
            op: PatchKind::Remove,
            from: None,
            path: path.to_string(),
            value: None,
        });
        Ok(old)
    }

    /// Replace a value. Recording is skipped when the value does not change,
    /// so the patch lists only paths that really changed.
    pub fn replace(&mut self, path: &str, value: Value) -> std::result::Result<(), PatchError> {
        if self.doc.pointer(path) == Some(&value) {
            return Ok(());
        }
        replace_at(&mut self.doc, path, value.clone())?;
        self.ops.push(PatchOp {
            op: PatchKind::Replace,
            from: None,
            path: path.to_string(),
            value: Some(value),
        });
        Ok(())
    }

    /// Set a member: replace when present, add when absent.
    pub fn set(&mut self, path: &str, value: Value) -> std::result::Result<(), PatchError> {
        if self.doc.pointer(path).is_some() {
            self.replace(path, value)
        } else {
            self.add(path, value)
        }
    }

    pub fn mv(&mut self, from: &str, path: &str) -> std::result::Result<(), PatchError> {
        if from == path {
            return Ok(());
        }
        let v = remove_at(&mut self.doc, from)?;
        add_at(&mut self.doc, path, v)?;
        self.ops.push(PatchOp {
            op: PatchKind::Move,
            from: Some(from.to_string()),
            path: path.to_string(),
            value: None,
        });
        Ok(())
    }

    pub fn op_count(&self) -> usize {
        self.ops.len()
    }

    pub fn finish(self) -> (Value, Vec<PatchOp>) {
        (self.doc, self.ops)
    }
}

/// Remove entries by resource key from a bundle copy. Returns the new bundle.
pub fn without_entries(bundle: &Value, keys: &[String]) -> Value {
    let mut out = bundle.clone();
    if let Some(Value::Array(items)) = out.get_mut("entry") {
        items.retain(|e| {
            let key = resource(e).and_then(resource_key);
            !key.is_some_and(|k| keys.contains(&k))
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn editor_records_a_patch_that_replays() {
        let base = json!({"resourceType": "Bundle", "entry": [{"a": 1}, {"b": 2}, {"c": 3}]});
        let mut ed = Editor::new(base.clone());
        ed.replace("/entry/0/a", json!(5)).unwrap();
        ed.add("/entry/1/x", json!("y")).unwrap();
        ed.mv("/entry/2", "/entry/0").unwrap();
        ed.remove("/entry/1/a").unwrap();
        let (doc, ops) = ed.finish();
        assert_eq!(apply_patch(&base, &ops).unwrap(), doc);
        assert_eq!(
            doc,
            json!({"resourceType": "Bundle", "entry": [{"c": 3}, {}, {"b": 2, "x": "y"}]})
        );
    }

    #[test]
    fn replace_with_same_value_is_not_recorded() {
        let mut ed = Editor::new(json!({"a": 1}));
        ed.replace("/a", json!(1)).unwrap();
        assert_eq!(ed.op_count(), 0);
    }

    #[test]
    fn remove_keeps_key_order() {
        let mut ed = Editor::new(json!({"a": 1, "b": 2, "c": 3}));
        ed.remove("/a").unwrap();
        let keys: Vec<&String> = ed.doc().as_object().unwrap().keys().collect();
        assert_eq!(keys, ["b", "c"]);
    }

    #[test]
    fn rejects_non_bundles() {
        let limits = Limits::default();
        assert!(parse(r#"{"resourceType": "Patient"}"#, &limits, "x").is_err());
        assert!(parse(r#"{"resourceType": "Bundle", "entry": {}}"#, &limits, "x").is_err());
        assert!(parse(r#"{"resourceType": "Bundle", "entry": []}"#, &limits, "x").is_ok());
    }
}
