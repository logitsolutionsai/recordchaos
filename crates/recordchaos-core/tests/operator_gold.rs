//! Every operator against fixtures/operator_gold.json: the patch it produces
//! and the result on each validation layer.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use recordchaos_core::bundle::apply_patch;
use recordchaos_core::operators::{OpCtx, REGISTRY, TargetRef, find};
use recordchaos_core::rng::CaseRng;
use recordchaos_core::validate::{LayerStatus, SchemaValidator, StrictValidator, references};
use serde_json::Value;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn load(rel: &str) -> Value {
    let text = std::fs::read_to_string(root().join(rel)).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn segment_match(pattern: &str, path: &str) -> bool {
    let p: Vec<&str> = pattern.split('/').collect();
    let q: Vec<&str> = path.split('/').collect();
    p.len() == q.len() && p.iter().zip(q.iter()).all(|(a, b)| *a == "*" || a == b)
}

fn status_name(s: LayerStatus) -> &'static str {
    match s {
        LayerStatus::Valid => "valid",
        LayerStatus::Invalid => "invalid",
        LayerStatus::NotChecked => "not_checked",
    }
}

#[test]
fn gold_covers_all_22_operators() {
    let gold = load("fixtures/operator_gold.json");
    let ops = gold["operators"].as_object().unwrap();
    assert_eq!(ops.len(), 22);
    for op in REGISTRY.iter() {
        assert!(
            ops.contains_key(op.info.id),
            "gold is missing {}",
            op.info.id
        );
    }
}

#[test]
fn every_operator_matches_its_gold() {
    let gold = load("fixtures/operator_gold.json");
    let schema = SchemaValidator::bundled().unwrap();
    let strict = StrictValidator::locate(None).ok();
    if strict.is_none() {
        eprintln!(
            "strict layer not checked: Java or validator_cli.jar is absent (reported as not checked, not as passing)"
        );
    }
    let mut failures = Vec::new();
    // Strict checks run after the loop, in one validator run.
    let mut strict_todo: Vec<(String, Value, Value)> = Vec::new();
    for (id, g) in gold["operators"].as_object().unwrap() {
        let op = find(id).unwrap_or_else(|| panic!("unknown operator {id}"));
        let base = load(&format!(
            "fixtures/bundles/{}",
            g["fixture"].as_str().unwrap()
        ));
        let before = base.clone();
        let target: TargetRef = serde_json::from_value(g["target"].clone()).unwrap();
        let args = g["args"].as_object().cloned().unwrap_or_default();
        let touched = BTreeSet::new();
        let ctx = OpCtx::new(&base, "2026-01-14", &touched);
        let targets = (op.targets)(&ctx, &Default::default());
        assert!(
            targets.contains(&target),
            "{id}: gold target {target:?} is not eligible; eligible: {targets:?}"
        );
        let mut rng = CaseRng::new(g["seed"].as_u64().unwrap());
        let applied = (op.apply)(&ctx, &target, &args, &mut rng)
            .unwrap_or_else(|e| panic!("{id}: not applicable: {}", e.0));
        assert_eq!(base, before, "{id} changed its input");
        // The patch replays to the result.
        assert_eq!(
            apply_patch(&base, &applied.patch).unwrap(),
            applied.bundle,
            "{id}: patch does not replay"
        );
        // Exactly the expected paths.
        let got: Vec<(String, String)> = applied
            .patch
            .iter()
            .map(|p| {
                (
                    serde_json::to_value(p.op)
                        .unwrap()
                        .as_str()
                        .unwrap()
                        .to_string(),
                    p.path.clone(),
                )
            })
            .collect();
        if let Some(all) = g.get("patch_all") {
            let (op_name, pattern) = (all[0].as_str().unwrap(), all[1].as_str().unwrap());
            assert!(!got.is_empty(), "{id}: empty patch");
            for (o, p) in &got {
                assert!(
                    o == op_name && segment_match(pattern, p),
                    "{id}: unexpected {o} {p}"
                );
            }
        } else {
            let want: Vec<(String, String)> = g["patch"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| {
                    (
                        x[0].as_str().unwrap().to_string(),
                        x[1].as_str().unwrap().to_string(),
                    )
                })
                .collect();
            let ok = want.len() == got.len()
                && want
                    .iter()
                    .zip(got.iter())
                    .all(|((wo, wp), (go, gp))| (wo == "*" || wo == go) && segment_match(wp, gp));
            if !ok {
                failures.push(format!("{id}: patch {got:?}, gold {want:?}"));
            }
        }
        if g.get("pre_step").and_then(Value::as_bool) == Some(true) {
            assert!(applied.pre_step.is_some(), "{id}: expected a first step");
        }
        // Layers.
        let v = &g["validity"];
        let schema_got = status_name(schema.validate(&applied.bundle).status);
        if v["schema"] != "any" && v["schema"].as_str() != Some(schema_got) {
            failures.push(format!("{id}: schema {schema_got}, gold {}", v["schema"]));
        }
        let refs_got = status_name(references::check(&applied.bundle).status);
        if v["references"] != "any" && v["references"].as_str() != Some(refs_got) {
            failures.push(format!(
                "{id}: references {refs_got}, gold {}",
                v["references"]
            ));
        }
        if strict.is_some() {
            strict_todo.push((id.clone(), applied.bundle.clone(), v["strict"].clone()));
        }
    }
    if let Some(s) = &strict {
        let bundles: Vec<&Value> = strict_todo.iter().map(|(_, b, _)| b).collect();
        let got = s.validate_many(&bundles);
        assert_eq!(got.len(), strict_todo.len());
        for ((id, _, want), layer) in strict_todo.iter().zip(got) {
            let strict_got = status_name(layer.status);
            if want != "any" && want.as_str() != Some(strict_got) {
                failures.push(format!("{id}: strict {strict_got}, gold {want}"));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "gold mismatches:\n{}",
        failures.join("\n")
    );
}

#[test]
fn fixtures_are_clean_baselines() {
    let schema = SchemaValidator::bundled().unwrap();
    for f in ["patient-a.json", "patient-b.json", "family-c.json"] {
        let b = load(&format!("fixtures/bundles/{f}"));
        let s = schema.validate(&b);
        assert_eq!(s.status, LayerStatus::Valid, "{f}: {:?}", s.issues);
        let r = references::check(&b);
        assert_eq!(r.status, LayerStatus::Valid, "{f}: {:?}", r.issues);
        assert!(
            recordchaos_core::synthetic::scan(&b).is_empty(),
            "{f} has real-looking values"
        );
    }
}

#[test]
fn strict_layer_catches_operators_9_and_14_only_there() {
    let Ok(strict) = StrictValidator::locate(None) else {
        eprintln!(
            "skipped: Java or validator_cli.jar is absent; the strict layer is reported as not checked"
        );
        return;
    };
    let schema = SchemaValidator::bundled().unwrap();
    let base = load("fixtures/bundles/patient-a.json");
    let mut caught: Vec<(&str, Value)> = Vec::new();
    for (id, key) in [
        ("time.period_end_before_start", "Encounter/enc-a1"),
        ("observation.remove_status", "Observation/obs-glucose-1"),
    ] {
        let op = find(id).unwrap();
        let touched = BTreeSet::new();
        let ctx = OpCtx::new(&base, "2026-01-14", &touched);
        let applied = (op.apply)(
            &ctx,
            &TargetRef::resource(key),
            &Default::default(),
            &mut CaseRng::new(1),
        )
        .unwrap();
        assert_eq!(
            schema.validate(&applied.bundle).status,
            LayerStatus::Valid,
            "{id}: schema should not catch it"
        );
        assert_eq!(
            references::check(&applied.bundle).status,
            LayerStatus::Valid,
            "{id}: references should not catch it"
        );
        caught.push((id, applied.bundle));
    }
    // Both bundles and the clean base in one validator run; each keeps its
    // own result.
    let mut bundles: Vec<&Value> = caught.iter().map(|(_, b)| b).collect();
    bundles.push(&base);
    let got = strict.validate_many(&bundles);
    for ((id, _), layer) in caught.iter().zip(&got) {
        assert_eq!(
            layer.status,
            LayerStatus::Invalid,
            "{id}: strict should catch it"
        );
    }
    assert_eq!(got[2].status, LayerStatus::Valid, "the base: {:?}", got[2]);
}

/// Apply one operator to patient-a with a fixed target and args.
fn apply_on_a(id: &str, target: Value, args: Value) -> Value {
    let base = load("fixtures/bundles/patient-a.json");
    let op = find(id).unwrap();
    let target: TargetRef = serde_json::from_value(target).unwrap();
    let args = args.as_object().cloned().unwrap_or_default();
    let touched = BTreeSet::new();
    let ctx = OpCtx::new(&base, "2026-01-14", &touched);
    (op.apply)(&ctx, &target, &args, &mut CaseRng::new(1))
        .unwrap_or_else(|e| panic!("{id}: not applicable: {}", e.0))
        .bundle
}

fn resource<'a>(bundle: &'a Value, id: &str) -> &'a Value {
    bundle["entry"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| &e["resource"])
        .find(|r| r["id"] == id)
        .unwrap_or_else(|| panic!("no resource {id}"))
}

/// The gold file checks which paths change, not the values written. These
/// values were worked out by hand from patient-a.json, the R4 datatypes page
/// (instant, dateTime) and UCUM, not taken from the code's output.
#[test]
fn operator_values_match_hand_checked_expectations() {
    use serde_json::json;
    // 8: 09:12 UTC is 14:12 at +05:00, the same instant.
    let b = apply_on_a(
        "time.timezone_offset_shift",
        json!({"key": "Observation/obs-glucose-1", "path": "/issued"}),
        json!({"offset": "+05:00"}),
    );
    assert_eq!(
        resource(&b, "obs-glucose-1")["issued"],
        "2026-01-02T14:12:00+05:00"
    );
    // 9: enc-a1 runs 08:00 to 09:30; swapped it ends before it starts (per-1).
    let b = apply_on_a(
        "time.period_end_before_start",
        json!({"key": "Encounter/enc-a1"}),
        json!({}),
    );
    let p = &resource(&b, "enc-a1")["period"];
    assert_eq!(p["start"], "2026-01-02T09:30:00Z");
    assert_eq!(p["end"], "2026-01-02T08:00:00Z");
    // 7: a dateTime may be a date only; issued (an instant) is untouched.
    let b = apply_on_a(
        "time.date_only_precision",
        json!({"key": "Observation/obs-glucose-1"}),
        json!({}),
    );
    let o = resource(&b, "obs-glucose-1");
    assert_eq!(o["effectiveDateTime"], "2026-01-02");
    assert_eq!(o["issued"], "2026-01-02T09:12:00Z");
    // 10: 70 g/L is 70 mg/mL (factor 1), so the number must not change.
    let b = apply_on_a(
        "quantity.equivalent_unit",
        json!({"key": "Observation/obs-protein-1"}),
        json!({}),
    );
    let q = &resource(&b, "obs-protein-1")["valueQuantity"];
    assert_eq!(q["code"], "mg/mL");
    assert_eq!(q["unit"], "mg/mL");
    assert_eq!(q["system"], "http://unitsofmeasure.org");
    assert_eq!(q["value"].to_string(), "70");
    // 11: unit text kept, code and system gone.
    let b = apply_on_a(
        "quantity.remove_unit_code",
        json!({"key": "Observation/obs-glucose-1"}),
        json!({}),
    );
    let q = &resource(&b, "obs-glucose-1")["valueQuantity"];
    assert_eq!(q["unit"], "mmol/L");
    assert!(q.get("code").is_none() && q.get("system").is_none());
    // 12: obs-6, dataAbsentReason only when there is no value[x].
    let b = apply_on_a(
        "observation.data_absent_reason",
        json!({"key": "Observation/obs-weight-1"}),
        json!({}),
    );
    let o = resource(&b, "obs-weight-1");
    assert!(o.get("valueQuantity").is_none());
    assert_eq!(
        o["dataAbsentReason"]["coding"][0]["system"],
        "http://terminology.hl7.org/CodeSystem/data-absent-reason"
    );
    // 5: issued 2026-01-02T09:12:00Z minus 30 days.
    let b = apply_on_a(
        "time.backdate_effective",
        json!({"key": "Observation/obs-glucose-1"}),
        json!({"days": 30}),
    );
    assert_eq!(
        resource(&b, "obs-glucose-1")["effectiveDateTime"],
        "2025-12-03T09:12:00Z"
    );
    // 1: a corrected copy issued 3 days after obs-glucose-3 (2026-01-10T09:12:00Z).
    let b = apply_on_a(
        "observation.duplicate_with_changed_issued",
        json!({"key": "Observation/obs-glucose-3"}),
        json!({"days_later": 3}),
    );
    let copy = b["entry"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| &e["resource"])
        .find(|r| r["status"] == "corrected")
        .expect("no corrected copy");
    assert_ne!(copy["id"], "obs-glucose-3");
    assert_eq!(copy["issued"], "2026-01-13T09:12:00Z");
    assert_ne!(
        copy["valueQuantity"]["value"],
        resource(&b, "obs-glucose-3")["valueQuantity"]["value"]
    );
    // 15: a urn:uuid fullUrl must be a lowercase RFC 4122 UUID, and every
    // reference to the Patient must follow it.
    let b = apply_on_a(
        "reference.to_urn_uuid",
        json!({"key": "Patient/pat-a"}),
        json!({}),
    );
    let full = b["entry"][0]["fullUrl"].as_str().unwrap();
    let uuid = full.strip_prefix("urn:uuid:").expect("not a urn:uuid");
    assert_eq!(uuid.len(), 36);
    assert!(
        uuid.chars()
            .all(|c| c == '-' || c.is_ascii_digit() || ('a'..='f').contains(&c))
    );
    let text = b.to_string();
    assert!(
        !text.contains("\"Patient/pat-a\""),
        "a reference still points at Patient/pat-a"
    );
    assert!(!text.contains("example.org/fhir/Patient/pat-a"));
}
