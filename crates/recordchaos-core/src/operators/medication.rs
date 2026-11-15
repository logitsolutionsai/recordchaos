//! Operator 4: medication.conflicting_active_requests.

use serde_json::{Map, Value, json};

use super::{
    Applied, ArgInfo, Effect, EffectKind, Intent, NotApplicable, OpCtx, Operator, OperatorInfo,
    Select, TargetRef, args_map, candidates, locate, matches, na, set_member, sibling_entry,
    sibling_full_url, target_key, unique_id,
};
use crate::bundle::{Editor, entries};
use crate::json::{arg_f64, as_f64, decimal_value, decimals_of, float_value};
use crate::rng::CaseRng;

const DOSE: &str = "/dosageInstruction/0/doseAndRate/0/doseQuantity/value";

fn targets(ctx: &OpCtx<'_>, select: &Select) -> Vec<TargetRef> {
    candidates(ctx)
        .into_iter()
        .filter(|(_, res, _)| {
            matches(res, select, &["MedicationRequest"])
                && res.get("status").and_then(Value::as_str) == Some("active")
                && res.pointer(DOSE).and_then(as_f64).is_some()
        })
        .map(|(_, _, key)| TargetRef::resource(key))
        .collect()
}

fn apply(
    ctx: &OpCtx<'_>,
    target: &TargetRef,
    args: &Map<String, Value>,
    rng: &mut CaseRng,
) -> Result<Applied, NotApplicable> {
    let key = target_key(target)?;
    let (idx, res) = locate(ctx, key)?;
    let id = res
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| na("no id"))?;
    let dose = res.pointer(DOSE).cloned().ok_or_else(|| na("no dose"))?;
    let (num, decimals) = match &dose {
        Value::Number(n) => (
            n.as_f64().ok_or_else(|| na("dose is not a number"))?,
            decimals_of(n),
        ),
        _ => return Err(na("dose is not a number")),
    };
    let factor = arg_f64(args, "dose_factor", 2.0);
    let new_dose = decimal_value(num * factor, decimals);
    let new_id = unique_id(ctx, "MedicationRequest", &format!("{id}-conflict"));
    let mut copy = res.clone();
    set_member(&mut copy, "id", json!(new_id));
    if let Some(v) = copy.pointer_mut(DOSE) {
        *v = new_dose.clone();
    }
    if let Some(t) = copy.pointer_mut("/dosageInstruction/0/text") {
        *t = json!(format!("Synthetic dose {new_dose}"));
    }
    let entry = entries(ctx.bundle)
        .get(idx)
        .ok_or_else(|| na("entry vanished"))?;
    let full_url = sibling_full_url(
        entry.get("fullUrl").and_then(Value::as_str),
        id,
        &new_id,
        rng,
    );
    let mut ed = Editor::new(ctx.bundle.clone());
    ed.add(
        &format!("/entry/{}", idx + 1),
        sibling_entry(entry, &full_url, copy),
    )?;
    let (bundle, patch) = ed.finish();
    Ok(Applied {
        bundle,
        patch,
        effect: Effect::of(EffectKind::NoCrashOnly),
        pre_step: None,
        touched: vec![key.to_string(), format!("MedicationRequest/{new_id}")],
        summary: format!(
            "{key} copied as MedicationRequest/{new_id}, both active, dose {dose} and {new_dose}"
        ),
        args: args_map(&[("dose_factor", float_value(factor))]),
    })
}

pub const CONFLICTING_ACTIVE_REQUESTS: Operator = Operator {
    info: OperatorInfo {
        number: 4,
        id: "medication.conflicting_active_requests",
        version: 1,
        summary: "Two active MedicationRequests for the same medication with different doses",
        intent: Intent::ValidEdge,
        caught_by: &[],
        strict_rule: None,
        view_effect: "Both kept",
        eligibility: "An active MedicationRequest with dosageInstruction[0].doseAndRate[0].doseQuantity.value.",
        paths: "/entry/{n+1} (a new entry after the original)",
        reversible: "Yes: remove the added entry.",
        before: r#"{"id": "medrq-a1", "status": "active", "doseQuantity": {"value": 500}}"#,
        after: r#"+ {"id": "medrq-a1-conflict", "status": "active", "doseQuantity": {"value": 1000}}"#,
        args: &[ArgInfo {
            name: "dose_factor",
            default: "2",
            help: "Multiplies the dose in the conflicting copy.",
        }],
        condition: "two active medication requests for one medication had different doses",
    },
    targets,
    apply,
};
