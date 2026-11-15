//! Time operators: 5 backdate_effective, 6 future_effective,
//! 7 date_only_precision, 8 timezone_offset_shift, 9 period_end_before_start.

use serde_json::{Map, Value, json};

use super::{
    Applied, ArgInfo, Effect, EffectKind, Intent, NotApplicable, OpCtx, Operator, OperatorInfo,
    Select, TargetRef, args_map, candidates, locate, matches, na, target_key,
};
use crate::bundle::{Editor, resource_ptr};
use crate::json::{arg_i64, arg_str};
use crate::rng::CaseRng;
use crate::time;
use crate::validate::Layer;

const TIMED: &[&str] = &["Observation", "DiagnosticReport"];

fn has_full_time(res: &Value, field: &str) -> bool {
    res.get(field)
        .and_then(Value::as_str)
        .and_then(time::parse)
        .is_some_and(|t| t.has_time)
}

fn effective_targets(ctx: &OpCtx<'_>, select: &Select, need_issued: bool) -> Vec<TargetRef> {
    candidates(ctx)
        .into_iter()
        .filter(|(_, res, _)| {
            matches(res, select, TIMED)
                && has_full_time(res, "effectiveDateTime")
                && (!need_issued || has_full_time(res, "issued"))
        })
        .map(|(_, _, key)| TargetRef::resource(key))
        .collect()
}

fn simple(
    bundle: Value,
    patch: Vec<crate::bundle::PatchOp>,
    kind: EffectKind,
    key: &str,
    summary: String,
    args: Map<String, Value>,
) -> Applied {
    Applied {
        bundle,
        patch,
        effect: Effect::of(kind),
        pre_step: None,
        touched: vec![key.to_string()],
        summary,
        args,
    }
}

// 5 ------------------------------------------------------------------------

fn backdate_targets(ctx: &OpCtx<'_>, select: &Select) -> Vec<TargetRef> {
    effective_targets(ctx, select, true)
}

fn backdate_apply(
    ctx: &OpCtx<'_>,
    target: &TargetRef,
    args: &Map<String, Value>,
    _rng: &mut CaseRng,
) -> Result<Applied, NotApplicable> {
    let key = target_key(target)?;
    let (idx, res) = locate(ctx, key)?;
    let days = arg_i64(args, "days", 30).max(1);
    let issued = res
        .get("issued")
        .and_then(Value::as_str)
        .ok_or_else(|| na("no issued"))?;
    let new_eff = time::add_days(issued, -days).ok_or_else(|| na("cannot read issued"))?;
    let mut ed = Editor::new(ctx.bundle.clone());
    ed.replace(
        &format!("{}/effectiveDateTime", resource_ptr(idx)),
        json!(new_eff),
    )?;
    let (bundle, patch) = ed.finish();
    Ok(simple(
        bundle,
        patch,
        EffectKind::NoCrashOnly,
        key,
        format!("{key}: effectiveDateTime moved to {new_eff}, {days} days before issued"),
        args_map(&[("days", json!(days))]),
    ))
}

pub const BACKDATE_EFFECTIVE: Operator = Operator {
    info: OperatorInfo {
        number: 5,
        id: "time.backdate_effective",
        version: 1,
        summary: "Moves effective[x] N days before issued",
        intent: Intent::ValidEdge,
        caught_by: &[],
        strict_rule: None,
        view_effect: "Order by effective time",
        eligibility: "An Observation or DiagnosticReport with a full effectiveDateTime and issued.",
        paths: "/entry/{n}/resource/effectiveDateTime",
        reversible: "Yes: restore effectiveDateTime.",
        before: r#"{"effectiveDateTime": "2026-01-10T08:30:00Z", "issued": "2026-01-10T09:12:00Z"}"#,
        after: r#"{"effectiveDateTime": "2025-12-11T09:12:00Z", "issued": "2026-01-10T09:12:00Z"}"#,
        args: &[ArgInfo {
            name: "days",
            default: "30",
            help: "Days before issued.",
        }],
        condition: "the effective time was {days} days before the issued time",
    },
    targets: backdate_targets,
    apply: backdate_apply,
};

// 6 ------------------------------------------------------------------------

fn future_targets(ctx: &OpCtx<'_>, select: &Select) -> Vec<TargetRef> {
    effective_targets(ctx, select, false)
}

fn future_apply(
    ctx: &OpCtx<'_>,
    target: &TargetRef,
    args: &Map<String, Value>,
    _rng: &mut CaseRng,
) -> Result<Applied, NotApplicable> {
    let key = target_key(target)?;
    let (idx, res) = locate(ctx, key)?;
    let days = arg_i64(args, "days_after", 30).max(1);
    let eff = res
        .get("effectiveDateTime")
        .and_then(Value::as_str)
        .ok_or_else(|| na("no effectiveDateTime"))?;
    let t = time::parse(eff).ok_or_else(|| na("cannot read effectiveDateTime"))?;
    let as_of =
        time::parse(ctx.as_of).ok_or_else(|| na(format!("cannot read --as-of {}", ctx.as_of)))?;
    // Keep the time of day, move the date to as_of + days.
    let secs_of_day = t.epoch.rem_euclid(86_400);
    let day0 = as_of.epoch.div_euclid(86_400) * 86_400;
    let new_epoch = day0 + days * 86_400 + secs_of_day;
    let new_eff = time::format_with_offset(new_epoch, &t.frac, t.offset_minutes);
    let mut ed = Editor::new(ctx.bundle.clone());
    ed.replace(
        &format!("{}/effectiveDateTime", resource_ptr(idx)),
        json!(new_eff),
    )?;
    let (bundle, patch) = ed.finish();
    Ok(simple(
        bundle,
        patch,
        EffectKind::NoCrashOnly,
        key,
        format!(
            "{key}: effectiveDateTime moved to {new_eff}, {days} days after the run date {}",
            ctx.as_of
        ),
        args_map(&[("days_after", json!(days)), ("as_of", json!(ctx.as_of))]),
    ))
}

pub const FUTURE_EFFECTIVE: Operator = Operator {
    info: OperatorInfo {
        number: 6,
        id: "time.future_effective",
        version: 1,
        summary: "Sets effective[x] after the run's --as-of date",
        intent: Intent::ValidEdge,
        caught_by: &[],
        strict_rule: None,
        view_effect: "Same as base; only no_crash is checked, since a future reading may become the latest",
        eligibility: "An Observation or DiagnosticReport with a full effectiveDateTime.",
        paths: "/entry/{n}/resource/effectiveDateTime",
        reversible: "Yes: restore effectiveDateTime.",
        before: r#"{"effectiveDateTime": "2026-01-10T08:30:00Z"}"#,
        after: r#"{"effectiveDateTime": "2026-03-01T08:30:00Z"} with --as-of 2026-01-30"#,
        args: &[ArgInfo {
            name: "days_after",
            default: "30",
            help: "Days after --as-of.",
        }],
        condition: "the effective time was after the run date",
    },
    targets: future_targets,
    apply: future_apply,
};

// 7 ------------------------------------------------------------------------

fn date_only_targets(ctx: &OpCtx<'_>, select: &Select) -> Vec<TargetRef> {
    effective_targets(ctx, select, false)
}

fn date_only_apply(
    ctx: &OpCtx<'_>,
    target: &TargetRef,
    _args: &Map<String, Value>,
    _rng: &mut CaseRng,
) -> Result<Applied, NotApplicable> {
    let key = target_key(target)?;
    let (idx, res) = locate(ctx, key)?;
    let eff = res
        .get("effectiveDateTime")
        .and_then(Value::as_str)
        .ok_or_else(|| na("no effectiveDateTime"))?;
    if eff.len() <= 10 {
        return Err(na("already a date"));
    }
    let date = eff[..10].to_string();
    let mut ed = Editor::new(ctx.bundle.clone());
    ed.replace(
        &format!("{}/effectiveDateTime", resource_ptr(idx)),
        json!(date),
    )?;
    let (bundle, patch) = ed.finish();
    Ok(simple(
        bundle,
        patch,
        EffectKind::SameAsBase,
        key,
        format!("{key}: effectiveDateTime {eff} written as {date}"),
        Map::new(),
    ))
}

pub const DATE_ONLY_PRECISION: Operator = Operator {
    info: OperatorInfo {
        number: 7,
        id: "time.date_only_precision",
        version: 1,
        summary: "Replaces an instant with a date (2026-01-04)",
        intent: Intent::ValidEdge,
        caught_by: &[],
        strict_rule: None,
        view_effect: "Same as base",
        eligibility: "An Observation or DiagnosticReport whose effectiveDateTime has a time part. issued is an instant and must keep its time, so it is never changed.",
        paths: "/entry/{n}/resource/effectiveDateTime",
        reversible: "No: the time of day is dropped.",
        before: r#"{"effectiveDateTime": "2026-01-04T08:30:00Z"}"#,
        after: r#"{"effectiveDateTime": "2026-01-04"}"#,
        args: &[],
        condition: "a timestamp was written as a date only",
    },
    targets: date_only_targets,
    apply: date_only_apply,
};

// 8 ------------------------------------------------------------------------

fn offset_targets(ctx: &OpCtx<'_>, select: &Select) -> Vec<TargetRef> {
    let mut out = Vec::new();
    for (_, res, key) in candidates(ctx) {
        if !matches(res, select, TIMED) {
            continue;
        }
        for field in ["effectiveDateTime", "issued"] {
            if has_full_time(res, field) {
                out.push(TargetRef::site(key.clone(), format!("/{field}")));
            }
        }
    }
    out
}

fn offset_apply(
    ctx: &OpCtx<'_>,
    target: &TargetRef,
    args: &Map<String, Value>,
    _rng: &mut CaseRng,
) -> Result<Applied, NotApplicable> {
    let key = target_key(target)?;
    let field = target.path.as_deref().unwrap_or("/effectiveDateTime");
    let (idx, res) = locate(ctx, key)?;
    let text = res
        .pointer(field)
        .and_then(Value::as_str)
        .ok_or_else(|| na(format!("no {field}")))?;
    let t = time::parse(text)
        .filter(|t| t.has_time)
        .ok_or_else(|| na("no time part"))?;
    let offset_text = arg_str(args, "offset", "+05:00");
    let mut offset =
        time::parse_offset(&offset_text).ok_or_else(|| na(format!("bad offset {offset_text}")))?;
    if offset == t.offset_minutes {
        // Already written with that offset: use the opposite sign instead.
        offset = -offset;
    }
    if offset == t.offset_minutes {
        return Err(na("offset would not change"));
    }
    let new_text = time::format_with_offset(t.epoch, &t.frac, offset);
    let mut ed = Editor::new(ctx.bundle.clone());
    ed.replace(&format!("{}{field}", resource_ptr(idx)), json!(new_text))?;
    let (bundle, patch) = ed.finish();
    Ok(simple(
        bundle,
        patch,
        EffectKind::SameAsBase,
        key,
        format!("{key}{field}: {text} written as {new_text}, the same instant"),
        args_map(&[("offset", json!(offset_text))]),
    ))
}

pub const TIMEZONE_OFFSET_SHIFT: Operator = Operator {
    info: OperatorInfo {
        number: 8,
        id: "time.timezone_offset_shift",
        version: 1,
        summary: "Same instant written with another offset (+05:00)",
        intent: Intent::ValidEdge,
        caught_by: &[],
        strict_rule: None,
        view_effect: "Same as base",
        eligibility: "An Observation or DiagnosticReport effectiveDateTime or issued with a time part.",
        paths: "/entry/{n}/resource/effectiveDateTime or /issued",
        reversible: "Yes: write the instant in UTC again.",
        before: r#"{"issued": "2026-01-04T09:12:00Z"}"#,
        after: r#"{"issued": "2026-01-04T14:12:00+05:00"}"#,
        args: &[ArgInfo {
            name: "offset",
            default: "+05:00",
            help: "The new UTC offset.",
        }],
        condition: "a timestamp was written with a {offset} offset",
    },
    targets: offset_targets,
    apply: offset_apply,
};

// 9 ------------------------------------------------------------------------

fn period_targets(ctx: &OpCtx<'_>, select: &Select) -> Vec<TargetRef> {
    candidates(ctx)
        .into_iter()
        .filter(|(_, res, _)| {
            let start = res.pointer("/period/start").and_then(Value::as_str);
            let end = res.pointer("/period/end").and_then(Value::as_str);
            matches(res, select, &["Encounter"])
                && match (start.and_then(time::parse), end.and_then(time::parse)) {
                    (Some(s), Some(e)) => s.epoch < e.epoch,
                    _ => false,
                }
        })
        .map(|(_, _, key)| TargetRef::resource(key))
        .collect()
}

fn period_apply(
    ctx: &OpCtx<'_>,
    target: &TargetRef,
    _args: &Map<String, Value>,
    _rng: &mut CaseRng,
) -> Result<Applied, NotApplicable> {
    let key = target_key(target)?;
    let (idx, res) = locate(ctx, key)?;
    let start = res
        .pointer("/period/start")
        .cloned()
        .ok_or_else(|| na("no period.start"))?;
    let end = res
        .pointer("/period/end")
        .cloned()
        .ok_or_else(|| na("no period.end"))?;
    if start == end {
        return Err(na("start equals end"));
    }
    let ptr = resource_ptr(idx);
    let mut ed = Editor::new(ctx.bundle.clone());
    ed.replace(&format!("{ptr}/period/start"), end.clone())?;
    ed.replace(&format!("{ptr}/period/end"), start.clone())?;
    let (bundle, patch) = ed.finish();
    Ok(simple(
        bundle,
        patch,
        EffectKind::Invalid,
        key,
        format!(
            "{key}: period.start and period.end swapped, so it ends at {start} before it starts at {end}"
        ),
        Map::new(),
    ))
}

pub const PERIOD_END_BEFORE_START: Operator = Operator {
    info: OperatorInfo {
        number: 9,
        id: "time.period_end_before_start",
        version: 1,
        summary: "Swaps an Encounter's period.start and end",
        intent: Intent::Invalid,
        caught_by: &[Layer::Strict],
        strict_rule: Some("per-1"),
        view_effect: "Rejected or flagged",
        eligibility: "An Encounter whose period has a start before its end.",
        paths: "/entry/{n}/resource/period/start, /period/end",
        reversible: "Yes: swap them back.",
        before: r#"{"period": {"start": "2026-01-04T08:00:00Z", "end": "2026-01-04T09:30:00Z"}}"#,
        after: r#"{"period": {"start": "2026-01-04T09:30:00Z", "end": "2026-01-04T08:00:00Z"}}"#,
        args: &[],
        condition: "an encounter ended before it started",
    },
    targets: period_targets,
    apply: period_apply,
};
