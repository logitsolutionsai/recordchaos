//! The 22 mutation operators.
//!
//! Each operator is a pure function: it takes the current bundle, a target and
//! its arguments, and returns a new bundle plus the JSON Patch it applied and an
//! [`Effect`] that tells the oracles what should happen. It never mutates its
//! input. When it finds nothing to change it returns [`NotApplicable`].

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::bundle::{PatchError, PatchOp, entries, first_code, has_code, resource, resource_key};
use crate::refs::RefGraph;
use crate::rng::CaseRng;
use crate::time;
use crate::validate::Layer;

pub mod bundle_ops;
pub mod identifier;
pub mod medication;
pub mod observation;
pub mod quantity;
pub mod reference;
pub mod registry;
pub mod resource_ops;
pub mod text;
pub mod time_ops;

pub use registry::{REGISTRY, find, markdown_doc};

/// What an operator means to produce.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    /// Valid FHIR that is awkward for importers.
    ValidEdge,
    /// Valid FHIR with the same meaning as the base.
    ValidMeaningKept,
    /// Invalid FHIR that an importer should reject or flag.
    Invalid,
}

impl Intent {
    pub fn label(self) -> &'static str {
        match self {
            Intent::ValidEdge => "Valid, edge case",
            Intent::ValidMeaningKept => "Valid, no change in meaning",
            Intent::Invalid => "Invalid",
        }
    }

    pub fn is_invalid(self) -> bool {
        self == Intent::Invalid
    }
}

/// What the oracles should expect from the target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectKind {
    /// Every declared view pointer equals the baseline.
    SameAsBase,
    /// Same as base, or rejected cleanly.
    SameAsBaseOrRejected,
    /// The `latest_*` pointer for a code holds the corrected resource.
    LatestCorrection,
    /// The target should reject or flag the case.
    Invalid,
    /// Invalid, and patient records must never be merged.
    CrossPatient,
    /// Only `no_crash` can be predicted.
    NoCrashOnly,
}

/// The effect of one mutation on the view.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Effect {
    pub kind: EffectKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issued: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_value: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_issued: Option<String>,
    /// True when the corrected resource has a new id that the view may hold.
    #[serde(default)]
    pub match_id: bool,
}

impl Effect {
    pub fn of(kind: EffectKind) -> Self {
        Self {
            kind,
            code: None,
            id: None,
            value: None,
            unit: None,
            issued: None,
            old_id: None,
            old_value: None,
            old_issued: None,
            match_id: false,
        }
    }
}

/// Which resources an operator may change. From the scenario's `select`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Select {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
}

/// What a mutation is aimed at.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TargetRef {
    /// `Type/id` of the resource, or none for bundle-level operators.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Pointer inside the resource, for operators that change one reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl TargetRef {
    pub fn bundle() -> Self {
        Self {
            key: None,
            path: None,
        }
    }

    pub fn resource(key: impl Into<String>) -> Self {
        Self {
            key: Some(key.into()),
            path: None,
        }
    }

    pub fn site(key: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            key: Some(key.into()),
            path: Some(path.into()),
        }
    }

    pub fn label(&self) -> String {
        match (&self.key, &self.path) {
            (Some(k), Some(p)) => format!("{k}{p}"),
            (Some(k), None) => k.clone(),
            _ => String::from("bundle"),
        }
    }
}

/// Why an operator made no change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotApplicable(pub String);

impl From<PatchError> for NotApplicable {
    fn from(e: PatchError) -> Self {
        NotApplicable(e.to_string())
    }
}

/// The result of applying one operator.
#[derive(Clone, Debug)]
pub struct Applied {
    pub bundle: Value,
    pub patch: Vec<PatchOp>,
    pub effect: Effect,
    /// For two-step operators: a bundle to ingest before this one.
    pub pre_step: Option<Value>,
    /// Resource keys this mutation changed or created.
    pub touched: Vec<String>,
    /// One line describing what changed.
    pub summary: String,
    /// Arguments after defaults were filled in.
    pub args: Map<String, Value>,
}

/// Documentation for one argument.
#[derive(Clone, Copy, Debug)]
pub struct ArgInfo {
    pub name: &'static str,
    pub default: &'static str,
    pub help: &'static str,
}

/// Static description of an operator: the registry and docs/operators.md are
/// generated from these.
#[derive(Clone, Copy, Debug)]
pub struct OperatorInfo {
    pub number: u8,
    pub id: &'static str,
    pub version: u32,
    pub summary: &'static str,
    pub intent: Intent,
    pub caught_by: &'static [Layer],
    pub strict_rule: Option<&'static str>,
    pub view_effect: &'static str,
    pub eligibility: &'static str,
    pub paths: &'static str,
    pub reversible: &'static str,
    pub before: &'static str,
    pub after: &'static str,
    pub args: &'static [ArgInfo],
    /// Clause for finding sentences, e.g. "the entries were in a different order".
    /// `{name}` is replaced with the resolved argument.
    pub condition: &'static str,
}

/// Shared context for one operator application.
pub struct OpCtx<'a> {
    pub bundle: &'a Value,
    pub graph: RefGraph,
    /// The run's reference date (`--as-of`), `YYYY-MM-DD`.
    pub as_of: &'a str,
    /// Resource keys already changed by earlier mutations in this case.
    pub touched: &'a BTreeSet<String>,
}

impl<'a> OpCtx<'a> {
    pub fn new(bundle: &'a Value, as_of: &'a str, touched: &'a BTreeSet<String>) -> Self {
        Self {
            bundle,
            graph: RefGraph::build(bundle),
            as_of,
            touched,
        }
    }
}

pub type TargetsFn = fn(&OpCtx<'_>, &Select) -> Vec<TargetRef>;
pub type ApplyFn =
    fn(&OpCtx<'_>, &TargetRef, &Map<String, Value>, &mut CaseRng) -> Result<Applied, NotApplicable>;

/// One operator: its description, how it finds targets and how it applies.
#[derive(Clone, Copy)]
pub struct Operator {
    pub info: OperatorInfo,
    pub targets: TargetsFn,
    pub apply: ApplyFn,
}

impl std::fmt::Debug for Operator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Operator")
            .field("id", &self.info.id)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Helpers shared by the operator families.

pub(crate) fn na(msg: impl Into<String>) -> NotApplicable {
    NotApplicable(msg.into())
}

/// True when a resource matches the selection. `default_type` applies when the
/// scenario names no resource type.
pub(crate) fn matches(res: &Value, select: &Select, default_types: &[&str]) -> bool {
    let rt = res
        .get("resourceType")
        .and_then(Value::as_str)
        .unwrap_or("");
    match &select.resource_type {
        Some(t) => {
            if t != rt {
                return false;
            }
        }
        None => {
            if !default_types.is_empty() && !default_types.contains(&rt) {
                return false;
            }
        }
    }
    if let Some(code) = &select.code {
        let hit = has_code(res, "code", code) || has_code(res, "medicationCodeableConcept", code);
        if !hit {
            return false;
        }
    }
    true
}

/// Resources of the bundle as (entry index, resource, key), skipping entries
/// without a resource key and resources touched earlier in the case.
pub(crate) fn candidates<'a>(ctx: &'a OpCtx<'_>) -> Vec<(usize, &'a Value, String)> {
    let mut out = Vec::new();
    for (i, e) in entries(ctx.bundle).iter().enumerate() {
        let Some(res) = resource(e) else { continue };
        let Some(key) = resource_key(res) else {
            continue;
        };
        if ctx.touched.contains(&key) {
            continue;
        }
        out.push((i, res, key));
    }
    out
}

/// The target's resource key, or NotApplicable.
pub(crate) fn target_key(target: &TargetRef) -> Result<&str, NotApplicable> {
    target
        .key
        .as_deref()
        .ok_or_else(|| na("operator needs a resource target"))
}

/// Entry index and resource for a key.
pub(crate) fn locate<'a>(
    ctx: &'a OpCtx<'_>,
    key: &str,
) -> Result<(usize, &'a Value), NotApplicable> {
    let idx = ctx
        .graph
        .entry_of_key(key)
        .ok_or_else(|| na(format!("{key} is not in the bundle")))?;
    let res = entries(ctx.bundle)
        .get(idx)
        .and_then(resource)
        .ok_or_else(|| na(format!("{key} has no resource")))?;
    Ok((idx, res))
}

/// Set a member on an object value; no-op for non-objects.
pub(crate) fn set_member(target: &mut Value, key: &str, value: Value) {
    if let Some(map) = target.as_object_mut() {
        map.insert(key.to_string(), value);
    }
}

/// A resource id not yet used for this resource type.
pub(crate) fn unique_id(ctx: &OpCtx<'_>, resource_type: &str, wanted: &str) -> String {
    let taken = |id: &str| {
        ctx.graph
            .by_key
            .contains_key(&format!("{resource_type}/{id}"))
    };
    if !taken(wanted) {
        return wanted.to_string();
    }
    let mut n = 2;
    loop {
        let candidate = format!("{wanted}-{n}");
        if !taken(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// A fullUrl for a new resource next to an existing one.
pub(crate) fn sibling_full_url(
    full_url: Option<&str>,
    old_id: &str,
    new_id: &str,
    rng: &mut CaseRng,
) -> String {
    if let Some(f) = full_url {
        if let Some(prefix) = f.strip_suffix(old_id) {
            if prefix.ends_with('/') && f.contains("://") {
                return format!("{prefix}{new_id}");
            }
        }
    }
    format!("urn:uuid:{}", rng.uuid())
}

/// A new entry next to `entry`, with the given fullUrl and resource.
pub(crate) fn sibling_entry(entry: &Value, full_url: &str, res: Value) -> Value {
    let mut e = Map::new();
    e.insert(String::from("fullUrl"), Value::String(full_url.to_string()));
    e.insert(String::from("resource"), res.clone());
    if entry.get("request").is_some() {
        let rt = res
            .get("resourceType")
            .and_then(Value::as_str)
            .unwrap_or("");
        let id = res.get("id").and_then(Value::as_str).unwrap_or("");
        e.insert(
            String::from("request"),
            json!({"method": "PUT", "url": format!("{rt}/{id}")}),
        );
    }
    Value::Object(e)
}

/// The effective time of a resource, as written.
pub(crate) fn effective_text(res: &Value) -> Option<&str> {
    res.get("effectiveDateTime")
        .or_else(|| res.get("effectiveInstant"))
        .or_else(|| res.get("effectivePeriod").and_then(|p| p.get("start")))
        .and_then(Value::as_str)
}

/// Sort key for "latest": (effective, issued) in epoch seconds.
pub(crate) fn time_key(res: &Value) -> Option<(i64, i64)> {
    let eff = effective_text(res).and_then(time::parse)?;
    let issued = res
        .get("issued")
        .and_then(Value::as_str)
        .and_then(time::parse)
        .map(|t| t.epoch)
        .unwrap_or(i64::MIN);
    Some((eff.epoch, issued))
}

/// For each Observation code, the key of the latest Observation by
/// (effective, issued, entry order). Touched resources count too, so an
/// earlier correction stays the latest.
pub(crate) fn latest_by_code(bundle: &Value) -> BTreeMap<String, String> {
    let mut best: BTreeMap<String, ((i64, i64, usize), String)> = BTreeMap::new();
    for (i, e) in entries(bundle).iter().enumerate() {
        let Some(res) = resource(e) else { continue };
        if res.get("resourceType").and_then(Value::as_str) != Some("Observation") {
            continue;
        }
        let (Some(code), Some(key), Some((eff, iss))) =
            (first_code(res, "code"), resource_key(res), time_key(res))
        else {
            continue;
        };
        let rank = (eff, iss, i);
        let better = best.get(&code).is_none_or(|(r, _)| rank > *r);
        if better {
            best.insert(code, (rank, key));
        }
    }
    best.into_iter().map(|(c, (_, k))| (c, k)).collect()
}

/// Replace `{name}` placeholders with argument values.
pub fn fill(template: &str, args: &Map<String, Value>) -> String {
    let mut out = template.to_string();
    for (k, v) in args {
        let text = match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        out = out.replace(&format!("{{{k}}}"), &text);
    }
    out
}

/// Build a resolved argument map.
pub(crate) fn args_map(pairs: &[(&str, Value)]) -> Map<String, Value> {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert((*k).to_string(), v.clone());
    }
    m
}

/// The candidate value fields of an Observation (`valueQuantity` and friends).
pub(crate) fn value_field(res: &Value) -> Option<String> {
    res.as_object()?
        .keys()
        .find(|k| k.starts_with("value") && k.len() > 5)
        .cloned()
}
