//! Operator 22: text.unicode_narrative.

use serde_json::{Map, Value, json};

use super::{
    Applied, Effect, EffectKind, Intent, NotApplicable, OpCtx, Operator, OperatorInfo, Select,
    TargetRef, candidates, locate, matches, target_key,
};
use crate::bundle::{Editor, resource_ptr};
use crate::rng::CaseRng;

/// Valid XHTML with accented Latin, combining characters, right-to-left Arabic
/// and Hebrew, a CJK line and an emoji outside the Basic Multilingual Plane.
pub const NARRATIVE: &str = "<div xmlns=\"http://www.w3.org/1999/xhtml\"><p>Synthetic r\u{e9}sum\u{e9}: Zo\u{eb} \u{c5}ngstr\u{f6}m, e\u{301} n\u{303}</p><p dir=\"rtl\">\u{645}\u{631}\u{62d}\u{628}\u{627} \u{5e9}\u{5dc}\u{5d5}\u{5dd}</p><p>\u{5408}\u{6210}\u{30c7}\u{30fc}\u{30bf} \u{1f9ea}</p></div>";

fn targets(ctx: &OpCtx<'_>, select: &Select) -> Vec<TargetRef> {
    candidates(ctx)
        .into_iter()
        .filter(|(_, res, _)| matches(res, select, &[]))
        .map(|(_, _, key)| TargetRef::resource(key))
        .collect()
}

fn apply(
    ctx: &OpCtx<'_>,
    target: &TargetRef,
    _args: &Map<String, Value>,
    _rng: &mut CaseRng,
) -> Result<Applied, NotApplicable> {
    let key = target_key(target)?;
    let (idx, _) = locate(ctx, key)?;
    let mut ed = Editor::new(ctx.bundle.clone());
    ed.set(
        &format!("{}/text", resource_ptr(idx)),
        json!({"status": "generated", "div": NARRATIVE}),
    )?;
    let (bundle, patch) = ed.finish();
    Ok(Applied {
        bundle,
        patch,
        effect: Effect::of(EffectKind::SameAsBase),
        pre_step: None,
        touched: vec![key.to_string()],
        summary: format!(
            "{key}: text.div set to non-ASCII, right-to-left and combining characters"
        ),
        args: Map::new(),
    })
}

pub const UNICODE_NARRATIVE: Operator = Operator {
    info: OperatorInfo {
        number: 22,
        id: "text.unicode_narrative",
        version: 1,
        summary: "Puts non-ASCII, right-to-left and combining characters in text.div (valid XHTML)",
        intent: Intent::ValidEdge,
        caught_by: &[],
        strict_rule: None,
        view_effect: "Same as base",
        eligibility: "Any resource with an id.",
        paths: "/entry/{n}/resource/text",
        reversible: "Yes: restore or remove text.",
        before: r#"{"text": {"status": "generated", "div": "<div xmlns=...>Glucose</div>"}}"#,
        after: r#"{"text": {"status": "generated", "div": "<div xmlns=...><p>Synthetic résumé ...</p><p dir=\"rtl\">مرحبا</p>...</div>"}}"#,
        args: &[],
        condition: "the narrative held non-ASCII, right-to-left and combining characters",
    },
    targets,
    apply,
};
