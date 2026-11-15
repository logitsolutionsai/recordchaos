//! Python bindings for recordchaos (module `recordchaos._core`).
//!
//! Everything crosses the boundary as JSON text, so the bindings need no
//! Python object conversion code: the pure-Python layer in
//! `python/recordchaos/` calls `json.dumps` and `json.loads`.

use std::collections::BTreeMap;
use std::path::Path;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use recordchaos_core::bundle::{self, without_entries};
use recordchaos_core::diff::line_diff;
use recordchaos_core::generate::{
    MutationRecord, case_intent, default_as_of, effect_is_invalid, generate, reapply,
};
use recordchaos_core::json::canonical;
use recordchaos_core::operators::{Intent, REGISTRY};
use recordchaos_core::oracle::{
    self, MutationView, OracleInput, OracleResult, fingerprint, primary_failure,
};
use recordchaos_core::results::{
    CaseResult, Finding, InputRef, LayersRun, Note, Outcome, RunResults, Summary, TargetInfo,
};
use recordchaos_core::run::{check_base, check_text, note_for, relevant_conditions};
use recordchaos_core::scenario::load_scenario;
use recordchaos_core::synthetic;
use recordchaos_core::target::{AcceptRules, Execution, StepKind, StepOutcome, View, ViewEntry};
use recordchaos_core::validate::{LayerStatus, SchemaValidator, Validators};
use recordchaos_core::{Limits, error::write_text};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

fn err<E: std::fmt::Display>(e: E) -> PyErr {
    PyValueError::new_err(e.to_string())
}

/// Parse a JSON value into a type through text, which is safe with
/// serde_json's arbitrary_precision feature.
fn reparse<T: DeserializeOwned>(v: &Value) -> PyResult<T> {
    let text = serde_json::to_string(v).map_err(err)?;
    serde_json::from_str(&text).map_err(err)
}

fn view_of(text: &str) -> PyResult<View> {
    let v: Value = serde_json::from_str(text).map_err(err)?;
    let mut out = View::new();
    if let Value::Object(m) = v {
        for (k, x) in m {
            out.insert(k, x);
        }
    }
    Ok(out)
}

/// The tool version.
#[pyfunction]
fn version() -> String {
    recordchaos_core::TOOL_VERSION.to_string()
}

/// Generate every case of a scenario. Returns JSON with the base bundle and
/// one object per case: id, seed, intent, mutations, bundle, pre_steps.
///
/// Like `recordchaos run`, it refuses a base bundle that fails the schema or
/// has a broken reference, and one with values that look like real
/// identifiers unless `confirm_synthetic` is true.
#[pyfunction]
#[pyo3(signature = (scenario_path, as_of=None, confirm_synthetic=false))]
fn load_cases(
    scenario_path: &str,
    as_of: Option<&str>,
    confirm_synthetic: bool,
) -> PyResult<String> {
    let limits = Limits::default();
    let scenario = load_scenario(Path::new(scenario_path), &limits).map_err(err)?;
    let base = bundle::load(&scenario.base_path, &limits).map_err(err)?;
    let validators = Validators::new(Some(SchemaValidator::bundled().map_err(err)?), None);
    check_base(&base, &validators).map_err(err)?;
    let suspects = synthetic::scan(&base.value);
    if !confirm_synthetic {
        if let Some(s) = suspects.first() {
            return Err(PyValueError::new_err(format!(
                "the base bundle {} has {} values that look like real identifiers (first: {} at {}). \
                 Use synthetic data only. If this bundle is synthetic, pass --recordchaos-confirm-synthetic. \
                 This check is a heuristic and does not prove that a bundle is de-identified.",
                base.source,
                suspects.len(),
                s.kind,
                s.path
            )));
        }
    }
    let as_of = as_of
        .map(str::to_string)
        .or_else(|| scenario.as_of.clone())
        .unwrap_or_else(|| default_as_of(&base.value));
    let cases = generate(&base.value, &scenario, &as_of);
    let list: Vec<Value> = cases
        .iter()
        .map(|c| {
            json!({
                "id": c.id,
                "seed": c.seed,
                "intent": c.intent(),
                "mutations": c.mutations,
                "not_applicable": c.not_applicable,
                "bundle": c.bundle,
                "pre_steps": c.pre_steps,
            })
        })
        .collect();
    let ops: Vec<&str> = scenario.operators.iter().map(|o| o.id.as_str()).collect();
    let out = json!({
        "scenario": scenario.path,
        "scenario_sha256": scenario.sha256,
        "base_path": base.source,
        "base_sha256": base.sha256,
        "seed": scenario.seed,
        "as_of": as_of,
        "operators": ops,
        "base": base.value,
        "cases": list,
    });
    serde_json::to_string(&out).map_err(err)
}

fn step(outcome: &str, first_line: &str) -> StepOutcome {
    let (kind, code) = match outcome {
        "rejected" => (StepKind::Rejected, 2),
        "crashed" => (StepKind::Crashed, 1),
        _ => (StepKind::Accepted, 0),
    };
    StepOutcome {
        kind,
        code: Some(code),
        http: false,
        first_line: first_line.to_string(),
        operation_outcome: false,
        elapsed_ms: 0,
    }
}

/// Run the oracles for one case. `outcome` is "accepted", "rejected" or
/// "crashed"; `first_line` is the first line of the error for crashes.
#[pyfunction]
#[pyo3(signature = (case_json, view_json, baseline_json, outcome="accepted", first_line=""))]
fn check(
    case_json: &str,
    view_json: &str,
    baseline_json: &str,
    outcome: &str,
    first_line: &str,
) -> PyResult<String> {
    let case: Value = serde_json::from_str(case_json).map_err(err)?;
    let mutations: Vec<MutationRecord> =
        reparse(case.get("mutations").unwrap_or(&Value::Array(Vec::new())))?;
    let view = view_of(view_json)?;
    let baseline = view_of(baseline_json)?;
    let mut spec: BTreeMap<String, ViewEntry> = BTreeMap::new();
    for k in view.keys().chain(baseline.keys()) {
        spec.insert(
            k.clone(),
            ViewEntry {
                pointer: k.clone(),
                get: None,
            },
        );
    }
    let accepted = outcome == "accepted";
    let exec = Execution {
        reset: StepOutcome::accepted(),
        ingests: vec![step(outcome, first_line)],
        observe: accepted.then(StepOutcome::accepted),
        view: if accepted { view } else { View::new() },
    };
    let accept = AcceptRules {
        rejected_exit_codes: vec![2],
        flag_view: spec
            .contains_key("flagged")
            .then(|| String::from("flagged")),
    };
    let invalid = case_intent(&mutations).is_invalid();
    let views: Vec<MutationView<'_>> = mutations
        .iter()
        .filter(|m| invalid || !effect_is_invalid(&m.effect))
        .map(|m| MutationView {
            operator: m.operator.as_str(),
            effect: &m.effect,
        })
        .collect();
    let input = OracleInput {
        mutations: &views,
        invalid,
        exec: &exec,
        baseline: &baseline,
        spec: &spec,
        accept: &accept,
        user: &[],
    };
    let mut results = oracle::evaluate(&input);
    // In-process there is no exit code: a crash is an exception raised by the
    // importer, and `first_line` names it ("KeyError at importer.py:95").
    let crashed = outcome == "crashed";
    if crashed {
        for r in results
            .iter_mut()
            .filter(|r| r.oracle == oracle::NO_CRASH && r.failed())
        {
            r.expected = Some(String::from("no exception from the importer"));
            r.got = Some(if first_line.is_empty() {
                String::from("an exception")
            } else {
                first_line.to_string()
            });
        }
    }
    let primary = primary_failure(&results).cloned();
    let fp = primary.as_ref().map(|p| fingerprint(p, &exec));
    let sentence = primary.as_ref().map(|p| {
        let conditions = relevant_conditions(p, &mutations);
        if crashed && p.oracle == oracle::NO_CRASH {
            let exception = first_line
                .split_whitespace()
                .next()
                .filter(|w| !w.is_empty())
                .map(|w| format!("raised {w}"))
                .unwrap_or_else(|| String::from("raised an exception"));
            let cond = if conditions.is_empty() {
                String::from("this case ran")
            } else {
                conditions.join(" and ")
            };
            format!("The importer {exception} when {cond}.")
        } else {
            oracle::sentence(p, exec.deciding(), &conditions, None)
        }
    });
    let out_kind = match &primary {
        Some(p) if p.oracle == oracle::NO_CRASH => Outcome::Crashed,
        Some(_) => Outcome::RuleBroken,
        None if outcome == "rejected" => Outcome::RejectedCleanly,
        None => Outcome::Handled,
    };
    let out = json!({
        "oracles": results,
        "primary": primary,
        "fingerprint": fp,
        "sentence": sentence,
        "outcome": out_kind,
        "check": primary.as_ref().map(|p| check_text(p, &spec)),
        "note": primary.as_ref().and_then(|p| note_for(&p.oracle, &mutations)),
    });
    serde_json::to_string(&out).map_err(err)
}

/// Apply a subset of a case's mutations to the base again (for shrinking).
#[pyfunction]
fn derive_case(base_json: &str, mutations_json: &str, as_of: &str) -> PyResult<String> {
    let base: Value = serde_json::from_str(base_json).map_err(err)?;
    let muts: Vec<MutationRecord> =
        reparse(&serde_json::from_str::<Value>(mutations_json).map_err(err)?)?;
    let d = reapply(&base, &muts, as_of)
        .map_err(|e| PyValueError::new_err(format!("not applicable: {}", e.0)))?;
    let out = json!({"bundle": d.bundle, "pre_steps": d.pre_steps, "mutations": d.records});
    serde_json::to_string(&out).map_err(err)
}

/// Write results.json, report.html and junit.xml for a pytest session.
/// `payload` holds the scenario (from load_cases, without cases) and one
/// record per checked case: {"case": ..., "result": ..., "shrunk": ...}.
#[pyfunction]
fn render_report(payload_json: &str, out_dir: &str) -> PyResult<String> {
    let payload: Value = serde_json::from_str(payload_json).map_err(err)?;
    let scen = payload.get("scenario").cloned().unwrap_or(Value::Null);
    let s = |k: &str| {
        scen.get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let base = scen.get("base").cloned().unwrap_or(Value::Null);
    let validators = Validators::new(Some(SchemaValidator::bundled().map_err(err)?), None);
    let records = payload
        .get("records")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut cases: Vec<CaseResult> = Vec::new();
    let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut extra: Vec<(Value, Vec<MutationRecord>, Option<OracleResult>, Value)> = Vec::new();
    for (i, rec) in records.iter().enumerate() {
        let case = rec.get("case").cloned().unwrap_or(Value::Null);
        let result = rec.get("result").cloned().unwrap_or(Value::Null);
        let mutations: Vec<MutationRecord> =
            reparse(case.get("mutations").unwrap_or(&Value::Array(Vec::new())))?;
        let oracles: Vec<OracleResult> =
            reparse(result.get("oracles").unwrap_or(&Value::Array(Vec::new())))?;
        let primary: Option<OracleResult> = reparse(result.get("primary").unwrap_or(&Value::Null))?;
        let outcome: Outcome = reparse(result.get("outcome").unwrap_or(&json!("handled")))?;
        let fp = result
            .get("fingerprint")
            .and_then(Value::as_str)
            .map(str::to_string);
        let bundle_v = case.get("bundle").cloned().unwrap_or(Value::Null);
        if let Some(f) = &fp {
            groups.entry(f.clone()).or_default().push(i);
        }
        cases.push(CaseResult {
            id: case
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            seed: case.get("seed").and_then(Value::as_u64).unwrap_or(0),
            intent: case_intent(&mutations),
            validation: validators.validate(&bundle_v),
            validity_note: None,
            outcome,
            step: None,
            oracles,
            finding: None,
            fingerprint: fp,
            dir: None,
            not_applicable: Vec::new(),
            mutations: mutations.clone(),
        });
        // A shrunk case carries its own bundle and oracle result, so the
        // finding's sentence, diff and validation describe the shrunk case.
        let shrunk_result = rec.get("shrunk_result").filter(|v| !v.is_null());
        let (shrunk, bundle_v, primary, result) = match rec.get("shrunk") {
            Some(v) if !v.is_null() => {
                let m: Vec<MutationRecord> = reparse(v)?;
                let b = rec
                    .get("shrunk_bundle")
                    .filter(|v| !v.is_null())
                    .cloned()
                    .unwrap_or(bundle_v);
                match shrunk_result {
                    Some(r) => {
                        let p: Option<OracleResult> =
                            reparse(r.get("primary").unwrap_or(&Value::Null))?;
                        (m, b, p.or(primary), r.clone())
                    }
                    None => (m, b, primary, result),
                }
            }
            _ => (mutations, bundle_v, primary, result),
        };
        extra.push((bundle_v, shrunk, primary, result));
    }
    let mut ordered: Vec<(String, Vec<usize>)> = groups.into_iter().collect();
    ordered.sort_by_key(|(_, v)| v.first().copied().unwrap_or(usize::MAX));
    let mut findings = Vec::new();
    for (n, (fp, idxs)) in ordered.iter().enumerate() {
        let fid = format!("F{}", n + 1);
        let Some(&rep) = idxs.iter().min_by_key(|i| extra[**i].1.len()) else {
            continue;
        };
        let (bundle_v, shrunk, primary, result) = &extra[rep];
        let Some(primary) = primary.clone() else {
            continue;
        };
        for i in idxs {
            cases[*i].finding = Some(fid.clone());
        }
        let removed: Vec<String> = Vec::new();
        let old_text = canonical(&without_entries(&base, &removed)).map_err(err)?;
        let new_text = canonical(bundle_v).map_err(err)?;
        let before = cases[rep].mutations.len();
        findings.push(Finding {
            id: fid,
            oracle: primary.oracle.clone(),
            kind: oracle::failure_kind(&primary).to_string(),
            fingerprint: fp.clone(),
            sentence: result
                .get("sentence")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            cases: idxs.iter().map(|i| cases[*i].id.clone()).collect(),
            representative: cases[rep].id.clone(),
            mutations_before: before,
            mutations_after: shrunk.len(),
            entries_removed: 0,
            shrink_replays: 0,
            flaky: false,
            reproduced: None,
            expected: primary.expected.clone(),
            got: primary.got.clone(),
            check: result
                .get("check")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            mutations: shrunk.clone(),
            validation: validators.validate(bundle_v),
            diff_title: {
                // Same wording as `recordchaos run`.
                let changed: usize = shrunk.iter().map(|m| m.patch_ops).sum();
                format!(
                    "{} · {changed} {} changed",
                    shrunk
                        .iter()
                        .map(|m| m.target.label())
                        .collect::<Vec<_>>()
                        .join(", "),
                    if changed == 1 { "path" } else { "paths" }
                )
            },
            diff: line_diff(&old_text, &new_text, 2, 60),
            replay: format!("pytest -k {}", cases[rep].id),
            first_line: None,
            note: result
                .get("note")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    let count = |o: Outcome| cases.iter().filter(|c| c.outcome == o).count();
    let failed = count(Outcome::RuleBroken) + count(Outcome::Crashed);
    let summary = Summary {
        cases: cases.len(),
        ran: cases.len(),
        handled: count(Outcome::Handled),
        rejected_cleanly: count(Outcome::RejectedCleanly),
        rule_broken: count(Outcome::RuleBroken),
        crashed: count(Outcome::Crashed),
        not_applicable: count(Outcome::NotApplicable),
        failed,
        expected_invalid: cases.iter().filter(|c| c.intent == Intent::Invalid).count(),
        unexpected_validity: 0,
        unexpected_invalidity: 0,
        valid_edge_cases: cases.iter().filter(|c| c.intent != Intent::Invalid).count(),
        valid_edge_schema_valid: cases
            .iter()
            .filter(|c| {
                c.intent != Intent::Invalid && c.validation.schema.status == LayerStatus::Valid
            })
            .count(),
        findings: findings.len(),
        shrink_replays: 0,
    };
    let ops: Vec<String> = scen
        .get("operators")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let results = RunResults {
        format_version: recordchaos_core::RESULTS_FORMAT_VERSION,
        tool: recordchaos_core::TOOL_NAME.to_string(),
        tool_version: recordchaos_core::TOOL_VERSION.to_string(),
        rng: recordchaos_core::RNG_NAME.to_string(),
        seed: scen.get("seed").and_then(Value::as_u64).unwrap_or(0),
        fhir_version: recordchaos_core::FHIR_VERSION.to_string(),
        scenario: InputRef {
            path: s("scenario"),
            sha256: s("scenario_sha256"),
        },
        base: InputRef {
            path: s("base_path"),
            sha256: s("base_sha256"),
        },
        target: TargetInfo {
            path: String::from("pytest"),
            sha256: String::new(),
            kind: String::from("python"),
            describe: String::from("pytest, in-process"),
        },
        as_of: s("as_of"),
        layers: LayersRun {
            schema: true,
            references: true,
            strict: false,
        },
        operators: ops,
        registry_size: REGISTRY.len(),
        out_dir: out_dir.replace('\\', "/"),
        summary,
        cases,
        findings,
        not_checked: vec![
            Note {
                title: String::from("Invariants and required fields"),
                text: String::from(
                    "The R4 JSON schema does not require Observation.status or check that a period starts before it ends. Only the HL7 validator does; the pytest plugin does not run it.",
                ),
            },
            Note {
                title: String::from("Terminology"),
                text: String::from(
                    "Codes are synthetic and are never checked against a terminology server.",
                ),
            },
            Note {
                title: String::from("Clinical sense"),
                text: String::from(
                    "A passing run says the importer copes with these shapes of data. It says nothing about clinical correctness.",
                ),
            },
        ],
    };
    let dir = Path::new(out_dir);
    write_text(
        &dir.join("results.json"),
        &recordchaos_core::json::canonical_of(&results).map_err(err)?,
    )
    .map_err(err)?;
    let meta = recordchaos_report::ReportMeta {
        generated_at: None,
        total_seconds: None,
        embed_fonts: true,
    };
    let html = recordchaos_report::render_html(&results, &meta).map_err(err)?;
    write_text(&dir.join("report.html"), &html).map_err(err)?;
    let junit = recordchaos_report::render_junit(&results).map_err(err)?;
    write_text(&dir.join("junit.xml"), &junit).map_err(err)?;
    Ok(results.verdict())
}

/// The native module, `recordchaos._core`.
#[pymodule]
mod _core {
    #[pymodule_export]
    use super::{check, derive_case, load_cases, render_report, version};
}
