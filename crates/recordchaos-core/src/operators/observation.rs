//! Observation operators: 1 duplicate_with_changed_issued,
//! 2 correction_sequence, 12 data_absent_reason, 13 remove_code,
//! 14 remove_status.

use serde_json::{Map, Value, json};

use super::{
    Applied, ArgInfo, Effect, EffectKind, Intent, NotApplicable, OpCtx, Operator, OperatorInfo,
    Select, TargetRef, args_map, candidates, latest_by_code, locate, matches, na, set_member,
    sibling_entry, sibling_full_url, target_key, unique_id, value_field,
};
use crate::bundle::{Editor, entries, first_code, resource_ptr};
use crate::json::{arg_f64, arg_i64, as_f64, decimal_value, decimals_of};
use crate::rng::CaseRng;
use crate::time;
use crate::validate::Layer;

const OBS: &[&str] = &["Observation"];

/// Latest Observation per code that has a numeric valueQuantity.
fn latest_quantity_targets(ctx: &OpCtx<'_>, select: &Select) -> Vec<TargetRef> {
    let latest = latest_by_code(ctx.bundle);
    let mut out = Vec::new();
    for (_, res, key) in candidates(ctx) {
        if !matches(res, select, OBS) {
            continue;
        }
        if res
            .pointer("/valueQuantity/value")
            .and_then(as_f64)
            .is_none()
        {
            continue;
        }
        let Some(code) = first_code(res, "code") else {
            continue;
        };
        if latest.get(&code) == Some(&key) {
            out.push(TargetRef::resource(key));
        }
    }
    out
}

fn quantity_parts(res: &Value) -> Result<(Value, f64, usize), NotApplicable> {
    let v = res
        .pointer("/valueQuantity/value")
        .cloned()
        .ok_or_else(|| na("no valueQuantity.value"))?;
    let (num, decimals) = match &v {
        Value::Number(n) => (
            n.as_f64().ok_or_else(|| na("value is not a number"))?,
            decimals_of(n),
        ),
        _ => return Err(na("value is not a number")),
    };
    Ok((v, num, decimals.max(1)))
}

// 1 ------------------------------------------------------------------------

fn duplicate_apply(
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
    let code = first_code(res, "code").ok_or_else(|| na("no code"))?;
    let (old_value, old_num, decimals) = quantity_parts(res)?;
    let days = arg_i64(args, "days_later", 3);
    let delta = arg_f64(args, "value_delta", -1.1);
    let old_issued = res
        .get("issued")
        .and_then(Value::as_str)
        .or_else(|| super::effective_text(res))
        .ok_or_else(|| na("no issued or effective time"))?
        .to_string();
    let new_issued =
        time::add_days(&old_issued, days).ok_or_else(|| na("cannot read the issued time"))?;
    let new_value = decimal_value(old_num + delta, decimals);
    let new_id = unique_id(ctx, "Observation", &format!("{id}-corrected"));

    let mut copy = res.clone();
    set_member(&mut copy, "id", json!(new_id));
    set_member(&mut copy, "status", json!("corrected"));
    set_member(&mut copy, "issued", json!(new_issued));
    if let Some(q) = copy.get_mut("valueQuantity") {
        set_member(q, "value", new_value.clone());
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
    let new_entry = sibling_entry(entry, &full_url, copy);

    let mut ed = Editor::new(ctx.bundle.clone());
    ed.add(&format!("/entry/{}", idx + 1), new_entry)?;
    let (bundle, patch) = ed.finish();

    let unit = res
        .pointer("/valueQuantity/unit")
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut effect = Effect::of(EffectKind::LatestCorrection);
    effect.code = Some(code);
    effect.id = Some(new_id.clone());
    effect.value = Some(new_value.clone());
    effect.unit = unit;
    effect.issued = Some(new_issued);
    effect.old_id = Some(id.to_string());
    effect.old_value = Some(old_value.clone());
    effect.old_issued = Some(old_issued);
    effect.match_id = true;
    Ok(Applied {
        bundle,
        patch,
        effect,
        pre_step: None,
        touched: vec![key.to_string(), format!("Observation/{new_id}")],
        summary: format!(
            "{key} copied as Observation/{new_id}: status corrected, issued {days} days later, value {old_value} to {new_value}"
        ),
        args: args_map(&[
            ("days_later", json!(days)),
            ("value_delta", crate::json::float_value(delta)),
        ]),
    })
}

pub const DUPLICATE_WITH_CHANGED_ISSUED: Operator = Operator {
    info: OperatorInfo {
        number: 1,
        id: "observation.duplicate_with_changed_issued",
        version: 1,
        summary: "Adds a corrected copy (new id, status corrected, later issued, new value)",
        intent: Intent::ValidEdge,
        caught_by: &[],
        strict_rule: None,
        view_effect: "Latest becomes the copy",
        eligibility: "The latest Observation for its code (by effective time, then issued) with a numeric valueQuantity.value and an issued or effective time.",
        paths: "/entry/{n+1} (a new entry after the original)",
        reversible: "Yes: remove the added entry.",
        before: r#"{"id": "obs-glucose-3", "status": "final", "issued": "2026-01-10T09:12:00Z", "valueQuantity": {"value": 7.2}}"#,
        after: r#"+ {"id": "obs-glucose-3-corrected", "status": "corrected", "issued": "2026-01-13T09:12:00Z", "valueQuantity": {"value": 6.1}}"#,
        args: &[
            ArgInfo {
                name: "days_later",
                default: "3",
                help: "Days between the original issued time and the correction.",
            },
            ArgInfo {
                name: "value_delta",
                default: "-1.1",
                help: "Added to the value in the corrected copy.",
            },
        ],
        condition: "a correction arrived {days_later} days later",
    },
    targets: latest_quantity_targets,
    apply: duplicate_apply,
};

// 2 ------------------------------------------------------------------------

fn sequence_apply(
    ctx: &OpCtx<'_>,
    target: &TargetRef,
    args: &Map<String, Value>,
    _rng: &mut CaseRng,
) -> Result<Applied, NotApplicable> {
    let key = target_key(target)?;
    let (idx, res) = locate(ctx, key)?;
    let id = res
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| na("no id"))?;
    let code = first_code(res, "code").ok_or_else(|| na("no code"))?;
    let (old_value, old_num, decimals) = quantity_parts(res)?;
    let hours = arg_i64(args, "hours_later", 6);
    let delta = arg_f64(args, "value_delta", -0.9);
    let base_time = res
        .get("issued")
        .and_then(Value::as_str)
        .or_else(|| super::effective_text(res))
        .ok_or_else(|| na("no issued or effective time"))?
        .to_string();
    let new_issued = time::add_seconds(&base_time, hours * 3_600)
        .ok_or_else(|| na("cannot read the issued time"))?;
    let new_value = decimal_value(old_num + delta, decimals);
    let ptr = resource_ptr(idx);

    // Step 1: the same bundle with the reading marked preliminary.
    let mut pre = Editor::new(ctx.bundle.clone());
    pre.set(&format!("{ptr}/status"), json!("preliminary"))?;
    let (pre_bundle, _) = pre.finish();

    // Step 2: the final version with a new value and a later issued time.
    let mut ed = Editor::new(ctx.bundle.clone());
    ed.set(&format!("{ptr}/status"), json!("final"))?;
    ed.set(&format!("{ptr}/issued"), json!(new_issued))?;
    ed.replace(&format!("{ptr}/valueQuantity/value"), new_value.clone())?;
    let (bundle, patch) = ed.finish();

    let mut effect = Effect::of(EffectKind::LatestCorrection);
    effect.code = Some(code);
    effect.id = Some(id.to_string());
    effect.value = Some(new_value.clone());
    effect.unit = res
        .pointer("/valueQuantity/unit")
        .and_then(Value::as_str)
        .map(str::to_string);
    effect.issued = Some(new_issued);
    effect.old_id = Some(id.to_string());
    effect.old_value = Some(old_value.clone());
    effect.match_id = false;
    Ok(Applied {
        bundle,
        patch,
        effect,
        pre_step: Some(pre_bundle),
        touched: vec![key.to_string()],
        summary: format!(
            "{key} sent twice: first preliminary with {old_value}, then final with {new_value}, issued {hours} hours later"
        ),
        args: args_map(&[
            ("hours_later", json!(hours)),
            ("value_delta", crate::json::float_value(delta)),
        ]),
    })
}

pub const CORRECTION_SEQUENCE: Operator = Operator {
    info: OperatorInfo {
        number: 2,
        id: "observation.correction_sequence",
        version: 1,
        summary: "Sends the bundle twice: first preliminary, then final with a new value",
        intent: Intent::ValidEdge,
        caught_by: &[],
        strict_rule: None,
        view_effect: "Latest is the final value",
        eligibility: "The latest Observation for its code with a numeric valueQuantity.value.",
        paths: "/entry/{n}/resource/status, /issued, /valueQuantity/value (second step)",
        reversible: "Yes: ingest only the first step and restore the value.",
        before: r#"{"id": "obs-glucose-3", "status": "final", "valueQuantity": {"value": 7.2}}"#,
        after: r#"step 1 {"status": "preliminary", "value": 7.2}, step 2 {"status": "final", "value": 6.3}"#,
        args: &[
            ArgInfo {
                name: "hours_later",
                default: "6",
                help: "Hours between the two versions' issued times.",
            },
            ArgInfo {
                name: "value_delta",
                default: "-0.9",
                help: "Added to the value in the final version.",
            },
        ],
        condition: "a final version followed a preliminary one",
    },
    targets: latest_quantity_targets,
    apply: sequence_apply,
};

// 12 -----------------------------------------------------------------------

fn value_targets(ctx: &OpCtx<'_>, select: &Select) -> Vec<TargetRef> {
    candidates(ctx)
        .into_iter()
        .filter(|(_, res, _)| matches(res, select, OBS) && value_field(res).is_some())
        .map(|(_, _, key)| TargetRef::resource(key))
        .collect()
}

fn absent_apply(
    ctx: &OpCtx<'_>,
    target: &TargetRef,
    _args: &Map<String, Value>,
    _rng: &mut CaseRng,
) -> Result<Applied, NotApplicable> {
    let key = target_key(target)?;
    let (idx, res) = locate(ctx, key)?;
    let field = value_field(res).ok_or_else(|| na("no value[x]"))?;
    let ptr = resource_ptr(idx);
    let mut ed = Editor::new(ctx.bundle.clone());
    ed.remove(&format!("{ptr}/{field}"))?;
    ed.set(
        &format!("{ptr}/dataAbsentReason"),
        json!({"coding": [{
            "system": "http://terminology.hl7.org/CodeSystem/data-absent-reason",
            "code": "unknown",
            "display": "Unknown"
        }]}),
    )?;
    let (bundle, patch) = ed.finish();
    Ok(Applied {
        bundle,
        patch,
        effect: Effect::of(EffectKind::NoCrashOnly),
        pre_step: None,
        touched: vec![key.to_string()],
        summary: format!("{key}: {field} removed, dataAbsentReason unknown added"),
        args: Map::new(),
    })
}

pub const DATA_ABSENT_REASON: Operator = Operator {
    info: OperatorInfo {
        number: 12,
        id: "observation.data_absent_reason",
        version: 1,
        summary: "Removes the value and adds dataAbsentReason",
        intent: Intent::ValidEdge,
        caught_by: &[],
        strict_rule: None,
        view_effect: "No value for that reading",
        eligibility: "An Observation with a value[x] element.",
        paths: "/entry/{n}/resource/value[x] (removed), /dataAbsentReason (added)",
        reversible: "Yes: restore value[x] and remove dataAbsentReason.",
        before: r#"{"valueQuantity": {"value": 72.5, "unit": "kg"}}"#,
        after: r#"{"dataAbsentReason": {"coding": [{"code": "unknown"}]}}"#,
        args: &[],
        condition: "a reading had a data absent reason instead of a value",
    },
    targets: value_targets,
    apply: absent_apply,
};

// 13, 14 -------------------------------------------------------------------

fn member_targets(ctx: &OpCtx<'_>, select: &Select, member: &str) -> Vec<TargetRef> {
    candidates(ctx)
        .into_iter()
        .filter(|(_, res, _)| matches(res, select, OBS) && res.get(member).is_some())
        .map(|(_, _, key)| TargetRef::resource(key))
        .collect()
}

fn code_targets(ctx: &OpCtx<'_>, select: &Select) -> Vec<TargetRef> {
    member_targets(ctx, select, "code")
}

fn status_targets(ctx: &OpCtx<'_>, select: &Select) -> Vec<TargetRef> {
    member_targets(ctx, select, "status")
}

fn remove_member(
    ctx: &OpCtx<'_>,
    target: &TargetRef,
    member: &str,
) -> Result<Applied, NotApplicable> {
    let key = target_key(target)?;
    let (idx, res) = locate(ctx, key)?;
    if res.get(member).is_none() {
        return Err(na(format!("{key} has no {member}")));
    }
    let mut ed = Editor::new(ctx.bundle.clone());
    ed.remove(&format!("{}/{member}", resource_ptr(idx)))?;
    let (bundle, patch) = ed.finish();
    Ok(Applied {
        bundle,
        patch,
        effect: Effect::of(EffectKind::Invalid),
        pre_step: None,
        touched: vec![key.to_string()],
        summary: format!("{key}: {member} removed"),
        args: Map::new(),
    })
}

fn remove_code_apply(
    ctx: &OpCtx<'_>,
    target: &TargetRef,
    _args: &Map<String, Value>,
    _rng: &mut CaseRng,
) -> Result<Applied, NotApplicable> {
    remove_member(ctx, target, "code")
}

fn remove_status_apply(
    ctx: &OpCtx<'_>,
    target: &TargetRef,
    _args: &Map<String, Value>,
    _rng: &mut CaseRng,
) -> Result<Applied, NotApplicable> {
    remove_member(ctx, target, "status")
}

pub const REMOVE_CODE: Operator = Operator {
    info: OperatorInfo {
        number: 13,
        id: "observation.remove_code",
        version: 1,
        summary: "Removes Observation.code",
        intent: Intent::Invalid,
        caught_by: &[Layer::Schema],
        strict_rule: None,
        view_effect: "Rejected",
        eligibility: "An Observation with a code.",
        paths: "/entry/{n}/resource/code",
        reversible: "Yes: restore code.",
        before: r#"{"resourceType": "Observation", "code": {...}, "status": "final"}"#,
        after: r#"{"resourceType": "Observation", "status": "final"}"#,
        args: &[],
        condition: "an Observation had no code",
    },
    targets: code_targets,
    apply: remove_code_apply,
};

pub const REMOVE_STATUS: Operator = Operator {
    info: OperatorInfo {
        number: 14,
        id: "observation.remove_status",
        version: 1,
        summary: "Removes Observation.status",
        intent: Intent::Invalid,
        caught_by: &[Layer::Strict],
        strict_rule: Some("Observation.status 1..1"),
        view_effect: "Rejected or flagged",
        eligibility: "An Observation with a status.",
        paths: "/entry/{n}/resource/status",
        reversible: "Yes: restore status.",
        before: r#"{"resourceType": "Observation", "status": "final", "code": {...}}"#,
        after: r#"{"resourceType": "Observation", "code": {...}}"#,
        args: &[],
        condition: "an Observation had no status",
    },
    targets: status_targets,
    apply: remove_status_apply,
};
