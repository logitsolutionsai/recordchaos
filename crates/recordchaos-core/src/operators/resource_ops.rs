//! Operator 21: resource.unexpected_type.

use serde_json::{Map, Value, json};

use super::{
    Applied, ArgInfo, Effect, EffectKind, Intent, NotApplicable, OpCtx, Operator, OperatorInfo,
    Select, TargetRef, args_map, na, unique_id,
};
use crate::bundle::{Editor, entries, resource};
use crate::json::arg_str;
use crate::refs::rest_base;
use crate::rng::CaseRng;

/// Resource types this operator can add, each a minimal valid R4 resource.
pub const SUPPORTED_TYPES: &[&str] = &["Specimen", "Device", "Flag"];

fn targets(ctx: &OpCtx<'_>, _select: &Select) -> Vec<TargetRef> {
    if ctx.graph.patients().is_empty() {
        Vec::new()
    } else {
        vec![TargetRef::bundle()]
    }
}

fn template(rt: &str, id: &str, patient_ref: &str) -> Option<Value> {
    let v = match rt {
        "Specimen" => json!({
            "resourceType": "Specimen",
            "id": id,
            "status": "available",
            "type": {"coding": [{"system": "https://example.org/recordchaos/codes", "code": "synthetic-blood-sample"}]},
            "subject": {"reference": patient_ref}
        }),
        "Device" => json!({
            "resourceType": "Device",
            "id": id,
            "status": "active",
            "patient": {"reference": patient_ref}
        }),
        "Flag" => json!({
            "resourceType": "Flag",
            "id": id,
            "status": "active",
            "code": {"coding": [{"system": "https://example.org/recordchaos/codes", "code": "synthetic-flag"}]},
            "subject": {"reference": patient_ref}
        }),
        _ => return None,
    };
    Some(v)
}

fn apply(
    ctx: &OpCtx<'_>,
    _target: &TargetRef,
    args: &Map<String, Value>,
    rng: &mut CaseRng,
) -> Result<Applied, NotApplicable> {
    let rt = arg_str(args, "resource_type", "Specimen");
    if !SUPPORTED_TYPES.contains(&rt.as_str()) {
        return Err(na(format!(
            "resource_type {rt} is not one of {}",
            SUPPORTED_TYPES.join(", ")
        )));
    }
    let patients = ctx.graph.patients();
    let p = *patients
        .first()
        .ok_or_else(|| na("no Patient in the bundle"))?;
    let p_entry = entries(ctx.bundle)
        .get(p)
        .ok_or_else(|| na("patient entry vanished"))?;
    let p_id = resource(p_entry)
        .and_then(|r| r.get("id"))
        .and_then(Value::as_str)
        .ok_or_else(|| na("patient has no id"))?;
    let p_full = p_entry.get("fullUrl").and_then(Value::as_str);
    let patient_ref = match p_full {
        Some(f) if f.starts_with("urn:uuid:") => f.to_string(),
        _ => format!("Patient/{p_id}"),
    };
    let id = unique_id(ctx, &rt, &format!("{}-unexpected-1", rt.to_lowercase()));
    let res = template(&rt, &id, &patient_ref).ok_or_else(|| na("unsupported type"))?;
    let full_url = match p_full.and_then(rest_base) {
        Some(base) => format!("{base}/{rt}/{id}"),
        None => format!("urn:uuid:{}", rng.uuid()),
    };
    let mut ed = Editor::new(ctx.bundle.clone());
    ed.add("/entry/-", json!({"fullUrl": full_url, "resource": res}))?;
    let (bundle, patch) = ed.finish();
    Ok(Applied {
        bundle,
        patch,
        effect: Effect::of(EffectKind::SameAsBase),
        pre_step: None,
        touched: vec![format!("{rt}/{id}")],
        summary: format!("added {rt}/{id} for {patient_ref}, a type the target did not declare"),
        args: args_map(&[("resource_type", json!(rt))]),
    })
}

pub const UNEXPECTED_TYPE: Operator = Operator {
    info: OperatorInfo {
        number: 21,
        id: "resource.unexpected_type",
        version: 1,
        summary: "Adds a valid resource of a type the target did not declare (default Specimen)",
        intent: Intent::ValidEdge,
        caught_by: &[],
        strict_rule: None,
        view_effect: "Ignored, no crash",
        eligibility: "A bundle with at least one Patient. resource_type is Specimen, Device or Flag.",
        paths: "/entry/- (a new last entry)",
        reversible: "Yes: remove the added entry.",
        before: "no Specimen in the bundle",
        after: r#"+ {"resourceType": "Specimen", "id": "specimen-unexpected-1", "status": "available"}"#,
        args: &[ArgInfo {
            name: "resource_type",
            default: "Specimen",
            help: "Specimen, Device or Flag.",
        }],
        condition: "the bundle held an unexpected {resource_type}",
    },
    targets,
    apply,
};
