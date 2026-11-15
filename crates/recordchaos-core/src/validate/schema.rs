//! Schema layer: the official R4 JSON schema (draft-06), bundled unmodified.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

use serde_json::Value;

use super::{Issue, LayerResult};
use crate::error::{Error, Result, read_text};

/// The official `fhir.schema.json` for R4 (4.0.1), kept unmodified. Source:
/// https://hl7.org/fhir/R4/fhir.schema.json.zip, licensed CC0 (see NOTICE).
pub const BUNDLED_SCHEMA: &str = include_str!("../../schemas/fhir-r4/fhir.schema.json");

/// Longest message kept per issue. oneOf messages can embed whole resources.
const MAX_MESSAGE: usize = 300;
/// Most errors read from the validator per bundle.
const MAX_ERRORS: usize = 200;

/// A compiled schema validator. Compile once per run.
pub struct SchemaValidator {
    validator: jsonschema::Validator,
    /// The schema's `definitions`, for per-resource-type validators.
    definitions: Value,
    /// Per-type validators, compiled on first use (see `drill_down`).
    by_type: Mutex<BTreeMap<String, Option<jsonschema::Validator>>>,
    pub source: String,
}

/// jsonschema reports a failed `oneOf` (the R4 schema's resource list) as one
/// error at the outermost resource, with the whole instance as its message.
fn is_one_of_failure(message: &str) -> bool {
    message.contains("is not valid under any of the schemas listed in the 'oneOf' keyword")
        || message.contains("is not valid under any of the given schemas")
}

impl SchemaValidator {
    /// The bundled R4 schema.
    pub fn bundled() -> Result<Self> {
        Self::from_text(BUNDLED_SCHEMA, "bundled R4 fhir.schema.json")
    }

    /// A schema file on disk (for example a newer copy of the same schema).
    pub fn from_path(path: &Path) -> Result<Self> {
        let text = read_text(path, 64 * 1024 * 1024)?;
        Self::from_text(&text, &path.display().to_string())
    }

    fn from_text(text: &str, source: &str) -> Result<Self> {
        let schema: Value = serde_json::from_str(text)
            .map_err(|e| Error::internal(format!("cannot parse {source}: {e}")))?;
        let validator = jsonschema::draft6::new(&schema)
            .map_err(|e| Error::internal(format!("cannot compile {source}: {e}")))?;
        let definitions = schema.get("definitions").cloned().unwrap_or(Value::Null);
        Ok(Self {
            validator,
            definitions,
            by_type: Mutex::new(BTreeMap::new()),
            source: source.to_string(),
        })
    }

    /// Errors for one resource against its own type's definition, with
    /// pointers prefixed by `prefix`. `None` when there is no definition.
    fn type_issues(&self, resource: &Value, prefix: &str) -> Option<Vec<Issue>> {
        let ty = resource.get("resourceType").and_then(Value::as_str)?;
        self.definitions.get(ty)?;
        let mut cache = self.by_type.lock().ok()?;
        let v = cache.entry(ty.to_string()).or_insert_with(|| {
            let schema = serde_json::json!({
                "$ref": format!("#/definitions/{ty}"),
                "definitions": self.definitions,
            });
            jsonschema::draft6::new(&schema).ok()
        });
        let v = v.as_ref()?;
        Some(
            v.iter_errors(resource)
                .take(MAX_ERRORS)
                .map(|e| Issue {
                    path: format!("{prefix}{}", e.instance_path().as_str()),
                    message: short(&e.to_string(), ty),
                })
                .collect(),
        )
    }

    /// Replace a generic oneOf failure with the errors of the resources
    /// inside it: `/entry/5/resource "code" is a required property` rather
    /// than the whole bundle as text.
    fn drill_down(&self, bundle: &Value, path: &str) -> Vec<Issue> {
        let target = if path.is_empty() {
            bundle
        } else {
            match bundle.pointer(path) {
                Some(v) => v,
                None => return Vec::new(),
            }
        };
        let mut out = Vec::new();
        if target.get("resourceType").and_then(Value::as_str) == Some("Bundle") {
            for (i, e) in crate::bundle::entries(target).iter().enumerate() {
                if let Some(r) = e.get("resource") {
                    let prefix = format!("{path}/entry/{i}/resource");
                    if let Some(issues) = self.type_issues(r, &prefix) {
                        out.extend(issues);
                    }
                }
            }
        }
        if let Some(issues) = self.type_issues(target, path) {
            out.extend(
                issues
                    .into_iter()
                    .filter(|i| !is_one_of_failure(&i.message)),
            );
        }
        out
    }

    /// Validate a bundle against the whole schema.
    pub fn validate(&self, bundle: &Value) -> LayerResult {
        if self.validator.is_valid(bundle) {
            return LayerResult::valid();
        }
        // Collecting errors against the whole schema starts at its root
        // oneOf, and jsonschema then gathers the errors of all ~150 branches
        // against the whole bundle. One invalid case of a 4,900-resource
        // bundle took 101 s that way. Checking the Bundle against its own
        // definition and each resource against its type gives the same
        // issues without that.
        if bundle.get("resourceType").and_then(Value::as_str) == Some("Bundle") {
            let found = self.drill_down(bundle, "");
            if !found.is_empty() {
                return LayerResult::from_issues(found);
            }
        }
        let mut issues: Vec<Issue> = Vec::new();
        for e in self.validator.iter_errors(bundle).take(MAX_ERRORS) {
            let path = e.instance_path().as_str().to_string();
            let message = e.to_string();
            if is_one_of_failure(&message) {
                let found = self.drill_down(bundle, &path);
                if !found.is_empty() {
                    issues.extend(found);
                    continue;
                }
            }
            let ty = bundle
                .pointer(&path)
                .and_then(|v| v.get("resourceType"))
                .and_then(Value::as_str)
                .unwrap_or("resource");
            issues.push(Issue {
                path,
                message: short(&message, ty),
            });
        }
        LayerResult::from_issues(issues)
    }
}

/// A readable issue message: a oneOf failure names the type instead of
/// quoting the instance, and long messages are cut.
fn short(message: &str, ty: &str) -> String {
    if is_one_of_failure(message) {
        return format!("does not match the R4 schema for {ty}");
    }
    if message.chars().count() > MAX_MESSAGE {
        message.chars().take(MAX_MESSAGE).collect::<String>() + "..."
    } else {
        message.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::LayerStatus;
    use serde_json::json;

    fn obs(extra: Value) -> Value {
        let mut o = json!({
            "resourceType": "Observation",
            "id": "o1",
            "status": "final",
            "code": {"coding": [{"system": "https://example.org/recordchaos/codes", "code": "synthetic-glucose"}]}
        });
        if let (Some(m), Value::Object(e)) = (o.as_object_mut(), extra) {
            for (k, v) in e {
                m.insert(k, v);
            }
        }
        json!({"resourceType": "Bundle", "type": "collection", "entry": [{"fullUrl": "https://example.org/fhir/Observation/o1", "resource": o}]})
    }

    #[test]
    fn valid_bundle_passes() {
        let v = SchemaValidator::bundled().unwrap();
        assert_eq!(v.validate(&obs(json!({}))).status, LayerStatus::Valid);
    }

    #[test]
    fn missing_code_fails() {
        let v = SchemaValidator::bundled().unwrap();
        let mut b = obs(json!({}));
        if let Some(r) = b
            .pointer_mut("/entry/0/resource")
            .and_then(Value::as_object_mut)
        {
            r.shift_remove("code");
        }
        let r = v.validate(&b);
        assert_eq!(r.status, LayerStatus::Invalid, "{:?}", r.issues);
        // The issue points at the resource and names the missing member,
        // not the whole bundle quoted as text.
        assert!(
            r.issues
                .iter()
                .any(|i| i.path == "/entry/0/resource" && i.message.contains("code")),
            "{:?}",
            r.issues
        );
        assert!(
            r.issues
                .iter()
                .all(|i| !i.message.contains("resourceType\":"))
        );
    }

    #[test]
    fn missing_status_passes_the_schema() {
        // The R4 JSON schema does not require Observation.status.
        let v = SchemaValidator::bundled().unwrap();
        let mut b = obs(json!({}));
        if let Some(r) = b
            .pointer_mut("/entry/0/resource")
            .and_then(Value::as_object_mut)
        {
            r.shift_remove("status");
        }
        assert_eq!(v.validate(&b).status, LayerStatus::Valid);
    }

    #[test]
    fn date_only_effective_is_valid_but_date_only_issued_is_not() {
        let v = SchemaValidator::bundled().unwrap();
        assert_eq!(
            v.validate(&obs(json!({"effectiveDateTime": "2026-01-04"})))
                .status,
            LayerStatus::Valid
        );
        assert_eq!(
            v.validate(&obs(json!({"issued": "2026-01-04"}))).status,
            LayerStatus::Invalid
        );
    }

    #[test]
    fn unknown_member_fails() {
        let v = SchemaValidator::bundled().unwrap();
        assert_eq!(
            v.validate(&obs(json!({"notAField": 1}))).status,
            LayerStatus::Invalid
        );
    }
}
