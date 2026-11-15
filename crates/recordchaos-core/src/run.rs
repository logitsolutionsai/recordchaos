//! A whole run: baseline, generate, validate, run, check, shrink, group.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Instant;

use serde::Serialize;
use serde_json::{Value, json};

use crate::bundle::{LoadedBundle, PatchOp, apply_patch, without_entries};
use crate::diff::line_diff;
use crate::error::{Error, Result, write_text};
use crate::generate::{
    GeneratedCase, MutationRecord, case_intent, default_as_of, effect_is_invalid, generate, reapply,
};
use crate::json::{canonical, canonical_of, json_to_toml};
use crate::operators::{EffectKind, Intent, REGISTRY, find};
use crate::oracle::{
    self, MutationView, OracleInput, OracleResult, OracleStatus, condition, fingerprint,
    primary_failure,
};
use crate::results::{
    CaseResult, Finding, InputRef, LayersRun, Note, Outcome, RunResults, Summary, TargetInfo,
};
use crate::scenario::{Scenario, TargetConfig, TargetKind, UserOracle};
use crate::shrink::{Budget, dangling_count, ddmin, removable_entries};
use crate::target::{AcceptRules, Execution, Target, View, ViewEntry, execute};
use crate::validate::{Layer, LayerStatus, Validation, Validators};

/// Options for `recordchaos run`.
#[derive(Clone, Debug)]
pub struct RunOptions {
    pub as_of: Option<String>,
    pub jobs: usize,
    pub shrink_budget: usize,
    /// The results folder as the user named it, for replay commands.
    pub out_dir: String,
}

/// Wall times per stage, for the terminal and run.toml only.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Timings {
    pub generate_s: f64,
    pub validate_s: f64,
    pub baseline_s: f64,
    pub run_s: f64,
    pub shrink_s: f64,
    pub total_s: f64,
}

/// Files to write for one case folder.
#[derive(Clone, Debug)]
pub struct CaseArtifact {
    pub id: String,
    pub bundle: Value,
    pub original: Value,
    pub pre_steps: Vec<Value>,
    pub patch: Vec<PatchOp>,
    pub observed: View,
    pub expected: Value,
    pub case_toml: String,
    /// The mutations left after shrinking, with their effects.
    pub mutations: Vec<MutationRecord>,
}

/// Everything a run produced.
pub struct RunOutput {
    pub results: RunResults,
    pub artifacts: Vec<CaseArtifact>,
    pub timings: Timings,
    pub baseline: View,
}

/// A stage finished, for live terminal output.
pub trait Progress {
    fn stage(&mut self, name: &str, text: &str, seconds: Option<f64>);
}

/// Progress that prints nothing.
pub struct Silent;

impl Progress for Silent {
    fn stage(&mut self, _name: &str, _text: &str, _seconds: Option<f64>) {}
}

/// What one replay of a case produced.
#[derive(Clone, Debug)]
struct Probe {
    exec: Execution,
    oracles: Vec<OracleResult>,
    primary: Option<OracleResult>,
    fingerprint: Option<String>,
}

/// Shared, per-run context for evaluating oracles.
struct Judge<'a> {
    spec: &'a BTreeMap<String, ViewEntry>,
    accept: &'a AcceptRules,
    user: &'a [UserOracle],
}

fn invalid_for_oracles(mutations: &[MutationRecord], unexpected_validity: bool) -> bool {
    case_intent(mutations).is_invalid() && !unexpected_validity
}

fn probe(
    target: &mut dyn Target,
    judge: &Judge<'_>,
    steps: &[&Value],
    mutations: &[MutationRecord],
    invalid: bool,
    baseline: &View,
) -> Result<Probe> {
    let exec = execute(target, steps)?;
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
        baseline,
        spec: judge.spec,
        accept: judge.accept,
        user: judge.user,
    };
    let oracles = oracle::evaluate(&input);
    let primary = primary_failure(&oracles).cloned();
    let fp = primary.as_ref().map(|p| fingerprint(p, &exec));
    Ok(Probe {
        exec,
        oracles,
        primary,
        fingerprint: fp,
    })
}

/// Check that the base bundle is clean: schema-valid and every reference
/// resolves. The baseline must be clean.
pub fn check_base(base: &LoadedBundle, validators: &Validators) -> Result<Validation> {
    if crate::bundle::entries(&base.value).is_empty() {
        return Err(Error::input(format!(
            "the base bundle {} has no entries, so there is nothing to mutate",
            base.source
        )));
    }
    let v = validators.validate_quick(&base.value);
    if v.schema.status == LayerStatus::Invalid {
        let first = v
            .schema
            .issues
            .first()
            .map(|i| format!("{} {}", i.path, i.message))
            .unwrap_or_default();
        return Err(Error::input(format!(
            "the base bundle {} fails the R4 schema, so it cannot be a clean baseline: {first}",
            base.source
        )));
    }
    if v.references.status == LayerStatus::Invalid {
        let first = v
            .references
            .issues
            .first()
            .map(|i| format!("{} {}", i.path, i.message))
            .unwrap_or_default();
        return Err(Error::input(format!(
            "the base bundle {} has a broken reference, so it cannot be a clean baseline: {first}",
            base.source
        )));
    }
    Ok(v)
}

/// Compare a case's validation with its operators' intent.
fn validity_note(mutations: &[MutationRecord], v: &Validation) -> (Option<String>, bool, bool) {
    let intent = case_intent(mutations);
    let invalid_layers: Vec<Layer> = [Layer::Schema, Layer::References, Layer::Strict]
        .into_iter()
        .filter(|l| v.get(*l).status == LayerStatus::Invalid)
        .collect();
    if intent.is_invalid() {
        if !invalid_layers.is_empty() {
            return (None, false, false);
        }
        // Which layers should have caught it, and did any of them run?
        let mut catchers: BTreeSet<Layer> = BTreeSet::new();
        for m in mutations.iter().filter(|m| m.intent == Intent::Invalid) {
            if let Some(op) = find(&m.operator) {
                catchers.extend(op.info.caught_by.iter().copied());
            }
        }
        let ran: Vec<Layer> = catchers
            .iter()
            .copied()
            .filter(|l| v.get(*l).status == LayerStatus::Valid)
            .collect();
        if ran.is_empty() {
            let names: Vec<&str> = catchers.iter().map(|l| l.name()).collect();
            return (
                Some(format!(
                    "Intended invalid; the layer that catches it ({}) did not run",
                    names.join(", ")
                )),
                false,
                false,
            );
        }
        let names: Vec<&str> = ran.iter().map(|l| l.name()).collect();
        return (
            Some(format!(
                "unexpected_validity: meant to be invalid, but {} found it valid",
                names.join(", ")
            )),
            true,
            false,
        );
    }
    if let Some(l) = invalid_layers.first() {
        let first = v
            .get(*l)
            .issues
            .first()
            .map(|i| format!("{} {}", i.path, i.message))
            .unwrap_or_default();
        return (
            Some(format!(
                "unexpected_invalidity: meant to be valid, but {} found: {first}",
                l.name()
            )),
            false,
            true,
        );
    }
    (None, false, false)
}

fn outcome_of(p: &Probe) -> Outcome {
    match &p.primary {
        Some(r) if r.oracle == oracle::NO_CRASH => Outcome::Crashed,
        Some(_) => Outcome::RuleBroken,
        None if p.exec.rejected() => Outcome::RejectedCleanly,
        None => Outcome::Handled,
    }
}

/// Result of shrinking one failure.
struct Shrunk {
    mutations: Vec<MutationRecord>,
    removed: Vec<String>,
    bundle: Value,
    pre_steps: Vec<Value>,
    reduced_base: Value,
    probe: Probe,
    fingerprint: String,
    replays: usize,
    flaky: bool,
    reproduced: Option<String>,
    /// False for a failure that was not shrunk because another case with
    /// the same fingerprint was.
    attempted: bool,
}

impl Shrunk {
    /// A failing case left as generated: another case with the same
    /// fingerprint was shrunk for the finding.
    fn not_attempted(case: &GeneratedCase, first: &Probe, base: &Value) -> Self {
        Self {
            mutations: case.mutations.clone(),
            removed: Vec::new(),
            bundle: case.bundle.clone(),
            pre_steps: case.pre_steps.clone(),
            reduced_base: base.clone(),
            probe: first.clone(),
            fingerprint: first.fingerprint.clone().unwrap_or_default(),
            replays: 0,
            flaky: false,
            reproduced: None,
            attempted: false,
        }
    }
}

fn needs_baseline(oracle_id: &str, user: &[UserOracle]) -> bool {
    oracle_id == oracle::SAME_AS_BASE
        || oracle_id == oracle::NO_CROSS_PATIENT_MERGE
        || user
            .iter()
            .any(|u| u.id == oracle_id && u.check == crate::scenario::CheckKind::SameAsBase)
}

#[allow(clippy::too_many_arguments)]
fn shrink_failure(
    target: &mut dyn Target,
    judge: &Judge<'_>,
    base: &Value,
    baseline: &View,
    case: &GeneratedCase,
    first: &Probe,
    unexpected_validity: bool,
    as_of: &str,
    budget_limit: usize,
) -> Result<Shrunk> {
    let target_fp = first.fingerprint.clone().unwrap_or_default();
    let mut budget = Budget::new(budget_limit.max(1));
    // Reproduce first. A failure that does not reproduce is flaky.
    let steps = case.steps();
    let again = if budget.take() {
        Some(probe(
            target,
            judge,
            &steps,
            &case.mutations,
            invalid_for_oracles(&case.mutations, unexpected_validity),
            baseline,
        )?)
    } else {
        None
    };
    let reproduced = again
        .as_ref()
        .is_some_and(|p| p.fingerprint.as_deref() == Some(target_fp.as_str()));
    if !reproduced {
        return Ok(Shrunk {
            mutations: case.mutations.clone(),
            removed: Vec::new(),
            bundle: case.bundle.clone(),
            pre_steps: case.pre_steps.clone(),
            reduced_base: base.clone(),
            probe: first.clone(),
            fingerprint: target_fp,
            replays: budget.used,
            flaky: true,
            reproduced: Some(String::from("reproduced 0 of 1 replays")),
            attempted: true,
        });
    }
    let mut best = again.unwrap_or_else(|| first.clone());

    // Stage 1: ddmin over the mutations.
    let mut error: Option<Error> = None;
    let mut kept = case.mutations.clone();
    if kept.len() > 1 {
        let mut last_good: Option<Probe> = None;
        let mut test = |subset: &[MutationRecord]| -> bool {
            if error.is_some() || subset.is_empty() || !budget.take() {
                return false;
            }
            let Ok(derived) = reapply(base, subset, as_of) else {
                return false;
            };
            let mut steps: Vec<&Value> = derived.pre_steps.iter().collect();
            steps.push(&derived.bundle);
            match probe(
                target,
                judge,
                &steps,
                &derived.records,
                invalid_for_oracles(&derived.records, false),
                baseline,
            ) {
                Ok(p) => {
                    let ok = p.fingerprint.as_deref() == Some(target_fp.as_str());
                    if ok {
                        last_good = Some(p);
                    }
                    ok
                }
                Err(e) => {
                    error = Some(e);
                    false
                }
            }
        };
        let reduced = ddmin(&case.mutations, &mut test);
        if let Some(e) = error.take() {
            return Err(e);
        }
        if reduced.len() < kept.len() {
            kept = reduced;
            if let Some(p) = last_good {
                best = p;
            }
        }
    }
    let derived = reapply(base, &kept, as_of)
        .map_err(|e| Error::internal(format!("shrinking lost a mutation: {}", e.0)))?;
    let mut bundle = derived.bundle;
    let mut pre_steps = derived.pre_steps;
    let mut reduced_base = base.clone();
    let mut removed: Vec<String> = Vec::new();

    // Stage 2: remove entries the remaining mutations do not need.
    let use_baseline = best
        .primary
        .as_ref()
        .is_some_and(|p| needs_baseline(&p.oracle, judge.user));
    let invalid = invalid_for_oracles(&kept, false);
    let mut progress = true;
    while progress && budget.left() > 0 {
        progress = false;
        for key in removable_entries(&bundle, &reduced_base, &derived.touched) {
            let cost = if use_baseline { 2 } else { 1 };
            if budget.left() < cost {
                break;
            }
            let keys = vec![key.clone()];
            let cand = without_entries(&bundle, &keys);
            if dangling_count(&cand) > dangling_count(&bundle) {
                continue;
            }
            let cand_base = without_entries(&reduced_base, &keys);
            let cand_pre: Vec<Value> = pre_steps
                .iter()
                .map(|p| without_entries(p, &keys))
                .collect();
            let cand_baseline = if use_baseline {
                budget.take();
                let exec = execute(target, &[&cand_base])?;
                if !exec.accepted() {
                    continue;
                }
                exec.view
            } else {
                baseline.clone()
            };
            budget.take();
            let mut steps: Vec<&Value> = cand_pre.iter().collect();
            steps.push(&cand);
            let p = probe(target, judge, &steps, &kept, invalid, &cand_baseline)?;
            if p.fingerprint.as_deref() == Some(target_fp.as_str()) {
                bundle = cand;
                reduced_base = cand_base;
                pre_steps = cand_pre;
                removed.push(key);
                best = p;
                progress = true;
            }
        }
    }
    Ok(Shrunk {
        mutations: kept,
        removed,
        bundle,
        pre_steps,
        reduced_base,
        probe: best,
        fingerprint: target_fp,
        replays: budget.used,
        flaky: false,
        reproduced: None,
        attempted: true,
    })
}

/// Condition clauses of the mutations relevant to a failure.
pub fn relevant_conditions(r: &OracleResult, mutations: &[MutationRecord]) -> Vec<String> {
    let pick: Vec<&MutationRecord> = match r.oracle.as_str() {
        oracle::INVALID_REJECTED | oracle::NO_CROSS_PATIENT_MERGE => mutations
            .iter()
            .filter(|m| effect_is_invalid(&m.effect))
            .collect(),
        oracle::LATEST_CORRECTION_SELECTED => mutations
            .iter()
            .filter(|m| m.effect.kind == EffectKind::LatestCorrection)
            .collect(),
        oracle::SAME_AS_BASE => mutations
            .iter()
            .filter(|m| {
                matches!(
                    m.effect.kind,
                    EffectKind::SameAsBase | EffectKind::SameAsBaseOrRejected
                )
            })
            .collect(),
        _ => mutations.iter().collect(),
    };
    let chosen: Vec<&MutationRecord> = if pick.is_empty() {
        mutations.iter().collect()
    } else {
        pick
    };
    let mut out: Vec<String> = Vec::new();
    for m in chosen {
        let c = condition(&m.operator, &m.args);
        if !out.contains(&c) {
            out.push(c);
        }
    }
    out
}

/// What the failing check read, for the report.
pub fn check_text(r: &OracleResult, spec: &BTreeMap<String, ViewEntry>) -> String {
    if r.user {
        return format!("Declared in the target file: {}", r.oracle);
    }
    let keys: Vec<String> = match r.oracle.as_str() {
        oracle::NO_CRASH => {
            return String::from("Built in. Reads the exit code or HTTP status of every step");
        }
        oracle::INVALID_REJECTED => {
            return String::from(
                "Built in. Expects a rejected exit code, a 4xx response or a flag in the view",
            );
        }
        _ => r
            .key
            .split_once(':')
            .map(|(_, k)| k.split(',').map(str::to_string).collect())
            .unwrap_or_default(),
    };
    let ptrs: Vec<String> = keys
        .iter()
        .filter_map(|k| {
            spec.get(k)
                .map(|e| e.get.clone().unwrap_or_else(|| e.pointer.clone()))
        })
        .collect();
    if ptrs.is_empty() {
        String::from("Built in. Compares your target's view with the base bundle's view")
    } else {
        format!(
            "Built in. Reads {} from your target's view",
            ptrs.join(", ")
        )
    }
}

/// Why a failure matters, for the report.
pub fn note_for(oracle_id: &str, mutations: &[MutationRecord]) -> Option<String> {
    let ops: BTreeSet<&str> = mutations.iter().map(|m| m.operator.as_str()).collect();
    match oracle_id {
        oracle::LATEST_CORRECTION_SELECTED => Some(String::from(
            "This bundle is valid FHIR. A correction is legitimate, so a robust importer should prefer the later issued time or the final status.",
        )),
        oracle::SAME_AS_BASE if ops.contains("bundle.reorder_entries") => Some(String::from(
            "Likely cause: the importer treats bundle order as time order. FHIR gives entry order no meaning in a collection bundle.",
        )),
        oracle::SAME_AS_BASE => Some(String::from(
            "This mutation keeps the bundle's meaning, so the view should not change.",
        )),
        oracle::NO_CRASH if ops.contains("reference.to_urn_uuid") => Some(String::from(
            "Transaction bundles from many EHR exports use urn:uuid full URLs. The bundle is valid FHIR.",
        )),
        oracle::INVALID_REJECTED => Some(String::from(
            "This bundle is invalid. Accepting it silently stores a record that does not hold together.",
        )),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn case_toml(
    case_id: &str,
    seed: u64,
    mutations: &[MutationRecord],
    validation: &Validation,
    fingerprint: Option<&str>,
    target_path: &str,
    flaky: bool,
    shrunk: bool,
) -> Result<String> {
    let mut root = toml::Table::new();
    root.insert(String::from("id"), toml::Value::String(case_id.to_string()));
    // Case seeds are u64 and half of them do not fit a TOML integer (i64),
    // so the decimal is written as text; clamping it gave i64::MAX.
    root.insert(String::from("seed"), toml::Value::String(seed.to_string()));
    root.insert(
        String::from("seed_hex"),
        toml::Value::String(format!("{seed:016x}")),
    );
    root.insert(
        String::from("base"),
        toml::Value::String(String::from("../../base.json")),
    );
    root.insert(
        String::from("target"),
        toml::Value::String(target_path.to_string()),
    );
    if let Some(fp) = fingerprint {
        root.insert(
            String::from("fingerprint"),
            toml::Value::String(fp.to_string()),
        );
    }
    root.insert(String::from("flaky"), toml::Value::Boolean(flaky));
    // False when another case with the same fingerprint was shrunk instead.
    root.insert(String::from("shrunk"), toml::Value::Boolean(shrunk));
    let mut layers = toml::Table::new();
    for l in [Layer::Schema, Layer::References, Layer::Strict] {
        let r = validation.get(l);
        let status = match r.status {
            LayerStatus::Valid => "valid",
            LayerStatus::Invalid => "invalid",
            LayerStatus::NotChecked => "not_checked",
        };
        layers.insert(
            l.name().to_string(),
            toml::Value::String(status.to_string()),
        );
    }
    root.insert(String::from("layers"), toml::Value::Table(layers));
    let mut muts = Vec::new();
    for m in mutations {
        let mut t = toml::Table::new();
        t.insert(
            String::from("operator"),
            toml::Value::String(m.operator.clone()),
        );
        t.insert(
            String::from("version"),
            toml::Value::Integer(i64::from(m.version)),
        );
        t.insert(
            String::from("target"),
            toml::Value::String(m.target.label()),
        );
        t.insert(
            String::from("seed_hex"),
            toml::Value::String(format!("{:016x}", m.seed)),
        );
        if let Some(args) = json_to_toml(&Value::Object(m.args.clone())) {
            t.insert(String::from("args"), args);
        }
        t.insert(
            String::from("summary"),
            toml::Value::String(m.summary.clone()),
        );
        muts.push(toml::Value::Table(t));
    }
    root.insert(String::from("mutation"), toml::Value::Array(muts));
    toml::to_string_pretty(&root)
        .map_err(|e| Error::internal(format!("cannot write case.toml: {e}")))
}

/// A view as a JSON object.
pub fn view_json(view: &View) -> Value {
    let mut m = serde_json::Map::new();
    for (k, v) in view {
        m.insert(k.clone(), v.clone());
    }
    Value::Object(m)
}

/// Run a scenario against a target.
#[allow(clippy::too_many_arguments)]
pub fn run(
    scenario: &Scenario,
    base: &LoadedBundle,
    target_cfg: &TargetConfig,
    target: &mut dyn Target,
    validators: &Validators,
    opts: &RunOptions,
    progress: &mut dyn Progress,
) -> Result<RunOutput> {
    let total = Instant::now();
    let mut timings = Timings::default();
    check_base(base, validators)?;
    let as_of = opts
        .as_of
        .clone()
        .or_else(|| scenario.as_of.clone())
        .unwrap_or_else(|| default_as_of(&base.value));

    // Generate.
    let t = Instant::now();
    let cases = generate(&base.value, scenario, &as_of);
    timings.generate_s = t.elapsed().as_secs_f64();
    let used_ops: BTreeSet<&str> = scenario.operators.iter().map(|o| o.id.as_str()).collect();
    progress.stage(
        "generate",
        &format!(
            "{} {} from {} {}",
            cases.len(),
            if cases.len() == 1 { "case" } else { "cases" },
            used_ops.len(),
            if used_ops.len() == 1 {
                "operator"
            } else {
                "operators"
            }
        ),
        Some(timings.generate_s),
    );

    // Validate.
    let t = Instant::now();
    let refs: Vec<&Value> = cases.iter().map(|c| &c.bundle).collect();
    let validations = validators.validate_all(&refs, opts.jobs);
    timings.validate_s = t.elapsed().as_secs_f64();
    let count_valid = |l: Layer| {
        validations
            .iter()
            .filter(|v| v.get(l).status == LayerStatus::Valid)
            .count()
    };
    let strict_text = if validators.strict.is_some() {
        format!("strict {}/{}", count_valid(Layer::Strict), cases.len())
    } else {
        String::from("strict not run (add --strict)")
    };
    progress.stage(
        "validate",
        &format!(
            "schema {}/{} · references {}/{} · {strict_text}",
            count_valid(Layer::Schema),
            cases.len(),
            count_valid(Layer::References),
            cases.len()
        ),
        Some(timings.validate_s),
    );

    // Baseline.
    let t = Instant::now();
    let base_exec = execute(target, &[&base.value])?;
    timings.baseline_s = t.elapsed().as_secs_f64();
    if !base_exec.accepted() {
        let s = base_exec.deciding();
        return Err(Error::unavailable(format!(
            "the target did not accept the base bundle ({}{}). Fix the importer or the target file first.",
            s.code_text(),
            if s.first_line.is_empty() {
                String::new()
            } else {
                format!(": {}", s.first_line)
            }
        )));
    }
    let baseline = base_exec.view.clone();
    progress.stage(
        "baseline",
        &format!("{} · view captured", target.describe()),
        Some(timings.baseline_s),
    );

    // Run.
    let accept = target.accept().clone();
    let judge = Judge {
        spec: target_cfg.view_spec(),
        accept: &accept,
        user: &target_cfg.oracles,
    };
    let t = Instant::now();
    let mut probes: Vec<Option<Probe>> = Vec::with_capacity(cases.len());
    let mut notes: Vec<(Option<String>, bool, bool)> = Vec::with_capacity(cases.len());
    for (case, v) in cases.iter().zip(validations.iter()) {
        let note = validity_note(&case.mutations, v);
        if case.mutations.is_empty() {
            probes.push(None);
        } else {
            let invalid = invalid_for_oracles(&case.mutations, note.1);
            probes.push(Some(probe(
                target,
                &judge,
                &case.steps(),
                &case.mutations,
                invalid,
                &baseline,
            )?));
        }
        notes.push(note);
    }
    timings.run_s = t.elapsed().as_secs_f64();
    let outcomes: Vec<Outcome> = probes
        .iter()
        .map(|p| p.as_ref().map(outcome_of).unwrap_or(Outcome::NotApplicable))
        .collect();
    let count = |o: Outcome| outcomes.iter().filter(|x| **x == o).count();
    progress.stage(
        "run",
        &format!("{} cases", probes.iter().filter(|p| p.is_some()).count()),
        Some(timings.run_s),
    );
    progress.stage(
        "outcomes",
        &format!(
            "{} handled · {} rejected cleanly · {} rule broken · {} crashed",
            count(Outcome::Handled),
            count(Outcome::RejectedCleanly),
            count(Outcome::RuleBroken),
            count(Outcome::Crashed)
        ),
        None,
    );

    // Shrink. Shrinking keeps the fingerprint exactly, so failures that start
    // with the same fingerprint end in the same finding. Only one case per
    // fingerprint is shrunk (the one with the fewest mutations, then the
    // lowest id); the rest of the group keep their generated bundle. If that
    // case turns out flaky, the next one is tried. Shrinking every failing
    // case gave the same findings at many times the cost (838 replays for
    // the demo's 67 failures, against 4 findings).
    let t = Instant::now();
    let mut shrunk: BTreeMap<usize, Shrunk> = BTreeMap::new();
    let mut by_fp: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, p) in probes.iter().enumerate() {
        if let Some(fp) = p.as_ref().and_then(|p| p.fingerprint.clone()) {
            by_fp.entry(fp).or_default().push(i);
        }
    }
    for idxs in by_fp.values() {
        let mut order = idxs.clone();
        order.sort_by_key(|i| (cases[*i].mutations.len(), *i));
        let mut done = false;
        for i in order {
            let (Some(Some(p)), Some(case)) = (probes.get(i), cases.get(i)) else {
                continue;
            };
            if done {
                shrunk.insert(i, Shrunk::not_attempted(case, p, &base.value));
                continue;
            }
            let unexpected = notes.get(i).is_some_and(|n| n.1);
            let s = shrink_failure(
                target,
                &judge,
                &base.value,
                &baseline,
                case,
                p,
                unexpected,
                &as_of,
                opts.shrink_budget,
            )?;
            done = !s.flaky;
            shrunk.insert(i, s);
        }
    }
    timings.shrink_s = t.elapsed().as_secs_f64();
    let replays: usize = shrunk.values().map(|s| s.replays).sum();
    if !shrunk.is_empty() {
        progress.stage(
            "shrink",
            &format!("{} failures, {replays} replays", shrunk.len()),
            Some(timings.shrink_s),
        );
    }

    // Group by final fingerprint (flaky failures stay apart).
    let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, s) in &shrunk {
        let key = if s.flaky {
            format!("flaky|{}", s.fingerprint)
        } else {
            s.fingerprint.clone()
        };
        groups.entry(key).or_default().push(*i);
    }
    let mut ordered: Vec<(String, Vec<usize>)> = groups.into_iter().collect();
    ordered.sort_by_key(|(_, idxs)| idxs.first().copied().unwrap_or(usize::MAX));

    // The findings and case folders validate every shrunk bundle. Send the
    // ones the case validation has not seen to the strict layer in one run.
    let shrunk_bundles: Vec<&Value> = shrunk.values().map(|s| &s.bundle).collect();
    validators.prefetch_strict(&shrunk_bundles);

    let out_dir = opts.out_dir.trim_end_matches('/').to_string();
    let mut findings = Vec::new();
    let mut finding_of: BTreeMap<usize, String> = BTreeMap::new();
    let mut artifacts = Vec::new();
    for (n, (fp, idxs)) in ordered.iter().enumerate() {
        let fid = format!("F{}", n + 1);
        // Representative: the shrunk case, then fewest mutations, then fewest
        // entries, then lowest id.
        let rep = idxs
            .iter()
            .copied()
            .min_by_key(|i| {
                let s = &shrunk[i];
                (
                    !s.attempted,
                    s.mutations.len(),
                    crate::bundle::resource_count(&s.bundle),
                    *i,
                )
            })
            .unwrap_or(idxs[0]);
        let s = &shrunk[&rep];
        let case = &cases[rep];
        let primary = s.probe.primary.clone().unwrap_or_else(|| OracleResult {
            oracle: String::from("unknown"),
            status: OracleStatus::Fail,
            expected: None,
            got: None,
            detail: None,
            key: String::new(),
            user: false,
        });
        let user = target_cfg.oracles.iter().find(|u| u.id == primary.oracle);
        let sentence = oracle::sentence(
            &primary,
            s.probe.exec.deciding(),
            &relevant_conditions(&primary, &s.mutations),
            user,
        );
        let validation = validators.validate(&s.bundle);
        let old_text = canonical(&s.reduced_base)?;
        let new_text = canonical(&s.bundle)?;
        let diff = line_diff(&old_text, &new_text, 2, 60);
        let changed_paths: usize = s.mutations.iter().map(|m| m.patch_ops).sum();
        let diff_title = format!(
            "{} · {} {} changed",
            s.mutations
                .iter()
                .map(|m| m.target.label())
                .collect::<Vec<_>>()
                .join(", "),
            changed_paths,
            if changed_paths == 1 { "path" } else { "paths" }
        );
        let first_line = {
            let l = &s.probe.exec.deciding().first_line;
            if l.is_empty() { None } else { Some(l.clone()) }
        };
        findings.push(Finding {
            id: fid.clone(),
            oracle: primary.oracle.clone(),
            kind: oracle::failure_kind(&primary).to_string(),
            fingerprint: fp.trim_start_matches("flaky|").to_string(),
            sentence,
            cases: idxs.iter().map(|i| cases[*i].id.clone()).collect(),
            representative: case.id.clone(),
            mutations_before: case.mutations.len(),
            mutations_after: s.mutations.len(),
            entries_removed: s.removed.len(),
            shrink_replays: idxs.iter().map(|i| shrunk[i].replays).sum(),
            flaky: s.flaky,
            reproduced: s.reproduced.clone(),
            expected: primary.expected.clone(),
            got: primary.got.clone(),
            check: check_text(&primary, target_cfg.view_spec()),
            mutations: s.mutations.clone(),
            validation,
            diff_title,
            diff,
            replay: format!(
                "recordchaos replay {}",
                crate::scenario::shell_arg(&format!("{out_dir}/cases/{}", case.id))
            ),
            first_line,
            note: note_for(&primary.oracle, &s.mutations),
        });
        for i in idxs {
            finding_of.insert(*i, fid.clone());
        }
    }

    // Case folders for every failing case.
    for (i, s) in &shrunk {
        let case = &cases[*i];
        let expected = json!({
            "baseline_view": view_json(&baseline),
            "fingerprint": s.fingerprint,
            "oracles": s.probe.oracles,
        });
        let patch = crate::generate::reapply(&base.value, &s.mutations, &as_of)
            .map(|d| d.patch)
            .unwrap_or_else(|_| case.patch.clone());
        // patch.json is from the base bundle to the shrunk bundle.
        let patch = if s.removed.is_empty() {
            patch
        } else {
            let mut p = patch;
            let mut shadow = apply_patch(&base.value, &p).unwrap_or_else(|_| s.bundle.clone());
            for key in &s.removed {
                if let Some(idx) = crate::bundle::find_entry(&shadow, key) {
                    p.push(PatchOp {
                        op: crate::bundle::PatchKind::Remove,
                        from: None,
                        path: format!("/entry/{idx}"),
                        value: None,
                    });
                    shadow = without_entries(&shadow, std::slice::from_ref(key));
                }
            }
            p
        };
        artifacts.push(CaseArtifact {
            id: case.id.clone(),
            bundle: s.bundle.clone(),
            original: case.bundle.clone(),
            pre_steps: s.pre_steps.clone(),
            patch,
            observed: s.probe.exec.view.clone(),
            expected,
            mutations: s.mutations.clone(),
            case_toml: case_toml(
                &case.id,
                case.seed,
                &s.mutations,
                &validators.validate(&s.bundle),
                Some(&s.fingerprint),
                &target_cfg.path,
                s.flaky,
                s.attempted,
            )?,
        });
    }

    // Case results.
    let mut case_results = Vec::with_capacity(cases.len());
    for (i, case) in cases.iter().enumerate() {
        let p = probes.get(i).and_then(|p| p.as_ref());
        let note = notes.get(i).cloned().unwrap_or((None, false, false));
        case_results.push(CaseResult {
            id: case.id.clone(),
            seed: case.seed,
            mutations: case.mutations.clone(),
            not_applicable: case.not_applicable.clone(),
            intent: case.intent(),
            validation: validations
                .get(i)
                .cloned()
                .unwrap_or_else(|| validators.validate(&case.bundle)),
            validity_note: note.0,
            outcome: outcomes.get(i).copied().unwrap_or(Outcome::NotApplicable),
            step: p.map(|p| p.exec.deciding().clone()),
            oracles: p.map(|p| p.oracles.clone()).unwrap_or_default(),
            finding: finding_of.get(&i).cloned(),
            fingerprint: p.and_then(|p| p.fingerprint.clone()),
            dir: shrunk
                .contains_key(&i)
                .then(|| format!("cases/{}", case.id)),
        });
    }

    let valid_edge: Vec<&CaseResult> = case_results
        .iter()
        .filter(|c| c.intent != Intent::Invalid && !c.mutations.is_empty())
        .collect();
    let summary = Summary {
        cases: cases.len(),
        ran: probes.iter().filter(|p| p.is_some()).count(),
        handled: count(Outcome::Handled),
        rejected_cleanly: count(Outcome::RejectedCleanly),
        rule_broken: count(Outcome::RuleBroken),
        crashed: count(Outcome::Crashed),
        not_applicable: count(Outcome::NotApplicable),
        failed: count(Outcome::RuleBroken) + count(Outcome::Crashed),
        expected_invalid: case_results
            .iter()
            .filter(|c| c.intent == Intent::Invalid)
            .count(),
        unexpected_validity: notes.iter().filter(|n| n.1).count(),
        unexpected_invalidity: notes.iter().filter(|n| n.2).count(),
        valid_edge_cases: valid_edge.len(),
        valid_edge_schema_valid: valid_edge
            .iter()
            .filter(|c| c.validation.schema.status == LayerStatus::Valid)
            .count(),
        findings: findings.len(),
        shrink_replays: replays,
    };

    let mut not_checked = vec![
        if validators.strict.is_some() {
            Note {
                title: String::from("Invariants and required fields"),
                text: String::from(
                    "The HL7 validator ran with the terminology server turned off (-tx n/a), so bindings to value sets were not checked.",
                ),
            }
        } else {
            Note {
                title: String::from("Invariants and required fields"),
                text: String::from(
                    "The R4 JSON schema does not require Observation.status or check that a period starts before it ends. Only the HL7 validator does. Run with --strict to include it.",
                ),
            }
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
    ];
    let skipped_checks = case_results
        .iter()
        .flat_map(|c| c.oracles.iter())
        .filter(|o| o.status == OracleStatus::NotChecked)
        .count();
    if skipped_checks > 0 {
        not_checked.push(Note {
            title: String::from("Checks that could not run"),
            text: format!("{skipped_checks} oracle checks were not run on some cases, each with its reason in results.json. They are never counted as passed."),
        });
    }
    if summary.not_applicable > 0 {
        not_checked.push(Note {
            title: String::from("Cases with nothing to change"),
            text: format!(
                "{} cases found no eligible target for their operators and did not run.",
                summary.not_applicable
            ),
        });
    }

    let target_kind = match &target_cfg.kind {
        TargetKind::Command(_) => "command",
        TargetKind::Http(_) => "http",
    };
    let mut operators: Vec<String> = Vec::new();
    for o in &scenario.operators {
        if !operators.contains(&o.id) {
            operators.push(o.id.clone());
        }
    }
    let results = RunResults {
        format_version: crate::RESULTS_FORMAT_VERSION,
        tool: crate::TOOL_NAME.to_string(),
        tool_version: crate::TOOL_VERSION.to_string(),
        rng: crate::RNG_NAME.to_string(),
        seed: scenario.seed,
        fhir_version: crate::FHIR_VERSION.to_string(),
        scenario: InputRef {
            path: scenario.path.clone(),
            sha256: scenario.sha256.clone(),
        },
        base: InputRef {
            path: base.source.clone(),
            sha256: base.sha256.clone(),
        },
        target: TargetInfo {
            path: target_cfg.path.clone(),
            sha256: target_cfg.sha256.clone(),
            kind: target_kind.to_string(),
            describe: target.describe(),
        },
        as_of,
        layers: LayersRun {
            schema: validators.schema.is_some(),
            references: true,
            strict: validators.strict.is_some(),
        },
        operators,
        registry_size: REGISTRY.len(),
        out_dir,
        summary,
        cases: case_results,
        findings,
        not_checked,
    };
    timings.total_s = total.elapsed().as_secs_f64();
    Ok(RunOutput {
        results,
        artifacts,
        timings,
        baseline,
    })
}

/// Write results.json, run.toml, base.json and the case folders.
pub fn write_outputs(
    dir: &Path,
    out: &RunOutput,
    base: &LoadedBundle,
    run_toml: &str,
) -> Result<()> {
    write_text(&dir.join("results.json"), &canonical_of(&out.results)?)?;
    write_text(&dir.join("run.toml"), run_toml)?;
    write_text(&dir.join("base.json"), &canonical(&base.value)?)?;
    write_text(
        &dir.join("baseline-view.json"),
        &canonical(&view_json(&out.baseline))?,
    )?;
    for a in &out.artifacts {
        let cdir = dir.join("cases").join(&a.id);
        write_text(&cdir.join("bundle.json"), &canonical(&a.bundle)?)?;
        write_text(&cdir.join("original.json"), &canonical(&a.original)?)?;
        write_text(&cdir.join("case.toml"), &a.case_toml)?;
        write_text(&cdir.join("patch.json"), &canonical_of(&a.patch)?)?;
        write_text(
            &cdir.join("observed.json"),
            &canonical(&view_json(&a.observed))?,
        )?;
        write_text(&cdir.join("expected.json"), &canonical(&a.expected)?)?;
        write_text(&cdir.join("mutations.json"), &canonical_of(&a.mutations)?)?;
        if !a.pre_steps.is_empty() {
            write_text(
                &cdir.join("pre-steps.json"),
                &canonical(&Value::Array(a.pre_steps.clone()))?,
            )?;
        }
    }
    Ok(())
}

/// Result of `recordchaos replay`.
#[derive(Clone, Debug)]
pub struct ReplayOutcome {
    pub expected_fingerprint: Option<String>,
    pub fingerprint: Option<String>,
    pub oracles: Vec<OracleResult>,
    pub sentence: Option<String>,
    pub same: bool,
    pub passes: bool,
}

/// Replay a case folder against a target. The baseline is captured again
/// from the run's base.json, so a fixed importer gets a fresh baseline.
pub fn replay(
    case_dir: &Path,
    target_cfg: &TargetConfig,
    target: &mut dyn Target,
    limits: &crate::Limits,
) -> Result<ReplayOutcome> {
    let read = |name: &str| crate::error::read_text(&case_dir.join(name), limits.max_bundle_bytes);
    let case_text = read("case.toml")?;
    let case: toml::Table = toml::from_str(&case_text).map_err(|e| {
        crate::error::toml_error(
            &crate::scenario::display_path(&case_dir.join("case.toml")),
            &case_text,
            &e,
        )
    })?;
    let bundle = crate::bundle::parse(&read("bundle.json")?, limits, "bundle.json")?;
    let pre_steps: Vec<Value> = match read("pre-steps.json") {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Array(a)) => a,
            _ => return Err(Error::input("pre-steps.json must be an array of bundles")),
        },
        Err(_) => Vec::new(),
    };
    let base_rel = case
        .get("base")
        .and_then(toml::Value::as_str)
        .unwrap_or("../../base.json");
    let base_path = case_dir.join(base_rel);
    let base = crate::bundle::load(&base_path, limits)?;
    let expected_fp = case
        .get("fingerprint")
        .and_then(toml::Value::as_str)
        .map(str::to_string);
    // The shrunk mutations, with their effects, sit next to the bundle.
    let mutations: Vec<MutationRecord> =
        serde_json::from_str(&read("mutations.json")?).map_err(|e| {
            Error::input(format!(
                "{}: {e}",
                case_dir.join("mutations.json").display()
            ))
        })?;
    let base_exec = execute(target, &[&base.value])?;
    if !base_exec.accepted() {
        return Err(Error::unavailable(format!(
            "the target did not accept the base bundle ({})",
            base_exec.deciding().code_text()
        )));
    }
    // The shrunk bundle may lack some base entries; compare against the base
    // with the same entries removed when the case removed any.
    let removed: Vec<String> = {
        let keep: BTreeSet<String> = crate::bundle::entries(&bundle)
            .iter()
            .filter_map(|e| crate::bundle::resource(e).and_then(crate::bundle::resource_key))
            .collect();
        crate::bundle::entries(&base.value)
            .iter()
            .filter_map(|e| crate::bundle::resource(e).and_then(crate::bundle::resource_key))
            .filter(|k| !keep.contains(k))
            .collect()
    };
    let baseline = if removed.is_empty() {
        base_exec.view
    } else {
        let reduced = without_entries(&base.value, &removed);
        let e = execute(target, &[&reduced])?;
        if e.accepted() { e.view } else { base_exec.view }
    };
    let accept = target.accept().clone();
    let judge = Judge {
        spec: target_cfg.view_spec(),
        accept: &accept,
        user: &target_cfg.oracles,
    };
    let mut steps: Vec<&Value> = pre_steps.iter().collect();
    steps.push(&bundle);
    let invalid = case_intent(&mutations).is_invalid();
    let p = probe(target, &judge, &steps, &mutations, invalid, &baseline)?;
    let sentence = p.primary.as_ref().map(|r| {
        let user = target_cfg.oracles.iter().find(|u| u.id == r.oracle);
        oracle::sentence(
            r,
            p.exec.deciding(),
            &relevant_conditions(r, &mutations),
            user,
        )
    });
    let same = p.fingerprint.is_some() && p.fingerprint == expected_fp;
    Ok(ReplayOutcome {
        expected_fingerprint: expected_fp,
        passes: p.primary.is_none(),
        fingerprint: p.fingerprint,
        oracles: p.oracles,
        sentence,
        same,
    })
}

#[cfg(test)]
mod case_toml_tests {
    use super::*;

    #[test]
    fn large_case_seeds_are_written_exactly() {
        let seed = 0xb9e2_bc54_c283_ca7d_u64;
        let v = Validation {
            schema: crate::validate::LayerResult::valid(),
            references: crate::validate::LayerResult::valid(),
            strict: crate::validate::LayerResult::not_checked("not run"),
        };
        let text = case_toml("c-0001", seed, &[], &v, None, "t.toml", false, true).unwrap();
        let t: toml::Table = toml::from_str(&text).unwrap();
        assert_eq!(t["seed"].as_str(), Some("13394475313979968125"));
        assert_eq!(t["seed_hex"].as_str(), Some("b9e2bc54c283ca7d"));
        assert_eq!(t["shrunk"].as_bool(), Some(true));
    }
}
