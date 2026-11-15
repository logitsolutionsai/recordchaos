//! Operator 3: identifier.duplicate_business_identifier.

use serde_json::{Map, Value, json};

use super::{
    Applied, Effect, EffectKind, Intent, NotApplicable, OpCtx, Operator, OperatorInfo, Select,
    TargetRef, args_map, candidates, locate, matches, na, target_key,
};
use crate::bundle::{Editor, resource_ptr};
use crate::rng::CaseRng;

fn first_identifier(res: &Value) -> Option<&Value> {
    let id = res.get("identifier")?.as_array()?.first()?;
    (id.get("system").is_some() && id.get("value").is_some()).then_some(id)
}

fn targets(ctx: &OpCtx<'_>, select: &Select) -> Vec<TargetRef> {
    let all = candidates(ctx);
    let mut out = Vec::new();
    for (_, res, key) in &all {
        if !matches(res, select, &[]) || first_identifier(res).is_none() {
            continue;
        }
        let rt = res.get("resourceType");
        let has_partner = all
            .iter()
            .any(|(_, other, k)| k != key && other.get("resourceType") == rt);
        if has_partner {
            out.push(TargetRef::resource(key.clone()));
        }
    }
    out
}

fn apply(
    ctx: &OpCtx<'_>,
    target: &TargetRef,
    _args: &Map<String, Value>,
    rng: &mut CaseRng,
) -> Result<Applied, NotApplicable> {
    let key = target_key(target)?;
    let (_, res) = locate(ctx, key)?;
    let ident = first_identifier(res)
        .ok_or_else(|| na("no identifier with system and value"))?
        .clone();
    let rt = res.get("resourceType").cloned();
    let partners: Vec<(usize, String)> = candidates(ctx)
        .into_iter()
        .filter(|(_, other, k)| k != key && other.get("resourceType").cloned() == rt)
        .map(|(i, _, k)| (i, k))
        .collect();
    if partners.is_empty() {
        return Err(na("no other resource of the same type"));
    }
    let (pidx, pkey) = partners[rng.index(partners.len())].clone();
    let mut ed = Editor::new(ctx.bundle.clone());
    ed.set(
        &format!("{}/identifier", resource_ptr(pidx)),
        json!([ident.clone()]),
    )?;
    let (bundle, patch) = ed.finish();
    let system = ident.get("system").and_then(Value::as_str).unwrap_or("");
    let value = ident.get("value").and_then(Value::as_str).unwrap_or("");
    Ok(Applied {
        bundle,
        patch,
        effect: Effect::of(EffectKind::NoCrashOnly),
        pre_step: None,
        touched: vec![key.to_string(), pkey.clone()],
        summary: format!("{pkey} given the identifier of {key} ({system}|{value})"),
        args: args_map(&[("identifier", json!(format!("{system}|{value}")))]),
    })
}

pub const DUPLICATE_BUSINESS_IDENTIFIER: Operator = Operator {
    info: OperatorInfo {
        number: 3,
        id: "identifier.duplicate_business_identifier",
        version: 1,
        summary: "Two different resources share one identifier system and value",
        intent: Intent::ValidEdge,
        caught_by: &[],
        strict_rule: None,
        view_effect: "Unchanged count unless declared",
        eligibility: "A resource with an identifier that has a system and value, and another resource of the same type.",
        paths: "/entry/{m}/resource/identifier of the second resource",
        reversible: "Yes: restore the second resource's identifier.",
        before: r#"A {"identifier": [{"system": ".../lab-accession", "value": "ACC-1"}]}, B {"identifier": [{"value": "ACC-2"}]}"#,
        after: r#"B {"identifier": [{"system": ".../lab-accession", "value": "ACC-1"}]}"#,
        args: &[],
        condition: "two resources shared one business identifier",
    },
    targets,
    apply,
};
