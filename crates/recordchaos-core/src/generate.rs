//! Case generation. For each case, a seeded generator picks operators and
//! targets, applies them to a copy of the base and records every change.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::bundle::{PatchOp, entries, resource};
use crate::operators::{Effect, EffectKind, Intent, NotApplicable, OpCtx, TargetRef, find};
use crate::rng::CaseRng;
use crate::scenario::Scenario;
use crate::time;

/// One applied mutation, enough to apply it again during shrinking.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MutationRecord {
    pub operator: String,
    pub version: u32,
    pub target: TargetRef,
    /// Arguments after defaults were filled in.
    pub args: Map<String, Value>,
    /// Seed of this mutation's own random stream.
    pub seed: u64,
    pub summary: String,
    pub intent: Intent,
    pub effect: Effect,
    /// Number of JSON Patch operations it produced.
    pub patch_ops: usize,
}

/// A generated case.
#[derive(Clone, Debug)]
pub struct GeneratedCase {
    pub id: String,
    pub seed: u64,
    pub mutations: Vec<MutationRecord>,
    /// Operators that found nothing to change, with the reason.
    pub not_applicable: Vec<String>,
    pub bundle: Value,
    /// Bundles to ingest before `bundle` (two-step operators).
    pub pre_steps: Vec<Value>,
    pub patch: Vec<PatchOp>,
    pub touched: BTreeSet<String>,
}

impl GeneratedCase {
    /// Intended validity of the whole case.
    pub fn intent(&self) -> Intent {
        case_intent(&self.mutations)
    }

    /// All bundles to ingest, in order.
    pub fn steps(&self) -> Vec<&Value> {
        let mut v: Vec<&Value> = self.pre_steps.iter().collect();
        v.push(&self.bundle);
        v
    }
}

/// Invalid if any mutation is meant to be invalid; meaning kept if all are;
/// otherwise a valid edge case.
pub fn case_intent(mutations: &[MutationRecord]) -> Intent {
    if mutations.iter().any(|m| m.intent == Intent::Invalid) {
        Intent::Invalid
    } else if !mutations.is_empty()
        && mutations
            .iter()
            .all(|m| m.intent == Intent::ValidMeaningKept)
    {
        Intent::ValidMeaningKept
    } else {
        Intent::ValidEdge
    }
}

/// True when the effect means the target should reject or flag the case.
pub fn effect_is_invalid(e: &Effect) -> bool {
    matches!(e.kind, EffectKind::Invalid | EffectKind::CrossPatient)
}

/// Default `--as-of` date: the latest date in the base bundle. Deterministic,
/// unlike today's date.
pub fn default_as_of(base: &Value) -> String {
    fn walk(v: &Value, best: &mut Option<i64>) {
        match v {
            Value::Object(m) => {
                for (k, x) in m {
                    if let Value::String(s) = x {
                        let timed = matches!(
                            k.as_str(),
                            "effectiveDateTime"
                                | "issued"
                                | "authoredOn"
                                | "start"
                                | "end"
                                | "recorded"
                                | "date"
                        );
                        if timed {
                            if let Some(t) = time::parse(s) {
                                *best = Some(best.map_or(t.epoch, |b| b.max(t.epoch)));
                            }
                        }
                    } else {
                        walk(x, best);
                    }
                }
            }
            Value::Array(a) => a.iter().for_each(|x| walk(x, best)),
            _ => {}
        }
    }
    let mut best = None;
    for e in entries(base) {
        if let Some(r) = resource(e) {
            walk(r, &mut best);
        }
    }
    match best {
        Some(epoch) => time::format_date(epoch),
        None => String::from("2026-01-01"),
    }
}

/// The result of applying a list of mutations.
#[derive(Clone, Debug)]
pub struct Derived {
    pub bundle: Value,
    pub pre_steps: Vec<Value>,
    pub patch: Vec<PatchOp>,
    pub touched: BTreeSet<String>,
    pub records: Vec<MutationRecord>,
}

/// Apply one operator to the current state. Returns the new state.
fn apply_one(
    state: &mut Derived,
    operator: &str,
    target: &TargetRef,
    args: &Map<String, Value>,
    seed: u64,
    as_of: &str,
) -> Result<(), NotApplicable> {
    let op = find(operator).ok_or_else(|| NotApplicable(format!("unknown operator {operator}")))?;
    let applied = {
        let ctx = OpCtx::new(&state.bundle, as_of, &state.touched);
        let mut rng = CaseRng::new(seed);
        (op.apply)(&ctx, target, args, &mut rng)?
    };
    state.records.push(MutationRecord {
        operator: operator.to_string(),
        version: op.info.version,
        target: target.clone(),
        args: applied.args,
        seed,
        summary: applied.summary,
        intent: op.info.intent,
        effect: applied.effect,
        patch_ops: applied.patch.len(),
    });
    state.patch.extend(applied.patch);
    if let Some(pre) = applied.pre_step {
        state.pre_steps.push(pre);
    }
    for k in applied.touched {
        state.touched.insert(k);
    }
    state.bundle = applied.bundle;
    Ok(())
}

/// Apply recorded mutations to the base again, in order. Used by shrinking.
/// Fails when a mutation no longer applies (its target was removed).
pub fn reapply(
    base: &Value,
    mutations: &[MutationRecord],
    as_of: &str,
) -> Result<Derived, NotApplicable> {
    let mut state = Derived {
        bundle: base.clone(),
        pre_steps: Vec::new(),
        patch: Vec::new(),
        touched: BTreeSet::new(),
        records: Vec::new(),
    };
    for m in mutations {
        apply_one(&mut state, &m.operator, &m.target, &m.args, m.seed, as_of)?;
    }
    Ok(state)
}

/// Generate every case of a scenario.
pub fn generate(base: &Value, scenario: &Scenario, as_of: &str) -> Vec<GeneratedCase> {
    let mut master = CaseRng::new(scenario.seed);
    let weights: Vec<u32> = scenario.operators.iter().map(|o| o.weight).collect();
    let mut out = Vec::with_capacity(scenario.cases);
    for n in 1..=scenario.cases {
        let case_seed = master.next_u64();
        let mut rng = CaseRng::new(case_seed);
        let count = 1 + rng.index(scenario.max_mutations_per_case.max(1));
        let mut state = Derived {
            bundle: base.clone(),
            pre_steps: Vec::new(),
            patch: Vec::new(),
            touched: BTreeSet::new(),
            records: Vec::new(),
        };
        let mut not_applicable = Vec::new();
        for _ in 0..count {
            let Some(pick) = rng.weighted(&weights) else {
                break;
            };
            let Some(entry) = scenario.operators.get(pick) else {
                break;
            };
            let mutation_seed = rng.next_u64();
            let Some(op) = find(&entry.id) else {
                not_applicable.push(format!("{}: unknown operator", entry.id));
                continue;
            };
            let targets = {
                let ctx = OpCtx::new(&state.bundle, as_of, &state.touched);
                (op.targets)(&ctx, &entry.select)
            };
            if targets.is_empty() {
                not_applicable.push(format!("{}: no eligible target", entry.id));
                continue;
            }
            let target = targets[rng.index(targets.len())].clone();
            if let Err(NotApplicable(reason)) = apply_one(
                &mut state,
                &entry.id,
                &target,
                &entry.args,
                mutation_seed,
                as_of,
            ) {
                not_applicable.push(format!("{}: {reason}", entry.id));
            }
        }
        out.push(GeneratedCase {
            id: format!("c-{n:04}"),
            seed: case_seed,
            mutations: state.records,
            not_applicable,
            bundle: state.bundle,
            pre_steps: state.pre_steps,
            patch: state.patch,
            touched: state.touched,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Limits;
    use crate::scenario::parse_scenario;
    use std::path::Path;

    fn base() -> Value {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/bundles/patient-a.json"
        );
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn scenario() -> Scenario {
        let text = r#"
base = "x.json"
seed = 7
cases = 20
max_mutations_per_case = 3
[[operator]]
id = "observation.duplicate_with_changed_issued"
[[operator]]
id = "bundle.reorder_entries"
[[operator]]
id = "reference.to_urn_uuid"
select = { resource_type = "Patient" }
[[operator]]
id = "reference.dangling_target"
"#;
        parse_scenario(text, Path::new("s.toml"), &Limits::default()).unwrap()
    }

    #[test]
    fn same_seed_gives_identical_cases() {
        let b = base();
        let s = scenario();
        let a = generate(&b, &s, "2026-01-31");
        let c = generate(&b, &s, "2026-01-31");
        assert_eq!(a.len(), 20);
        for (x, y) in a.iter().zip(c.iter()) {
            assert_eq!(
                crate::json::canonical(&x.bundle).unwrap(),
                crate::json::canonical(&y.bundle).unwrap()
            );
            assert_eq!(x.patch, y.patch);
        }
    }

    #[test]
    fn reapply_reproduces_each_case() {
        let b = base();
        for case in generate(&b, &scenario(), "2026-01-31") {
            let d = reapply(&b, &case.mutations, "2026-01-31").unwrap();
            assert_eq!(d.bundle, case.bundle, "{}", case.id);
            assert_eq!(
                crate::bundle::apply_patch(&b, &case.patch).unwrap(),
                case.bundle,
                "{}",
                case.id
            );
        }
    }

    #[test]
    fn base_is_never_changed() {
        let b = base();
        let before = crate::json::canonical(&b).unwrap();
        let _ = generate(&b, &scenario(), "2026-01-31");
        assert_eq!(crate::json::canonical(&b).unwrap(), before);
    }

    #[test]
    fn default_as_of_is_the_latest_date() {
        assert_eq!(default_as_of(&base()), "2026-01-14");
    }
}
