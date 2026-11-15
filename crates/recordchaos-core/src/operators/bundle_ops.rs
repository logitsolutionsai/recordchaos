//! Bundle operators: 19 reorder_entries, 20 duplicate_full_url.

use serde_json::{Map, Value, json};

use super::{
    Applied, Effect, EffectKind, Intent, NotApplicable, OpCtx, Operator, OperatorInfo, Select,
    TargetRef, args_map, candidates, locate, matches, na, target_key,
};
use crate::bundle::{Editor, entries};
use crate::rng::CaseRng;
use crate::validate::Layer;

// 19 -----------------------------------------------------------------------

fn reorder_targets(ctx: &OpCtx<'_>, _select: &Select) -> Vec<TargetRef> {
    if entries(ctx.bundle).len() >= 2 {
        vec![TargetRef::bundle()]
    } else {
        Vec::new()
    }
}

fn reorder_apply(
    ctx: &OpCtx<'_>,
    _target: &TargetRef,
    _args: &Map<String, Value>,
    rng: &mut CaseRng,
) -> Result<Applied, NotApplicable> {
    let n = entries(ctx.bundle).len();
    if n < 2 {
        return Err(na("fewer than two entries"));
    }
    let identity: Vec<usize> = (0..n).collect();
    let mut perm = rng.permutation(n);
    let mut tries = 0;
    while perm == identity && tries < 8 {
        perm = rng.permutation(n);
        tries += 1;
    }
    if perm == identity {
        perm.swap(0, n - 1);
    }
    // perm[p] = original index of the entry that ends up at position p.
    // Apply as a sequence of moves so patch.json stays small and exact.
    let mut current: Vec<usize> = identity;
    let mut ed = Editor::new(ctx.bundle.clone());
    for (p, wanted) in perm.iter().enumerate() {
        let Some(at) = current.iter().position(|x| x == wanted) else {
            return Err(na("internal: permutation lost an entry"));
        };
        if at != p {
            ed.mv(&format!("/entry/{at}"), &format!("/entry/{p}"))?;
            let item = current.remove(at);
            current.insert(p, item);
        }
    }
    let (bundle, patch) = ed.finish();
    let moved = perm.iter().enumerate().filter(|(p, o)| p != *o).count();
    Ok(Applied {
        bundle,
        patch,
        effect: Effect::of(EffectKind::SameAsBase),
        pre_step: None,
        touched: Vec::new(),
        summary: format!("entries shuffled, {moved} of {n} in a new position"),
        args: args_map(&[("order", json!(perm))]),
    })
}

pub const REORDER_ENTRIES: Operator = Operator {
    info: OperatorInfo {
        number: 19,
        id: "bundle.reorder_entries",
        version: 1,
        summary: "Shuffles entry order",
        intent: Intent::ValidMeaningKept,
        caught_by: &[],
        strict_rule: None,
        view_effect: "Same as base",
        eligibility: "A bundle with at least two entries. The new order is never the original order.",
        paths: "/entry (move operations)",
        reversible: "Yes: apply the inverse permutation.",
        before: "entry 0 Patient, 1 Encounter, 2 Observation",
        after: "entry 0 Observation, 1 Patient, 2 Encounter",
        args: &[],
        condition: "the entries were in a different order",
    },
    targets: reorder_targets,
    apply: reorder_apply,
};

// 20 -----------------------------------------------------------------------

fn dup_url_targets(ctx: &OpCtx<'_>, select: &Select) -> Vec<TargetRef> {
    if entries(ctx.bundle).len() < 2 {
        return Vec::new();
    }
    candidates(ctx)
        .into_iter()
        .filter(|(idx, res, _)| {
            matches(res, select, &[])
                && ctx
                    .graph
                    .entry_full_urls
                    .get(*idx)
                    .is_some_and(Option::is_some)
        })
        .map(|(_, _, key)| TargetRef::resource(key))
        .collect()
}

fn dup_url_apply(
    ctx: &OpCtx<'_>,
    target: &TargetRef,
    _args: &Map<String, Value>,
    rng: &mut CaseRng,
) -> Result<Applied, NotApplicable> {
    let key = target_key(target)?;
    let (idx, _) = locate(ctx, key)?;
    let url = ctx
        .graph
        .entry_full_urls
        .get(idx)
        .cloned()
        .flatten()
        .ok_or_else(|| na("entry has no fullUrl"))?;
    let others: Vec<usize> = (0..entries(ctx.bundle).len())
        .filter(|j| {
            *j != idx
                && ctx
                    .graph
                    .entry_full_urls
                    .get(*j)
                    .cloned()
                    .flatten()
                    .as_deref()
                    != Some(url.as_str())
        })
        .collect();
    if others.is_empty() {
        return Err(na("no other entry"));
    }
    let j = others[rng.index(others.len())];
    let other_key = ctx
        .graph
        .keys
        .get(j)
        .cloned()
        .flatten()
        .unwrap_or_else(|| format!("entry {j}"));
    let mut ed = Editor::new(ctx.bundle.clone());
    ed.set(&format!("/entry/{j}/fullUrl"), json!(url))?;
    let (bundle, patch) = ed.finish();
    Ok(Applied {
        bundle,
        patch,
        effect: Effect::of(EffectKind::Invalid),
        pre_step: None,
        touched: vec![key.to_string(), other_key.clone()],
        summary: format!("{other_key} given the fullUrl of {key} ({url})"),
        args: Map::new(),
    })
}

pub const DUPLICATE_FULL_URL: Operator = Operator {
    info: OperatorInfo {
        number: 20,
        id: "bundle.duplicate_full_url",
        version: 1,
        summary: "Gives two entries the same fullUrl",
        intent: Intent::Invalid,
        caught_by: &[Layer::References, Layer::Strict],
        strict_rule: Some("bdl-7"),
        view_effect: "Rejected",
        eligibility: "An entry with a fullUrl, in a bundle with at least two entries.",
        paths: "/entry/{m}/fullUrl of a second entry",
        reversible: "Yes: restore the second entry's fullUrl.",
        before: r#"entry 2 {"fullUrl": ".../Encounter/enc-a1"}, entry 5 {"fullUrl": ".../Observation/obs-glucose-2"}"#,
        after: r#"entry 5 {"fullUrl": ".../Encounter/enc-a1"}"#,
        args: &[],
        condition: "two entries shared one fullUrl",
    },
    targets: dup_url_targets,
    apply: dup_url_apply,
};
