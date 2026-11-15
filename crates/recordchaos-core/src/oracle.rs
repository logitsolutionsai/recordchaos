//! Oracles: named checks over the observed view. Built-in oracles plus
//! user-declared comparisons; no expression language and no code execution.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::json::{display_value, fnv1a64, values_equal};
use crate::operators::{Effect, EffectKind, fill, find};
use crate::scenario::{AppliesTo, CheckKind, UserOracle};
use crate::target::{AcceptRules, Execution, StepKind, StepOutcome, View, ViewEntry};

pub const NO_CRASH: &str = "no_crash";
pub const INVALID_REJECTED: &str = "invalid_rejected";
pub const SAME_AS_BASE: &str = "same_as_base";
pub const LATEST_CORRECTION_SELECTED: &str = "latest_correction_selected";
pub const NO_CROSS_PATIENT_MERGE: &str = "no_cross_patient_merge";

/// Built-in oracles in priority order: the first failing one is the case's
/// primary failure.
pub const BUILT_IN: [&str; 5] = [
    NO_CRASH,
    INVALID_REJECTED,
    NO_CROSS_PATIENT_MERGE,
    LATEST_CORRECTION_SELECTED,
    SAME_AS_BASE,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OracleStatus {
    Pass,
    Fail,
    NotChecked,
}

/// Result of one oracle on one case.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OracleResult {
    pub oracle: String,
    pub status: OracleStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub got: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Stable text that identifies how it failed, for the fingerprint.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,
    /// True for user-declared oracles.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub user: bool,
}

impl OracleResult {
    fn new(oracle: &str, status: OracleStatus) -> Self {
        Self {
            oracle: oracle.to_string(),
            status,
            expected: None,
            got: None,
            detail: None,
            key: String::new(),
            user: false,
        }
    }

    fn pass(oracle: &str) -> Self {
        Self::new(oracle, OracleStatus::Pass)
    }

    fn not_checked(oracle: &str, why: impl Into<String>) -> Self {
        let mut r = Self::new(oracle, OracleStatus::NotChecked);
        r.detail = Some(why.into());
        r
    }

    fn fail(
        oracle: &str,
        expected: impl Into<String>,
        got: impl Into<String>,
        key: impl Into<String>,
    ) -> Self {
        let mut r = Self::new(oracle, OracleStatus::Fail);
        r.expected = Some(expected.into());
        r.got = Some(got.into());
        r.key = key.into();
        r
    }

    pub fn failed(&self) -> bool {
        self.status == OracleStatus::Fail
    }
}

/// One mutation as the oracles see it.
#[derive(Clone, Copy, Debug)]
pub struct MutationView<'a> {
    pub operator: &'a str,
    pub effect: &'a Effect,
}

/// Everything the oracles need for one case.
pub struct OracleInput<'a> {
    pub mutations: &'a [MutationView<'a>],
    /// The case should be rejected or flagged.
    pub invalid: bool,
    pub exec: &'a Execution,
    pub baseline: &'a View,
    pub spec: &'a BTreeMap<String, ViewEntry>,
    pub accept: &'a AcceptRules,
    pub user: &'a [UserOracle],
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Null => false,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn flagged(input: &OracleInput<'_>) -> bool {
    input
        .accept
        .flag_view
        .as_ref()
        .and_then(|name| input.exec.view.get(name))
        .is_some_and(truthy)
}

fn got_crash(step: &StepOutcome) -> String {
    if step.first_line.is_empty() {
        step.code_text()
    } else {
        format!("{}: {}", step.code_text(), step.first_line)
    }
}

/// Does an observed value hold the corrected reading? `None` when the view
/// cannot tell (for example an id-only view when the id did not change).
pub fn holds_correction(observed: &Value, e: &Effect) -> Option<bool> {
    let id_ok = |s: &str| {
        e.id.as_deref()
            .is_some_and(|id| s == id || s.ends_with(&format!("/{id}")))
    };
    let val_ok = |x: &Value| e.value.as_ref().is_some_and(|w| values_equal(x, w));
    match observed {
        Value::String(s) => {
            if e.match_id {
                Some(id_ok(s))
            } else {
                None
            }
        }
        Value::Number(_) => Some(val_ok(observed)),
        Value::Object(m) => {
            let id = m.get("id").and_then(Value::as_str);
            let val = m
                .get("value")
                .or_else(|| m.get("valueQuantity").and_then(|q| q.get("value")))
                .filter(|v| v.is_number());
            match (id, val) {
                (_, Some(v)) if !e.match_id => Some(val_ok(v)),
                (Some(i), Some(v)) => Some(id_ok(i) && val_ok(v)),
                (None, Some(v)) => Some(val_ok(v)),
                (Some(i), None) if e.match_id => Some(id_ok(i)),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Keys of `latest_*` view entries for a code: the pointer or GET path
/// mentions the code, or the name's suffix is part of it (`latest_glucose`
/// for `synthetic-glucose`).
pub fn latest_keys_for(spec: &BTreeMap<String, ViewEntry>, code: &str) -> Vec<String> {
    spec.iter()
        .filter(|(name, entry)| {
            if !name.starts_with("latest") {
                return false;
            }
            let suffix = name
                .trim_start_matches("latest")
                .trim_start_matches(['_', '-']);
            entry.locator().contains(code) || (suffix.len() >= 3 && code.contains(suffix))
        })
        .map(|(name, _)| name.clone())
        .collect()
}

fn describe_expected(e: &Effect) -> String {
    let value = e.value.as_ref().map(display_value).unwrap_or_default();
    let unit = e
        .unit
        .as_deref()
        .map(|u| format!(" {u}"))
        .unwrap_or_default();
    let id = e.id.as_deref().unwrap_or("?");
    match &e.issued {
        Some(i) => format!("{value}{unit} from Observation/{id}, issued {i}"),
        None => format!("{value}{unit} from Observation/{id}"),
    }
}

/// A view value in words. A reading object (`{"id": .., "value": ..}`, or an
/// Observation with `valueQuantity`) reads as `7.2 mmol/L from
/// Observation/obs-glucose-3`; anything else is shown as compact JSON.
fn describe_value(v: &Value) -> String {
    if let Value::Object(m) = v {
        let quantity = m.get("valueQuantity");
        let value = m.get("value").filter(|x| x.is_number()).or_else(|| {
            quantity
                .and_then(|q| q.get("value"))
                .filter(|x| x.is_number())
        });
        if let Some(value) = value {
            let unit = m
                .get("unit")
                .or_else(|| quantity.and_then(|q| q.get("unit").or_else(|| q.get("code"))))
                .and_then(Value::as_str)
                .map(|u| format!(" {u}"))
                .unwrap_or_default();
            let from = match m.get("id").and_then(Value::as_str) {
                Some(id) if id.contains('/') => format!(" from {id}"),
                Some(id) => {
                    let ty = m
                        .get("resourceType")
                        .and_then(Value::as_str)
                        .unwrap_or("Observation");
                    format!(" from {ty}/{id}")
                }
                None => String::new(),
            };
            return format!("{}{unit}{from}", display_value(value));
        }
    }
    display_value(v)
}

fn describe_view(keys: &[String], view: &View) -> String {
    keys.iter()
        .map(|k| match view.get(k) {
            Some(v) => format!("{k} = {}", describe_value(v)),
            None => format!("{k} absent"),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Run every applicable oracle on one case.
pub fn evaluate(input: &OracleInput<'_>) -> Vec<OracleResult> {
    let mut out = Vec::new();
    let exec = input.exec;
    let step = exec.deciding();
    let crashed = exec.crashed();
    let rejected = exec.rejected();
    let unpredictable: Vec<&str> = input
        .mutations
        .iter()
        .filter(|m| m.effect.kind == EffectKind::NoCrashOnly)
        .map(|m| m.operator)
        .collect();

    // no_crash: every case.
    if crashed {
        let expected = if step.http {
            "a 2xx or 4xx response"
        } else {
            "exit 0 or a rejected exit code"
        };
        out.push(OracleResult::fail(NO_CRASH, expected, got_crash(step), ""));
    } else {
        out.push(OracleResult::pass(NO_CRASH));
    }

    // invalid_rejected: invalid cases.
    if input.invalid {
        if crashed {
            out.push(OracleResult::not_checked(
                INVALID_REJECTED,
                "the importer crashed; see no_crash",
            ));
        } else if rejected {
            let mut r = OracleResult::pass(INVALID_REJECTED);
            let oo = if step.operation_outcome {
                " with an OperationOutcome"
            } else {
                ""
            };
            r.detail = Some(format!("rejected, {}{oo}", step.code_text()));
            out.push(r);
        } else if flagged(input) {
            let mut r = OracleResult::pass(INVALID_REJECTED);
            r.detail = Some(String::from("accepted and flagged"));
            out.push(r);
        } else {
            out.push(OracleResult::fail(
                INVALID_REJECTED,
                "rejected, or flagged at a declared view pointer",
                format!("accepted ({})", step.code_text()),
                "accepted",
            ));
        }
    }

    // no_cross_patient_merge: operator 18.
    if input
        .mutations
        .iter()
        .any(|m| m.effect.kind == EffectKind::CrossPatient)
    {
        let keys: Vec<String> = input
            .spec
            .keys()
            .filter(|k| k.contains("patient"))
            .cloned()
            .collect();
        if crashed {
            out.push(OracleResult::not_checked(
                NO_CROSS_PATIENT_MERGE,
                "the importer crashed; see no_crash",
            ));
        } else if rejected {
            out.push(OracleResult::pass(NO_CROSS_PATIENT_MERGE));
        } else if keys.is_empty() {
            out.push(OracleResult::not_checked(
                NO_CROSS_PATIENT_MERGE,
                "no view entry with \"patient\" in its name",
            ));
        } else {
            let differ: Vec<String> = keys
                .iter()
                .filter(|k| !opt_equal(input.baseline.get(*k), exec.view.get(*k)))
                .cloned()
                .collect();
            if differ.is_empty() {
                out.push(OracleResult::pass(NO_CROSS_PATIENT_MERGE));
            } else {
                out.push(OracleResult::fail(
                    NO_CROSS_PATIENT_MERGE,
                    describe_view(&differ, input.baseline),
                    describe_view(&differ, &exec.view),
                    format!("merged:{}", differ.join(",")),
                ));
            }
        }
    }

    // The checks below apply to valid cases only.
    if !input.invalid {
        // Last correction per code wins.
        let mut corrections: BTreeMap<String, &Effect> = BTreeMap::new();
        for m in input.mutations {
            if m.effect.kind == EffectKind::LatestCorrection {
                if let Some(code) = &m.effect.code {
                    corrections.insert(code.clone(), m.effect);
                }
            }
        }
        let mut tainted: Vec<String> = Vec::new();
        for (code, effect) in &corrections {
            let keys = latest_keys_for(input.spec, code);
            tainted.extend(keys.iter().cloned());
            if !unpredictable.is_empty() {
                out.push(OracleResult::not_checked(
                    LATEST_CORRECTION_SELECTED,
                    format!(
                        "{} changes the view in ways this check cannot predict",
                        unpredictable.join(", ")
                    ),
                ));
                continue;
            }
            if keys.is_empty() {
                out.push(OracleResult::not_checked(
                    LATEST_CORRECTION_SELECTED,
                    format!("no latest_* view entry mentions {code}"),
                ));
            } else if crashed {
                out.push(OracleResult::not_checked(
                    LATEST_CORRECTION_SELECTED,
                    "the importer crashed; see no_crash",
                ));
            } else if rejected {
                out.push(OracleResult::fail(
                    LATEST_CORRECTION_SELECTED,
                    describe_expected(effect),
                    format!("the bundle was rejected ({})", step.code_text()),
                    "rejected",
                ));
            } else {
                let mut bad = Vec::new();
                let mut unknown = Vec::new();
                for k in &keys {
                    match exec.view.get(k).map(|v| holds_correction(v, effect)) {
                        Some(Some(true)) => {}
                        Some(None) => unknown.push(k.clone()),
                        Some(Some(false)) | None => bad.push(k.clone()),
                    }
                }
                if !bad.is_empty() {
                    out.push(OracleResult::fail(
                        LATEST_CORRECTION_SELECTED,
                        describe_expected(effect),
                        describe_view(&bad, &exec.view),
                        format!("stale:{}", bad.join(",")),
                    ));
                } else if !unknown.is_empty() {
                    out.push(OracleResult::not_checked(
                        LATEST_CORRECTION_SELECTED,
                        format!("{} does not show the value or a new id", unknown.join(", ")),
                    ));
                } else {
                    out.push(OracleResult::pass(LATEST_CORRECTION_SELECTED));
                }
            }
        }

        // same_as_base: when a meaning-preserving mutation is present.
        let keeps: Vec<&MutationView<'_>> = input
            .mutations
            .iter()
            .filter(|m| {
                matches!(
                    m.effect.kind,
                    EffectKind::SameAsBase | EffectKind::SameAsBaseOrRejected
                )
            })
            .collect();
        if !keeps.is_empty() {
            let may_reject = keeps
                .iter()
                .all(|m| m.effect.kind == EffectKind::SameAsBaseOrRejected);
            let keys: Vec<String> = input
                .spec
                .keys()
                .filter(|k| {
                    !tainted.contains(*k) && input.accept.flag_view.as_deref() != Some(k.as_str())
                })
                .cloned()
                .collect();
            if !unpredictable.is_empty() {
                out.push(OracleResult::not_checked(
                    SAME_AS_BASE,
                    format!(
                        "{} changes the view in ways this check cannot predict",
                        unpredictable.join(", ")
                    ),
                ));
            } else if crashed {
                out.push(OracleResult::not_checked(
                    SAME_AS_BASE,
                    "the importer crashed; see no_crash",
                ));
            } else if rejected {
                if may_reject {
                    let mut r = OracleResult::pass(SAME_AS_BASE);
                    r.detail = Some(format!(
                        "rejected cleanly ({}), which this operator allows",
                        step.code_text()
                    ));
                    out.push(r);
                } else {
                    out.push(OracleResult::fail(
                        SAME_AS_BASE,
                        "accepted, with the same view as the base bundle",
                        format!("the bundle was rejected ({})", step.code_text()),
                        "rejected",
                    ));
                }
            } else if keys.is_empty() {
                out.push(OracleResult::not_checked(
                    SAME_AS_BASE,
                    "no view entries left to compare",
                ));
            } else {
                let differ: Vec<String> = keys
                    .iter()
                    .filter(|k| !opt_equal(input.baseline.get(*k), exec.view.get(*k)))
                    .cloned()
                    .collect();
                if differ.is_empty() {
                    out.push(OracleResult::pass(SAME_AS_BASE));
                } else {
                    out.push(OracleResult::fail(
                        SAME_AS_BASE,
                        describe_view(&differ, input.baseline),
                        describe_view(&differ, &exec.view),
                        format!("differs:{}", differ.join(",")),
                    ));
                }
            }
        }
    }

    // User-declared oracles.
    for u in input.user {
        let applies = match u.applies_to {
            AppliesTo::All => true,
            AppliesTo::Valid => !input.invalid,
            AppliesTo::Invalid => input.invalid,
        };
        if !applies {
            continue;
        }
        let mut r = if crashed {
            OracleResult::not_checked(&u.id, "the importer crashed; see no_crash")
        } else if rejected {
            OracleResult::not_checked(&u.id, "the bundle was rejected")
        } else {
            user_check(u, &exec.view, input.baseline)
        };
        r.user = true;
        out.push(r);
    }
    out
}

fn opt_equal(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => values_equal(x, y),
        _ => false,
    }
}

fn user_check(u: &UserOracle, view: &View, baseline: &View) -> OracleResult {
    let now = view.get(&u.view);
    let want = u.value.as_ref();
    let shown = |v: Option<&Value>| {
        v.map(display_value)
            .unwrap_or_else(|| String::from("absent"))
    };
    let (ok, expected) = match u.check {
        CheckKind::Equals => (opt_equal(now, want), shown(want)),
        CheckKind::NotEquals => (!opt_equal(now, want), format!("not {}", shown(want))),
        CheckKind::Exists => (now.is_some_and(|v| !v.is_null()), String::from("present")),
        CheckKind::Absent => (now.is_none_or(Value::is_null), String::from("absent")),
        CheckKind::CountEquals => {
            let count = now.and_then(|v| match v {
                Value::Array(a) => Some(a.len() as u64),
                Value::Object(o) => Some(o.len() as u64),
                _ => None,
            });
            let target = want.and_then(Value::as_u64);
            (
                count.is_some() && count == target,
                format!("count {}", shown(want)),
            )
        }
        CheckKind::SameAsBase => (
            opt_equal(now, baseline.get(&u.view)),
            shown(baseline.get(&u.view)),
        ),
    };
    if ok {
        OracleResult::pass(&u.id)
    } else {
        OracleResult::fail(
            &u.id,
            format!("{} {expected}", u.view),
            format!("{} {}", u.view, shown(now)),
            format!("user:{}", u.id),
        )
    }
}

/// The case's primary failure: the first failing oracle in priority order.
pub fn primary_failure(results: &[OracleResult]) -> Option<&OracleResult> {
    for name in BUILT_IN {
        if let Some(r) = results.iter().find(|r| r.oracle == name && r.failed()) {
            return Some(r);
        }
    }
    results.iter().find(|r| r.user && r.failed())
}

/// Outcome kind of a failure.
pub fn failure_kind(r: &OracleResult) -> &'static str {
    if r.oracle == NO_CRASH {
        "crashed"
    } else {
        "rule_broken"
    }
}

/// Fingerprint: (oracle id, outcome kind, HTTP status class or exit code,
/// hash of the first line of the error). For crashes the error is the
/// target's first output line; for broken rules it is the oracle's key.
pub fn fingerprint(r: &OracleResult, exec: &Execution) -> String {
    let step = exec.deciding();
    let line = if r.oracle == NO_CRASH {
        step.first_line.as_str()
    } else {
        r.key.as_str()
    };
    format!(
        "{}|{}|{}|{:016x}",
        r.oracle,
        failure_kind(r),
        step.class(),
        fnv1a64(line)
    )
}

/// Condition clause for a mutation, e.g. "the entries were in a different order".
pub fn condition(operator: &str, args: &serde_json::Map<String, Value>) -> String {
    match find(operator) {
        Some(op) => fill(op.info.condition, args),
        None => operator.to_string(),
    }
}

/// The finding sentence, from templates owned by each oracle and operator.
/// `conditions` are the clauses of the relevant mutations.
pub fn sentence(
    r: &OracleResult,
    step: &StepOutcome,
    conditions: &[String],
    user: Option<&UserOracle>,
) -> String {
    let cond = if conditions.is_empty() {
        String::from("this case ran")
    } else {
        conditions.join(" and ")
    };
    match r.oracle.as_str() {
        NO_CRASH => {
            let how = match (step.kind, step.code, step.http) {
                (StepKind::Timeout, _, _) => String::from("timed out"),
                (_, Some(c), true) => format!("returned {c}"),
                (_, Some(c), false) => format!("exited with code {c}"),
                (_, None, true) => String::from("dropped the connection"),
                (_, None, false) => String::from("was killed by a signal"),
            };
            format!("The importer {how} when {cond}.")
        }
        INVALID_REJECTED => format!(
            "The importer accepted the bundle without rejecting or flagging it when {cond}."
        ),
        NO_CROSS_PATIENT_MERGE => {
            format!("The importer merged records across patients when {cond}.")
        }
        LATEST_CORRECTION_SELECTED => {
            if r.key == "rejected" {
                format!("The importer rejected a valid correction when {cond}.")
            } else if conditions.iter().any(|c| c.contains("preliminary")) {
                format!("The importer kept the preliminary reading when {cond}.")
            } else {
                format!("The importer kept the older reading when {cond}.")
            }
        }
        SAME_AS_BASE => {
            if r.key == "rejected" {
                format!("The importer rejected a valid bundle when {cond}.")
            } else {
                let keys = r
                    .key
                    .strip_prefix("differs:")
                    .unwrap_or("view")
                    .replace(',', " and ");
                format!("The importer reported a different {keys} when {cond}.")
            }
        }
        _ => match user.and_then(|u| u.sentence.clone()) {
            Some(s) => s,
            None => format!("The check {} failed when {cond}.", r.oracle),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn exec(kind: StepKind, code: i64, view: View) -> Execution {
        let step = StepOutcome {
            kind,
            code: Some(code),
            http: false,
            first_line: String::new(),
            operation_outcome: false,
            elapsed_ms: 0,
        };
        Execution {
            reset: StepOutcome::accepted(),
            ingests: vec![step],
            observe: Some(StepOutcome::accepted()),
            view,
        }
    }

    fn spec() -> BTreeMap<String, ViewEntry> {
        let mut s = BTreeMap::new();
        s.insert(
            String::from("latest_glucose"),
            ViewEntry {
                pointer: String::from("/latest/synthetic-glucose"),
                get: None,
            },
        );
        s.insert(
            String::from("patient_count"),
            ViewEntry {
                pointer: String::from("/patients/count"),
                get: None,
            },
        );
        s
    }

    fn view(latest: Value, count: i64) -> View {
        let mut v = View::new();
        v.insert(String::from("latest_glucose"), latest);
        v.insert(String::from("patient_count"), json!(count));
        v
    }

    fn correction() -> Effect {
        let mut e = Effect::of(EffectKind::LatestCorrection);
        e.code = Some(String::from("synthetic-glucose"));
        e.id = Some(String::from("obs-glucose-3-corrected"));
        e.value = Some(json!(6.1));
        e.match_id = true;
        e
    }

    #[test]
    fn view_readings_are_described_in_words() {
        assert_eq!(
            describe_value(&json!({"id": "obs-glucose-3", "value": 7.2})),
            "7.2 from Observation/obs-glucose-3"
        );
        assert_eq!(
            describe_value(&json!({"resourceType": "Observation", "id": "o1",
                "valueQuantity": {"value": 6.1, "unit": "mmol/L"}})),
            "6.1 mmol/L from Observation/o1"
        );
        assert_eq!(describe_value(&json!(3)), "3");
        assert_eq!(describe_value(&json!({"a": 1})), r#"{"a":1}"#);
    }

    #[test]
    fn stale_latest_fails_and_fresh_passes() {
        let base = view(json!({"id": "obs-glucose-3", "value": 7.2}), 1);
        let effect = correction();
        let muts = [MutationView {
            operator: "observation.duplicate_with_changed_issued",
            effect: &effect,
        }];
        let accept = AcceptRules::default();
        let s = spec();
        let stale = exec(StepKind::Accepted, 0, base.clone());
        let input = OracleInput {
            mutations: &muts,
            invalid: false,
            exec: &stale,
            baseline: &base,
            spec: &s,
            accept: &accept,
            user: &[],
        };
        let r = evaluate(&input);
        let p = primary_failure(&r).unwrap();
        assert_eq!(p.oracle, LATEST_CORRECTION_SELECTED);
        let fresh = exec(
            StepKind::Accepted,
            0,
            view(json!({"id": "obs-glucose-3-corrected", "value": 6.1}), 1),
        );
        let input = OracleInput {
            exec: &fresh,
            ..input
        };
        assert!(primary_failure(&evaluate(&input)).is_none());
    }

    #[test]
    fn invalid_cases_must_be_rejected() {
        let base = view(json!({"id": "obs-glucose-3", "value": 7.2}), 1);
        let effect = Effect::of(EffectKind::Invalid);
        let muts = [MutationView {
            operator: "reference.dangling_target",
            effect: &effect,
        }];
        let accept = AcceptRules {
            rejected_exit_codes: vec![2],
            flag_view: None,
        };
        let s = spec();
        let accepted = exec(StepKind::Accepted, 0, base.clone());
        let input = OracleInput {
            mutations: &muts,
            invalid: true,
            exec: &accepted,
            baseline: &base,
            spec: &s,
            accept: &accept,
            user: &[],
        };
        assert_eq!(
            primary_failure(&evaluate(&input)).unwrap().oracle,
            INVALID_REJECTED
        );
        let rejected = exec(StepKind::Rejected, 2, View::new());
        let input = OracleInput {
            exec: &rejected,
            ..input
        };
        assert!(primary_failure(&evaluate(&input)).is_none());
    }

    #[test]
    fn crash_is_primary_and_fingerprinted() {
        let base = view(json!(7.2), 1);
        let effect = Effect::of(EffectKind::SameAsBase);
        let muts = [MutationView {
            operator: "reference.to_urn_uuid",
            effect: &effect,
        }];
        let accept = AcceptRules {
            rejected_exit_codes: vec![2],
            flag_view: None,
        };
        let s = spec();
        let mut crashed = exec(StepKind::Crashed, 101, View::new());
        crashed.ingests[0].first_line = String::from("thread 'main' panicked");
        crashed.observe = None;
        let input = OracleInput {
            mutations: &muts,
            invalid: false,
            exec: &crashed,
            baseline: &base,
            spec: &s,
            accept: &accept,
            user: &[],
        };
        let r = evaluate(&input);
        let p = primary_failure(&r).unwrap();
        assert_eq!(p.oracle, NO_CRASH);
        let fp = fingerprint(p, &crashed);
        assert!(fp.starts_with("no_crash|crashed|exit:101|"), "{fp}");
        let text = sentence(
            p,
            crashed.deciding(),
            &[String::from("the patient reference used a urn:uuid")],
            None,
        );
        assert_eq!(
            text,
            "The importer exited with code 101 when the patient reference used a urn:uuid."
        );
    }

    #[test]
    fn reorder_changes_are_caught() {
        let base = view(json!({"id": "obs-glucose-3", "value": 7.2}), 1);
        let effect = Effect::of(EffectKind::SameAsBase);
        let muts = [MutationView {
            operator: "bundle.reorder_entries",
            effect: &effect,
        }];
        let accept = AcceptRules::default();
        let s = spec();
        let changed = exec(
            StepKind::Accepted,
            0,
            view(json!({"id": "obs-glucose-1", "value": 6.4}), 1),
        );
        let input = OracleInput {
            mutations: &muts,
            invalid: false,
            exec: &changed,
            baseline: &base,
            spec: &s,
            accept: &accept,
            user: &[],
        };
        let r = evaluate(&input);
        let p = primary_failure(&r).unwrap();
        assert_eq!(p.oracle, SAME_AS_BASE);
        assert_eq!(p.key, "differs:latest_glucose");
        let text = sentence(
            p,
            changed.deciding(),
            &[String::from("the entries were in a different order")],
            None,
        );
        assert_eq!(
            text,
            "The importer reported a different latest_glucose when the entries were in a different order."
        );
    }

    #[test]
    fn holds_correction_reads_common_shapes() {
        let e = correction();
        assert_eq!(
            holds_correction(&json!("obs-glucose-3-corrected"), &e),
            Some(true)
        );
        assert_eq!(
            holds_correction(&json!("Observation/obs-glucose-3"), &e),
            Some(false)
        );
        assert_eq!(holds_correction(&json!(6.1), &e), Some(true));
        assert_eq!(
            holds_correction(&json!({"valueQuantity": {"value": 6.1}}), &e),
            Some(true)
        );
        assert_eq!(holds_correction(&json!(true), &e), None);
    }
}
