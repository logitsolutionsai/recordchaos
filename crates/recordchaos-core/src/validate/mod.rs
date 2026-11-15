//! The three validation layers: the bundled R4 JSON schema, our reference
//! checker and the optional HL7 Java validator.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod references;
pub mod schema;
pub mod strict;

pub use schema::SchemaValidator;
pub use strict::StrictValidator;

/// A validation layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    Schema,
    References,
    Strict,
}

impl Layer {
    pub fn name(self) -> &'static str {
        match self {
            Layer::Schema => "schema",
            Layer::References => "references",
            Layer::Strict => "strict",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Layer::Schema => "R4 JSON schema 4.0.1",
            Layer::References => "References inside the bundle",
            Layer::Strict => "HL7 validator",
        }
    }
}

/// Status of one layer for one bundle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayerStatus {
    Valid,
    Invalid,
    NotChecked,
}

/// One problem found by a layer.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Issue {
    /// JSON Pointer into the bundle.
    pub path: String,
    pub message: String,
}

/// Result of one layer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerResult {
    pub status: LayerStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<Issue>,
    /// Why the layer did not run, when status is not_checked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Cap on issues kept per layer, so a broken bundle cannot flood results.json.
pub const MAX_ISSUES: usize = 25;

impl LayerResult {
    pub fn valid() -> Self {
        Self {
            status: LayerStatus::Valid,
            issues: Vec::new(),
            reason: None,
        }
    }

    pub fn invalid(mut issues: Vec<Issue>) -> Self {
        issues.sort();
        issues.dedup();
        issues.truncate(MAX_ISSUES);
        Self {
            status: LayerStatus::Invalid,
            issues,
            reason: None,
        }
    }

    pub fn not_checked(reason: impl Into<String>) -> Self {
        Self {
            status: LayerStatus::NotChecked,
            issues: Vec::new(),
            reason: Some(reason.into()),
        }
    }

    pub fn from_issues(issues: Vec<Issue>) -> Self {
        if issues.is_empty() {
            Self::valid()
        } else {
            Self::invalid(issues)
        }
    }

    /// Short text for reports: "Valid", "1 dangling", "Not run, add --strict".
    pub fn short(&self, layer: Layer) -> String {
        match self.status {
            LayerStatus::Valid => match layer {
                Layer::References => String::from("All resolve"),
                _ => String::from("Valid"),
            },
            LayerStatus::Invalid => {
                let n = self.issues.len();
                match layer {
                    Layer::References => format!("{n} broken"),
                    _ => format!("Invalid, {n} {}", if n == 1 { "issue" } else { "issues" }),
                }
            }
            LayerStatus::NotChecked => match layer {
                Layer::Strict => String::from("Not run, add --strict"),
                _ => String::from("Not checked"),
            },
        }
    }
}

/// Results of every layer for one bundle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Validation {
    pub schema: LayerResult,
    pub references: LayerResult,
    pub strict: LayerResult,
}

/// Overall verdict over the layers that ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Valid,
    Invalid,
    NotChecked,
}

impl Validation {
    pub fn get(&self, layer: Layer) -> &LayerResult {
        match layer {
            Layer::Schema => &self.schema,
            Layer::References => &self.references,
            Layer::Strict => &self.strict,
        }
    }

    pub fn verdict(&self) -> Verdict {
        let all = [&self.schema, &self.references, &self.strict];
        if all.iter().any(|l| l.status == LayerStatus::Invalid) {
            Verdict::Invalid
        } else if all.iter().any(|l| l.status == LayerStatus::Valid) {
            Verdict::Valid
        } else {
            Verdict::NotChecked
        }
    }
}

/// The validators for a run. The schema validator is compiled once.
pub struct Validators {
    pub schema: Option<SchemaValidator>,
    pub strict: Option<StrictValidator>,
    pub schema_reason: String,
    /// Strict results by the SHA-256 of a bundle's canonical text. One
    /// validator run costs a JVM start and a package load, so a bundle the
    /// run already checked is never sent again.
    strict_cache: Mutex<HashMap<String, LayerResult>>,
}

impl Validators {
    pub fn new(schema: Option<SchemaValidator>, strict: Option<StrictValidator>) -> Self {
        Self {
            schema,
            strict,
            schema_reason: String::from("schema layer turned off"),
            strict_cache: Mutex::new(HashMap::new()),
        }
    }

    /// The schema and reference layers, which are fast and run in-process.
    fn quick(&self, bundle: &Value) -> (LayerResult, LayerResult) {
        let schema = match &self.schema {
            Some(v) => v.validate(bundle),
            None => LayerResult::not_checked(self.schema_reason.clone()),
        };
        (schema, references::check(bundle))
    }

    pub fn validate(&self, bundle: &Value) -> Validation {
        let (schema, references) = self.quick(bundle);
        let strict = self
            .strict_many(&[bundle])
            .pop()
            .unwrap_or_else(|| LayerResult::not_checked("HL7 validator returned no result"));
        Validation {
            schema,
            references,
            strict,
        }
    }

    /// The schema and reference layers only, with the strict layer marked
    /// not checked. For checks that never read the strict result, such as
    /// the base-bundle check, so they do not cost a validator run.
    pub fn validate_quick(&self, bundle: &Value) -> Validation {
        let (schema, references) = self.quick(bundle);
        Validation {
            schema,
            references,
            strict: LayerResult::not_checked("HL7 validator not needed for this check"),
        }
    }

    /// Run the strict layer on these bundles now, in one validator run, so
    /// later calls to `validate` for them are answered from the cache.
    pub fn prefetch_strict(&self, bundles: &[&Value]) {
        if self.strict.is_some() {
            let _ = self.strict_many(bundles);
        }
    }

    /// Strict results for many bundles: cached ones from the cache, the rest
    /// (each distinct bundle once) in one validator run.
    fn strict_many(&self, bundles: &[&Value]) -> Vec<LayerResult> {
        let Some(strict) = &self.strict else {
            return vec![
                LayerResult::not_checked("HL7 validator not run, add --strict");
                bundles.len()
            ];
        };
        let keys: Vec<Option<String>> = bundles
            .iter()
            .map(|b| {
                crate::json::canonical(b)
                    .ok()
                    .map(|t| crate::json::sha256_hex(t.as_bytes()))
            })
            .collect();
        let mut todo: Vec<usize> = Vec::new();
        {
            let cache = self.strict_cache.lock().unwrap_or_else(|e| e.into_inner());
            let mut queued: HashSet<&str> = HashSet::new();
            // No let chains: they need Rust 1.88, and the MSRV is 1.85.
            for (i, key) in keys.iter().enumerate() {
                if let Some(k) = key {
                    if !cache.contains_key(k) && queued.insert(k) {
                        todo.push(i);
                    }
                }
            }
        }
        if !todo.is_empty() {
            let batch: Vec<&Value> = todo.iter().map(|i| bundles[*i]).collect();
            let fresh = strict.validate_many(&batch);
            let mut cache = self.strict_cache.lock().unwrap_or_else(|e| e.into_inner());
            for (i, layer) in todo.iter().zip(fresh) {
                if let Some(k) = &keys[*i] {
                    cache.insert(k.clone(), layer);
                }
            }
        }
        let cache = self.strict_cache.lock().unwrap_or_else(|e| e.into_inner());
        keys.iter()
            .map(|k| {
                k.as_ref()
                    .and_then(|k| cache.get(k).cloned())
                    .unwrap_or_else(|| {
                        LayerResult::not_checked(
                            "HL7 validator not run: cannot serialise the bundle",
                        )
                    })
            })
            .collect()
    }

    /// Validate many bundles, using up to `jobs` threads for the schema and
    /// reference layers and one validator run for the strict layer. Output
    /// order matches input order, so results stay deterministic.
    pub fn validate_all(&self, bundles: &[&Value], jobs: usize) -> Vec<Validation> {
        let quick = self.quick_all(bundles, jobs);
        let strict = self.strict_many(bundles);
        quick
            .into_iter()
            .zip(strict)
            .map(|((schema, references), strict)| Validation {
                schema,
                references,
                strict,
            })
            .collect()
    }

    fn quick_all(&self, bundles: &[&Value], jobs: usize) -> Vec<(LayerResult, LayerResult)> {
        let jobs = jobs.max(1);
        if jobs == 1 || bundles.len() < 2 {
            return bundles.iter().map(|b| self.quick(b)).collect();
        }
        let chunk = bundles.len().div_ceil(jobs);
        let mut out: Vec<(LayerResult, LayerResult)> = Vec::with_capacity(bundles.len());
        std::thread::scope(|scope| {
            let handles: Vec<_> = bundles
                .chunks(chunk)
                .map(|part| {
                    scope.spawn(move || part.iter().map(|b| self.quick(b)).collect::<Vec<_>>())
                })
                .collect();
            for h in handles {
                if let Ok(mut part) = h.join() {
                    out.append(&mut part);
                }
            }
        });
        if out.len() != bundles.len() {
            // A worker panicked; fall back to one thread so no case is lost.
            return bundles.iter().map(|b| self.quick(b)).collect();
        }
        out
    }
}

/// Map a FHIRPath location such as `Bundle.entry[2].resource.ofType(Observation).status`
/// to a JSON Pointer such as `/entry/2/resource/status`.
pub fn fhirpath_to_pointer(expr: &str) -> String {
    // Validator 7 writes the resource's type and id as a comment:
    // `Bundle.entry[4].resource/*Observation/obs-1*/.code`. Ids may hold
    // dots, so drop comments before splitting.
    let mut clean = String::new();
    let mut rest = expr;
    while let Some(start) = rest.find("/*") {
        clean.push_str(&rest[..start]);
        rest = match rest[start..].find("*/") {
            Some(end) => &rest[start + end + 2..],
            None => "",
        };
    }
    clean.push_str(rest);
    let body = clean.strip_prefix("Bundle").unwrap_or(&clean);
    let mut segs: Vec<String> = Vec::new();
    for part in body.split('.') {
        if part.is_empty() || part.starts_with("where(") {
            continue;
        }
        if let Some(ty) = part
            .strip_prefix("ofType(")
            .and_then(|p| p.strip_suffix(')'))
        {
            // `value.ofType(Quantity)` is the JSON name `valueQuantity`;
            // after `resource` it only names the resource type.
            if let Some(last) = segs.last_mut() {
                let is_index = last.chars().all(|c| c.is_ascii_digit());
                if last != "resource" && !is_index {
                    let mut chars = ty.chars();
                    if let Some(first) = chars.next() {
                        last.push(first.to_ascii_uppercase());
                        last.push_str(chars.as_str());
                    }
                }
            }
            continue;
        }
        match part.split_once('[') {
            Some((name, idx)) => {
                segs.push(name.to_string());
                segs.push(idx.trim_end_matches(']').to_string());
            }
            None => segs.push(part.to_string()),
        }
    }
    segs.iter().map(|s| format!("/{s}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fhirpath_maps_to_pointer() {
        assert_eq!(
            fhirpath_to_pointer("Bundle.entry[2].resource.ofType(Observation).status"),
            "/entry/2/resource/status"
        );
        assert_eq!(fhirpath_to_pointer("Bundle"), "");
    }

    #[test]
    fn fhirpath_from_validator_7_maps_to_pointer() {
        // Expressions as validator_cli.jar 7.0.6 writes them.
        assert_eq!(
            fhirpath_to_pointer(
                "Bundle.entry[4].resource/*Observation/obs-glucose-1*/.identifier[0].system"
            ),
            "/entry/4/resource/identifier/0/system"
        );
        assert_eq!(
            fhirpath_to_pointer(
                "Bundle.entry[11].resource/*MedicationRequest/medrq.a1*/.medication.ofType(CodeableConcept).coding[0].system"
            ),
            "/entry/11/resource/medicationCodeableConcept/coding/0/system"
        );
        assert_eq!(
            fhirpath_to_pointer("Bundle.entry[2].resource.ofType(Encounter).period"),
            "/entry/2/resource/period"
        );
    }

    #[test]
    fn strict_off_marks_every_case_not_checked() {
        let v = Validators::new(None, None);
        let a = serde_json::json!({"resourceType": "Bundle", "type": "collection", "entry": []});
        let all = v.validate_all(&[&a, &a, &a], 2);
        assert_eq!(all.len(), 3);
        for x in &all {
            assert_eq!(x.strict.status, LayerStatus::NotChecked);
            assert_eq!(x.strict.short(Layer::Strict), "Not run, add --strict");
        }
    }

    #[test]
    fn strict_results_are_cached_per_bundle() {
        // A Java that cannot start gives not_checked for every case; the
        // second call for the same bundle comes from the cache.
        let strict = StrictValidator {
            java: String::from("recordchaos-no-such-java"),
            jar: std::path::PathBuf::from("validator_cli.jar"),
            version: String::new(),
            timeout: std::time::Duration::from_secs(5),
        };
        let v = Validators::new(None, Some(strict));
        let a = serde_json::json!({"resourceType": "Bundle", "type": "collection", "entry": []});
        let b = serde_json::json!({"resourceType": "Bundle", "type": "batch", "entry": []});
        let all = v.validate_all(&[&a, &b, &a], 1);
        assert_eq!(all.len(), 3);
        assert!(
            all.iter()
                .all(|x| x.strict.status == LayerStatus::NotChecked)
        );
        assert_eq!(v.strict_cache.lock().map(|c| c.len()).unwrap_or(0), 2);
        assert_eq!(v.validate(&a).strict, all[0].strict);
        assert_eq!(v.validate_quick(&a).strict.status, LayerStatus::NotChecked);
    }

    #[test]
    fn verdict_ignores_layers_not_run() {
        let v = Validation {
            schema: LayerResult::valid(),
            references: LayerResult::valid(),
            strict: LayerResult::not_checked("not run"),
        };
        assert_eq!(v.verdict(), Verdict::Valid);
        assert_eq!(v.strict.short(Layer::Strict), "Not run, add --strict");
    }
}
