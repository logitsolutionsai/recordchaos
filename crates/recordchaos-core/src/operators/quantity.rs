//! Quantity operators: 10 equivalent_unit, 11 remove_unit_code.

use serde_json::{Map, Value, json};

use super::{
    Applied, Effect, EffectKind, Intent, NotApplicable, OpCtx, Operator, OperatorInfo, Select,
    TargetRef, args_map, candidates, locate, matches, na, target_key,
};
use crate::bundle::{Editor, resource_ptr};
use crate::rng::CaseRng;

/// UCUM unit pairs with a conversion factor of exactly 1, so the number stays
/// the same and only the unit changes.
pub const EQUIVALENT_UNITS: &[(&str, &str)] = &[
    ("g/L", "mg/mL"),
    ("mg/mL", "g/L"),
    ("mmol/L", "umol/mL"),
    ("umol/mL", "mmol/L"),
    ("mg/L", "ug/mL"),
    ("ug/mL", "mg/L"),
    ("mmol/mol", "umol/mmol"),
    ("umol/mmol", "mmol/mol"),
];

fn equivalent(code: &str) -> Option<&'static str> {
    EQUIVALENT_UNITS
        .iter()
        .find(|(from, to)| *from == code && from != to)
        .map(|(_, to)| *to)
}

fn equivalent_targets(ctx: &OpCtx<'_>, select: &Select) -> Vec<TargetRef> {
    candidates(ctx)
        .into_iter()
        .filter(|(_, res, _)| {
            matches(res, select, &["Observation"])
                && res
                    .pointer("/valueQuantity/code")
                    .and_then(Value::as_str)
                    .and_then(equivalent)
                    .is_some()
        })
        .map(|(_, _, key)| TargetRef::resource(key))
        .collect()
}

fn equivalent_apply(
    ctx: &OpCtx<'_>,
    target: &TargetRef,
    _args: &Map<String, Value>,
    _rng: &mut CaseRng,
) -> Result<Applied, NotApplicable> {
    let key = target_key(target)?;
    let (idx, res) = locate(ctx, key)?;
    let code = res
        .pointer("/valueQuantity/code")
        .and_then(Value::as_str)
        .ok_or_else(|| na("no valueQuantity.code"))?;
    let to = equivalent(code).ok_or_else(|| na(format!("no equivalent unit for {code}")))?;
    let ptr = format!("{}/valueQuantity", resource_ptr(idx));
    let mut ed = Editor::new(ctx.bundle.clone());
    ed.replace(&format!("{ptr}/code"), json!(to))?;
    if res.pointer("/valueQuantity/unit").is_some() {
        ed.replace(&format!("{ptr}/unit"), json!(to))?;
    }
    if res.pointer("/valueQuantity/system").is_none() {
        ed.add(&format!("{ptr}/system"), json!("http://unitsofmeasure.org"))?;
    }
    let (bundle, patch) = ed.finish();
    Ok(Applied {
        bundle,
        patch,
        effect: Effect::of(EffectKind::SameAsBase),
        pre_step: None,
        touched: vec![key.to_string()],
        summary: format!("{key}: unit {code} written as {to}, the same value"),
        args: args_map(&[("from", json!(code)), ("to", json!(to))]),
    })
}

pub const EQUIVALENT_UNIT: Operator = Operator {
    info: OperatorInfo {
        number: 10,
        id: "quantity.equivalent_unit",
        version: 1,
        summary: "Rewrites a quantity in an equivalent UCUM unit (1 g/L as 1 mg/mL)",
        intent: Intent::ValidEdge,
        caught_by: &[],
        strict_rule: None,
        view_effect: "Same value after conversion",
        eligibility: "An Observation valueQuantity whose UCUM code has a factor-1 equivalent: g/L, mg/mL, mmol/L, umol/mL, mg/L, ug/mL, mmol/mol, umol/mmol.",
        paths: "/entry/{n}/resource/valueQuantity/code, /unit",
        reversible: "Yes: write the original unit again.",
        before: r#"{"valueQuantity": {"value": 70, "unit": "g/L", "code": "g/L"}}"#,
        after: r#"{"valueQuantity": {"value": 70, "unit": "mg/mL", "code": "mg/mL"}}"#,
        args: &[],
        condition: "a quantity used an equivalent unit ({from} as {to})",
    },
    targets: equivalent_targets,
    apply: equivalent_apply,
};

fn unit_code_targets(ctx: &OpCtx<'_>, select: &Select) -> Vec<TargetRef> {
    candidates(ctx)
        .into_iter()
        .filter(|(_, res, _)| {
            matches(res, select, &["Observation"])
                && res.pointer("/valueQuantity/unit").is_some()
                && (res.pointer("/valueQuantity/code").is_some()
                    || res.pointer("/valueQuantity/system").is_some())
        })
        .map(|(_, _, key)| TargetRef::resource(key))
        .collect()
}

fn unit_code_apply(
    ctx: &OpCtx<'_>,
    target: &TargetRef,
    _args: &Map<String, Value>,
    _rng: &mut CaseRng,
) -> Result<Applied, NotApplicable> {
    let key = target_key(target)?;
    let (idx, res) = locate(ctx, key)?;
    let ptr = format!("{}/valueQuantity", resource_ptr(idx));
    let mut ed = Editor::new(ctx.bundle.clone());
    for member in ["code", "system"] {
        if res.pointer(&format!("/valueQuantity/{member}")).is_some() {
            ed.remove(&format!("{ptr}/{member}"))?;
        }
    }
    if ed.op_count() == 0 {
        return Err(na("no code or system to remove"));
    }
    let (bundle, patch) = ed.finish();
    let unit = res
        .pointer("/valueQuantity/unit")
        .and_then(Value::as_str)
        .unwrap_or("");
    Ok(Applied {
        bundle,
        patch,
        effect: Effect::of(EffectKind::SameAsBaseOrRejected),
        pre_step: None,
        touched: vec![key.to_string()],
        summary: format!("{key}: valueQuantity code and system removed, unit {unit:?} kept"),
        args: Map::new(),
    })
}

pub const REMOVE_UNIT_CODE: Operator = Operator {
    info: OperatorInfo {
        number: 11,
        id: "quantity.remove_unit_code",
        version: 1,
        summary: "Removes valueQuantity.code and system, keeps unit",
        intent: Intent::ValidEdge,
        caught_by: &[],
        strict_rule: None,
        view_effect: "Same as base, or flagged",
        eligibility: "An Observation valueQuantity with a unit and a code or system.",
        paths: "/entry/{n}/resource/valueQuantity/code, /system",
        reversible: "Yes: restore code and system.",
        before: r#"{"valueQuantity": {"value": 6.8, "unit": "mmol/L", "system": "http://unitsofmeasure.org", "code": "mmol/L"}}"#,
        after: r#"{"valueQuantity": {"value": 6.8, "unit": "mmol/L"}}"#,
        args: &[],
        condition: "a quantity had a unit but no unit code",
    },
    targets: unit_code_targets,
    apply: unit_code_apply,
};
