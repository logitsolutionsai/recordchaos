//! Property tests: the input is never mutated, meaning-preserving operators
//! keep the resource count, every operator reports exactly the paths it
//! changed, the same seed gives byte-identical output, and ids are stable.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use proptest::prelude::*;
use recordchaos_core::bundle::{apply_patch, resource_count};
use recordchaos_core::json::canonical;
use recordchaos_core::operators::{EffectKind, Intent, OpCtx, REGISTRY};
use recordchaos_core::rng::CaseRng;
use serde_json::Value;

fn fixture(name: &str) -> Value {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/bundles")
        .join(name);
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn operators_are_pure_and_patches_are_exact(op_idx in 0usize..22, fixture_idx in 0usize..3, seed in any::<u64>()) {
        let names = ["patient-a.json", "patient-b.json", "family-c.json"];
        let base = fixture(names[fixture_idx]);
        let before = canonical(&base).unwrap();
        let op = &REGISTRY[op_idx];
        let touched = BTreeSet::new();
        let ctx = OpCtx::new(&base, "2026-03-31", &touched);
        let targets = (op.targets)(&ctx, &Default::default());
        prop_assume!(!targets.is_empty());
        let target = targets[(seed as usize) % targets.len()].clone();
        let args = serde_json::Map::new();
        let first = (op.apply)(&ctx, &target, &args, &mut CaseRng::new(seed));
        let second = (op.apply)(&ctx, &target, &args, &mut CaseRng::new(seed));
        // The input is never mutated.
        prop_assert_eq!(canonical(&base).unwrap(), before);
        match (first, second) {
            (Ok(a), Ok(b)) => {
                // Same seed, byte-identical output.
                prop_assert_eq!(canonical(&a.bundle).unwrap(), canonical(&b.bundle).unwrap());
                prop_assert_eq!(&a.patch, &b.patch);
                // The patch replays to the result, so it lists every change.
                prop_assert_eq!(apply_patch(&base, &a.patch).unwrap(), a.bundle.clone());
                prop_assert!(!a.patch.is_empty(), "{} made a silent no-op", op.info.id);
                // Meaning-preserving operators keep the resource count.
                if op.info.intent == Intent::ValidMeaningKept || a.effect.kind == EffectKind::SameAsBase && op.info.id != "resource.unexpected_type" {
                    prop_assert_eq!(resource_count(&a.bundle), resource_count(&base));
                }
                // Ids are stable: every original resource id is still there,
                // except where an operator removed nothing but ids never change.
                let ids = |v: &Value| -> BTreeSet<String> {
                    recordchaos_core::bundle::entries(v)
                        .iter()
                        .filter_map(|e| recordchaos_core::bundle::resource(e).and_then(recordchaos_core::bundle::resource_key))
                        .collect()
                };
                let old_ids = ids(&base);
                let new_ids = ids(&a.bundle);
                prop_assert!(old_ids.is_subset(&new_ids), "{} lost an id", op.info.id);
                // Exactly the paths it changed: the replay check above shows
                // the patch covers every change; here no op is a no-op. (The
                // old leaf check counted every leaf under /entry/ as covered
                // as soon as the patch had one add, so it could not fail.)
                for i in 0..a.patch.len() {
                    let before = apply_patch(&base, &a.patch[..i]).unwrap();
                    let after = apply_patch(&base, &a.patch[..=i]).unwrap();
                    prop_assert!(
                        before != after,
                        "{}: patch op {} ({:?} {}) changes nothing",
                        op.info.id, i, a.patch[i].op, a.patch[i].path
                    );
                }
            }
            (Err(a), Err(b)) => prop_assert_eq!(a, b),
            _ => prop_assert!(false, "{}: same seed gave different results", op.info.id),
        }
    }
}

#[test]
fn generation_is_deterministic_across_runs() {
    use recordchaos_core::Limits;
    use recordchaos_core::generate::generate;
    use recordchaos_core::scenario::load_scenario;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let s = load_scenario(
        &root.join("scenarios/all-operators.toml"),
        &Limits::default(),
    )
    .unwrap();
    let base = fixture("family-c.json");
    let a = generate(&base, &s, "2026-03-31");
    let b = generate(&base, &s, "2026-03-31");
    let text = |cases: &[recordchaos_core::generate::GeneratedCase]| -> String {
        cases
            .iter()
            .map(|c| canonical(&c.bundle).unwrap())
            .collect::<Vec<_>>()
            .join("")
    };
    assert_eq!(text(&a), text(&b));
    // Every operator in the registry was applied at least once.
    let used: BTreeSet<&str> = a
        .iter()
        .flat_map(|c| c.mutations.iter().map(|m| m.operator.as_str()))
        .collect();
    let missing: Vec<&str> = REGISTRY
        .iter()
        .map(|o| o.info.id)
        .filter(|id| !used.contains(id))
        .collect();
    assert!(missing.is_empty(), "never applied: {missing:?}");
}
