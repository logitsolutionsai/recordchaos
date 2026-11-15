//! Reference operators: 15 to_urn_uuid, 16 to_absolute_url,
//! 17 dangling_target, 18 cross_patient.

use serde_json::{Map, Value, json};

use super::{
    Applied, ArgInfo, Effect, EffectKind, Intent, NotApplicable, OpCtx, Operator, OperatorInfo,
    Select, TargetRef, args_map, candidates, locate, matches, na, target_key,
};
use crate::bundle::{Editor, entries, resource, resource_key, resource_ptr};
use crate::json::arg_str;
use crate::refs::{Resolution, split_relative};
use crate::rng::CaseRng;
use crate::validate::Layer;

fn full_url_of(ctx: &OpCtx<'_>, idx: usize) -> Option<String> {
    ctx.graph.entry_full_urls.get(idx).cloned().flatten()
}

// 15 -----------------------------------------------------------------------

fn urn_targets(ctx: &OpCtx<'_>, select: &Select) -> Vec<TargetRef> {
    candidates(ctx)
        .into_iter()
        .filter(|(idx, res, _)| {
            matches(res, select, &[])
                && !full_url_of(ctx, *idx).is_some_and(|f| f.starts_with("urn:uuid:"))
                && !ctx.graph.referrers(*idx).is_empty()
        })
        .map(|(_, _, key)| TargetRef::resource(key))
        .collect()
}

fn urn_apply(
    ctx: &OpCtx<'_>,
    target: &TargetRef,
    _args: &Map<String, Value>,
    rng: &mut CaseRng,
) -> Result<Applied, NotApplicable> {
    let key = target_key(target)?;
    let (idx, res) = locate(ctx, key)?;
    let referrers = ctx.graph.referrers(idx);
    if referrers.is_empty() {
        return Err(na(format!("nothing refers to {key}")));
    }
    let urn = format!("urn:uuid:{}", rng.uuid());
    let mut ed = Editor::new(ctx.bundle.clone());
    ed.set(&format!("/entry/{idx}/fullUrl"), json!(urn))?;
    for site in &referrers {
        ed.replace(&site.path, json!(urn))?;
    }
    let (bundle, patch) = ed.finish();
    let rt = res
        .get("resourceType")
        .and_then(Value::as_str)
        .unwrap_or("resource");
    Ok(Applied {
        bundle,
        patch,
        effect: Effect::of(EffectKind::SameAsBase),
        pre_step: None,
        touched: vec![key.to_string()],
        summary: format!(
            "{key}: fullUrl set to {urn}, {} rewritten the same way",
            count_of(referrers.len(), "reference", "references")
        ),
        args: args_map(&[("resource_type", json!(rt)), ("urn", json!(urn))]),
    })
}

pub const TO_URN_UUID: Operator = Operator {
    info: OperatorInfo {
        number: 15,
        id: "reference.to_urn_uuid",
        version: 1,
        summary: "Rewrites a resource's fullUrl to urn:uuid: and every reference to it",
        intent: Intent::ValidEdge,
        caught_by: &[],
        strict_rule: None,
        view_effect: "Same as base",
        eligibility: "A resource whose fullUrl is not already a urn:uuid and that at least one reference points at.",
        paths: "/entry/{n}/fullUrl and every reference to it",
        reversible: "Yes: restore the fullUrl and the references.",
        before: r#"{"fullUrl": "https://example.org/fhir/Patient/pat-a"} ... {"subject": {"reference": "Patient/pat-a"}}"#,
        after: r#"{"fullUrl": "urn:uuid:5b0e7c1a-..."} ... {"subject": {"reference": "urn:uuid:5b0e7c1a-..."}}"#,
        args: &[],
        condition: "the {resource_type} reference used a urn:uuid",
    },
    targets: urn_targets,
    apply: urn_apply,
};

// 16 -----------------------------------------------------------------------

fn absolute_targets(ctx: &OpCtx<'_>, select: &Select) -> Vec<TargetRef> {
    candidates(ctx)
        .into_iter()
        .filter(|(idx, res, _)| {
            matches(res, select, &[])
                && ctx
                    .graph
                    .referrers(*idx)
                    .iter()
                    .any(|s| split_relative(&s.reference).is_some())
        })
        .map(|(_, _, key)| TargetRef::resource(key))
        .collect()
}

fn absolute_apply(
    ctx: &OpCtx<'_>,
    target: &TargetRef,
    args: &Map<String, Value>,
    _rng: &mut CaseRng,
) -> Result<Applied, NotApplicable> {
    let key = target_key(target)?;
    let (idx, _) = locate(ctx, key)?;
    let base = arg_str(args, "base", "https://example.org/fhir");
    let base = base.trim_end_matches('/').to_string();
    if !base.contains("://") {
        return Err(na(format!("base {base:?} is not an absolute URL")));
    }
    let abs = format!("{base}/{key}");
    let referrers = ctx.graph.referrers(idx);
    if referrers.is_empty() {
        return Err(na(format!("nothing refers to {key}")));
    }
    let mut ed = Editor::new(ctx.bundle.clone());
    ed.set(&format!("/entry/{idx}/fullUrl"), json!(abs))?;
    for site in &referrers {
        ed.replace(&site.path, json!(abs))?;
    }
    let (bundle, patch) = ed.finish();
    Ok(Applied {
        bundle,
        patch,
        effect: Effect::of(EffectKind::SameAsBase),
        pre_step: None,
        touched: vec![key.to_string()],
        summary: format!(
            "references to {key} written as {abs} ({})",
            count_of(referrers.len(), "reference", "references")
        ),
        args: args_map(&[("base", json!(base))]),
    })
}

pub const TO_ABSOLUTE_URL: Operator = Operator {
    info: OperatorInfo {
        number: 16,
        id: "reference.to_absolute_url",
        version: 1,
        summary: "Rewrites references to absolute URLs on a declared base",
        intent: Intent::ValidEdge,
        caught_by: &[],
        strict_rule: None,
        view_effect: "Same as base",
        eligibility: "A resource that at least one relative reference (Type/id) points at.",
        paths: "/entry/{n}/fullUrl (set to base/Type/id) and every reference to it",
        reversible: "Yes: restore the relative references.",
        before: r#"{"subject": {"reference": "Patient/pat-a"}}"#,
        after: r#"{"subject": {"reference": "https://example.org/fhir/Patient/pat-a"}}"#,
        args: &[ArgInfo {
            name: "base",
            default: "https://example.org/fhir",
            help: "Absolute base URL for the references.",
        }],
        condition: "references used absolute URLs on {base}",
    },
    targets: absolute_targets,
    apply: absolute_apply,
};

// 17 -----------------------------------------------------------------------

fn dangling_targets(ctx: &OpCtx<'_>, select: &Select) -> Vec<TargetRef> {
    let mut out = Vec::new();
    for (idx, res, key) in candidates(ctx) {
        let type_ok = match &select.resource_type {
            Some(t) => res.get("resourceType").and_then(Value::as_str) == Some(t.as_str()),
            None => true,
        };
        if !type_ok {
            continue;
        }
        for (site, resolution) in ctx.graph.sites_of(idx) {
            if site.in_contained || !matches!(resolution, Resolution::Entry(_)) {
                continue;
            }
            if select.field.as_deref().is_some_and(|f| f != site.field) {
                continue;
            }
            out.push(TargetRef::site(key.clone(), site.rel_path.clone()));
        }
    }
    out
}

fn dangling_apply(
    ctx: &OpCtx<'_>,
    target: &TargetRef,
    _args: &Map<String, Value>,
    rng: &mut CaseRng,
) -> Result<Applied, NotApplicable> {
    let key = target_key(target)?;
    let rel = target
        .path
        .as_deref()
        .ok_or_else(|| na("operator needs a reference path"))?;
    let (idx, res) = locate(ctx, key)?;
    let current = res
        .pointer(rel)
        .and_then(Value::as_str)
        .ok_or_else(|| na(format!("no reference at {rel}")))?;
    let mut missing = if let Some((t, id)) = split_relative(current) {
        format!("{t}/{id}-missing")
    } else if current.starts_with("urn:uuid:") {
        format!("urn:uuid:{}", rng.uuid())
    } else {
        format!("{}-missing", current.trim_end_matches('/'))
    };
    while ctx.graph.resolve(idx, &missing) != Resolution::Unresolved {
        missing.push_str("-x");
    }
    let field = ctx
        .graph
        .sites_of(idx)
        .into_iter()
        .find(|(s, _)| s.rel_path == rel)
        .map(|(s, _)| s.field.clone())
        .unwrap_or_else(|| String::from("reference"));
    let mut ed = Editor::new(ctx.bundle.clone());
    ed.replace(&format!("{}{rel}", resource_ptr(idx)), json!(missing))?;
    let (bundle, patch) = ed.finish();
    Ok(Applied {
        bundle,
        patch,
        effect: Effect::of(EffectKind::Invalid),
        pre_step: None,
        touched: vec![key.to_string()],
        summary: format!("{key}{rel}: {current} changed to {missing}, which is not in the bundle"),
        args: args_map(&[("field", json!(field))]),
    })
}

pub const DANGLING_TARGET: Operator = Operator {
    info: OperatorInfo {
        number: 17,
        id: "reference.dangling_target",
        version: 1,
        summary: "Points a reference at a resource that is not in the bundle",
        intent: Intent::Invalid,
        caught_by: &[Layer::References],
        strict_rule: None,
        view_effect: "Rejected or flagged",
        eligibility: "A reference that resolves inside the bundle, outside contained resources. select.field limits it to one member, such as result.",
        paths: "the reference string",
        reversible: "Yes: restore the reference.",
        before: r#"{"result": [{"reference": "Observation/obs-glucose-3"}]}"#,
        after: r#"{"result": [{"reference": "Observation/obs-glucose-3-missing"}]}"#,
        args: &[],
        condition: "a {field} reference pointed at a resource that is not in the bundle",
    },
    targets: dangling_targets,
    apply: dangling_apply,
};

// 18 -----------------------------------------------------------------------

fn subject_site(ctx: &OpCtx<'_>, idx: usize) -> Option<(String, String)> {
    ctx.graph
        .sites_of(idx)
        .into_iter()
        .find(|(s, r)| {
            s.top_level
                && !s.in_contained
                && (s.field == "subject" || s.field == "patient")
                && matches!(r, Resolution::Entry(_))
        })
        .map(|(s, _)| (s.rel_path.clone(), s.reference.clone()))
}

fn cross_targets(ctx: &OpCtx<'_>, select: &Select) -> Vec<TargetRef> {
    let pats = ctx.graph.patient_map();
    let patients = ctx.graph.patients();
    if patients.len() < 2 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (idx, res, key) in candidates(ctx) {
        if !matches(res, select, &[])
            || ctx.graph.types.get(idx).and_then(|t| t.as_deref()) == Some("Patient")
        {
            continue;
        }
        let Some(p) = pats.get(idx).copied().flatten() else {
            continue;
        };
        if subject_site(ctx, idx).is_none() {
            continue;
        }
        // Only where the crossing is visible: another reference of this
        // resource links to a resource of the same patient.
        let linked = ctx.graph.sites_of(idx).iter().any(|(s, r)| {
            s.field != "subject"
                && s.field != "patient"
                && matches!(r, Resolution::Entry(j) if *j != p && pats.get(*j).copied().flatten() == Some(p))
        });
        if linked {
            out.push(TargetRef::resource(key));
        }
    }
    out
}

fn cross_apply(
    ctx: &OpCtx<'_>,
    target: &TargetRef,
    _args: &Map<String, Value>,
    rng: &mut CaseRng,
) -> Result<Applied, NotApplicable> {
    let key = target_key(target)?;
    let (idx, _) = locate(ctx, key)?;
    let pats = ctx.graph.patient_map();
    let p = pats
        .get(idx)
        .copied()
        .flatten()
        .ok_or_else(|| na("no subject patient"))?;
    let (rel, old) = subject_site(ctx, idx).ok_or_else(|| na("no subject reference"))?;
    let others: Vec<usize> = ctx
        .graph
        .patients()
        .into_iter()
        .filter(|q| *q != p)
        .collect();
    if others.is_empty() {
        return Err(na("only one patient in the bundle"));
    }
    let q = others[rng.index(others.len())];
    let q_entry = entries(ctx.bundle)
        .get(q)
        .ok_or_else(|| na("patient entry vanished"))?;
    let q_key = resource(q_entry)
        .and_then(resource_key)
        .ok_or_else(|| na("patient has no id"))?;
    let new_ref = match q_entry.get("fullUrl").and_then(Value::as_str) {
        Some(f) if f.starts_with("urn:uuid:") => f.to_string(),
        _ => q_key.clone(),
    };
    let mut ed = Editor::new(ctx.bundle.clone());
    ed.replace(&format!("{}{rel}", resource_ptr(idx)), json!(new_ref))?;
    let (bundle, patch) = ed.finish();
    Ok(Applied {
        bundle,
        patch,
        effect: Effect::of(EffectKind::CrossPatient),
        pre_step: None,
        touched: vec![key.to_string()],
        summary: format!(
            "{key}: subject changed from {old} to {new_ref}, another patient in the bundle"
        ),
        args: args_map(&[("other_patient", json!(q_key))]),
    })
}

pub const CROSS_PATIENT: Operator = Operator {
    info: OperatorInfo {
        number: 18,
        id: "reference.cross_patient",
        version: 1,
        summary: "Points one resource's subject at another patient in the bundle",
        intent: Intent::Invalid,
        caught_by: &[Layer::References],
        strict_rule: None,
        view_effect: "Rejected, never merged",
        eligibility: "A resource with a subject or patient reference that also links to another resource of the same patient (for example its encounter), in a bundle with at least two patients.",
        paths: "/entry/{n}/resource/subject/reference",
        reversible: "Yes: restore the subject.",
        before: r#"{"subject": {"reference": "Patient/pat-c1"}, "encounter": {"reference": "Encounter/enc-c1"}}"#,
        after: r#"{"subject": {"reference": "Patient/pat-c2"}, "encounter": {"reference": "Encounter/enc-c1"}}"#,
        args: &[],
        condition: "a resource's subject pointed at another patient while it stayed linked to the first",
    },
    targets: cross_targets,
    apply: cross_apply,
};

/// "1 reference", "3 references".
fn count_of(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

#[cfg(test)]
mod count_tests {
    #[test]
    fn counts_are_singular_for_one() {
        assert_eq!(super::count_of(1, "reference", "references"), "1 reference");
        assert_eq!(
            super::count_of(2, "reference", "references"),
            "2 references"
        );
    }
}
