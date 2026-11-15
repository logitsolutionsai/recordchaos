//! The recordchaos report: one offline HTML file (fonts embedded, no network
//! requests) and JUnit XML for CI.

use base64::Engine;
use quick_junit::{NonSuccessKind, Report, TestCase, TestCaseStatus, TestSuite};
use recordchaos_core::diff::DiffKind;
use recordchaos_core::json::short_hash;
use recordchaos_core::results::{CaseResult, Finding, Outcome, RunResults};
use recordchaos_core::validate::{Layer, LayerStatus, Validation};
use serde::Serialize;

/// The page the footer link points at.
pub const GUIDE_URL: &str = "https://logitsolutions.ai/guides/ai-in-medical-practices/";

const TEMPLATE: &str = include_str!("../templates/report.html");

/// Geist and Geist Mono, Latin subset, SIL Open Font License 1.1 (see
/// crates/recordchaos-report/assets/fonts/OFL.txt).
const GEIST_400: &[u8] = include_bytes!("../assets/fonts/geist-latin-400-normal.woff2");
const GEIST_500: &[u8] = include_bytes!("../assets/fonts/geist-latin-500-normal.woff2");
const GEIST_600: &[u8] = include_bytes!("../assets/fonts/geist-latin-600-normal.woff2");
const GEIST_MONO_400: &[u8] = include_bytes!("../assets/fonts/geist-mono-latin-400-normal.woff2");
const GEIST_MONO_500: &[u8] = include_bytes!("../assets/fonts/geist-mono-latin-500-normal.woff2");

/// A report error.
#[derive(Debug)]
pub struct ReportError(pub String);

impl std::fmt::Display for ReportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "report error: {}", self.0)
    }
}

impl std::error::Error for ReportError {}

/// Facts about the run that are not in results.json (they are not
/// deterministic), read from run.toml.
#[derive(Clone, Debug, Default)]
pub struct ReportMeta {
    pub generated_at: Option<String>,
    pub total_seconds: Option<f64>,
    pub embed_fonts: bool,
}

#[derive(Serialize)]
struct Fonts {
    sans400: String,
    sans500: String,
    sans600: String,
    mono400: String,
    mono500: String,
}

#[derive(Serialize)]
struct Square {
    id: String,
    href: String,
    class: &'static str,
    label: &'static str,
}

#[derive(Serialize)]
struct Counts {
    handled: usize,
    rejected: usize,
    rule: usize,
    crashed: usize,
    notchecked: usize,
    failed: usize,
    invalid: usize,
}

#[derive(Serialize)]
struct MetaRow {
    label: &'static str,
    value: String,
    mono: bool,
    caution: Option<String>,
}

#[derive(Serialize)]
struct Fact {
    label: String,
    value: String,
    mono: bool,
}

#[derive(Serialize)]
struct LayerRow {
    name: &'static str,
    value: String,
    caution: bool,
}

#[derive(Serialize)]
struct DiffRow {
    class: &'static str,
    sign: &'static str,
    line: String,
    text: String,
}

#[derive(Serialize)]
struct FindingView {
    id: String,
    anchor: String,
    label: &'static str,
    label_class: &'static str,
    oracle: String,
    count_line: String,
    sentence: String,
    facts: Vec<Fact>,
    layers: Vec<LayerRow>,
    note: Option<String>,
    diff_title: String,
    diff_side: String,
    diff: Vec<DiffRow>,
    replay: String,
}

#[derive(Serialize)]
struct CaseRow {
    id: String,
    finding_anchor: Option<String>,
    tags: String,
    mutation: String,
    intended: &'static str,
    schema: String,
    references: String,
    strict: String,
    strict_caution: bool,
    importer: String,
    bad: bool,
}

#[derive(Serialize)]
struct Note {
    title: String,
    text: String,
}

#[derive(Serialize)]
struct Model {
    version: String,
    generated_at: Option<String>,
    scenario_path: String,
    verdict: String,
    lede: String,
    squares: Vec<Square>,
    counts: Counts,
    meta: Vec<MetaRow>,
    findings: Vec<FindingView>,
    cases: Vec<CaseRow>,
    total: usize,
    not_checked: Vec<Note>,
    footer_inputs: String,
    guide_url: &'static str,
    fonts: Option<Fonts>,
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Short text for one layer. "Not run, add --strict" is only true when the
/// run had no strict layer; when it had one, a skipped case says why.
fn layer_text(v: &Validation, l: Layer, strict_ran: bool) -> String {
    let r = v.get(l);
    if l == Layer::Strict && strict_ran && r.status == LayerStatus::NotChecked {
        return match &r.reason {
            Some(why) => format!("Not checked: {why}"),
            None => String::from("Not checked"),
        };
    }
    r.short(l)
}

fn layer_rows(v: &Validation, strict_ran: bool) -> Vec<LayerRow> {
    [Layer::Schema, Layer::References, Layer::Strict]
        .into_iter()
        .map(|l| LayerRow {
            name: l.title(),
            value: layer_text(v, l, strict_ran),
            caution: v.get(l).status == LayerStatus::NotChecked,
        })
        .collect()
}

/// Case ids listed in a finding's head line before "and N more".
const MAX_LISTED_CASES: usize = 5;

/// "c-0001, c-0003, c-0006", or the first five and "and 20 more".
fn case_list(cases: &[String]) -> String {
    if cases.len() <= MAX_LISTED_CASES + 1 {
        return cases.join(", ");
    }
    format!(
        "{} and {} more",
        cases[..MAX_LISTED_CASES].join(", "),
        cases.len() - MAX_LISTED_CASES
    )
}

/// Split "Observation/obs-1 · 2 paths changed" into the label and the count,
/// as the design's diff head shows them (label left, count right).
fn split_diff_title(title: &str) -> (String, String) {
    match title.rsplit_once(" · ") {
        Some((label, count)) => (label.to_string(), count.to_string()),
        None => (title.to_string(), String::new()),
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn finding_view(f: &Finding, strict_ran: bool) -> FindingView {
    let (label, label_class) = if f.flaky {
        ("Flaky", "caution flaky")
    } else if f.kind == "crashed" {
        ("Crashed", "fail crashed")
    } else {
        ("Rule broken", "fail rule")
    };
    let shrink = if f.flaky {
        f.reproduced
            .clone()
            .unwrap_or_else(|| String::from("did not reproduce"))
    } else if f.mutations_after < f.mutations_before {
        format!(
            "shrunk from {} to {}",
            plural(f.mutations_before, "mutation", "mutations"),
            f.mutations_after
        )
    } else {
        String::from("already minimal")
    };
    let count_line = format!(
        "{} · {} · {shrink}",
        plural(f.cases.len(), "case", "cases"),
        case_list(&f.cases)
    );
    let mut facts = Vec::new();
    if let Some(e) = &f.expected {
        facts.push(Fact {
            label: String::from("Expected"),
            value: e.clone(),
            mono: false,
        });
    }
    if let Some(g) = &f.got {
        facts.push(Fact {
            label: String::from("Got"),
            value: g.clone(),
            mono: false,
        });
    }
    // A crash's first line is usually inside "Got" already; show it once.
    let first_line = f
        .first_line
        .as_ref()
        .filter(|l| !f.got.as_deref().is_some_and(|g| g.contains(l.as_str())));
    if let Some(l) = first_line {
        facts.push(Fact {
            label: String::from("First line"),
            value: l.clone(),
            mono: true,
        });
    }
    for m in &f.mutations {
        let args: Vec<String> = m
            .args
            .iter()
            .filter(|(k, _)| !matches!(k.as_str(), "order" | "urn"))
            .map(|(k, v)| match v {
                serde_json::Value::String(s) => format!("{k}={s}"),
                other => format!("{k}={other}"),
            })
            .collect();
        let value = if args.is_empty() {
            m.operator.clone()
        } else {
            format!("{} {}", m.operator, args.join(" "))
        };
        facts.push(Fact {
            label: String::from("Mutation"),
            value,
            mono: true,
        });
        facts.push(Fact {
            label: String::from("Change"),
            value: m.summary.clone(),
            mono: false,
        });
    }
    if f.entries_removed > 0 {
        facts.push(Fact {
            label: String::from("Shrunk"),
            value: format!(
                "{} not needed to fail were removed",
                plural(f.entries_removed, "entry", "entries")
            ),
            mono: false,
        });
    }
    facts.push(Fact {
        label: String::from("Check"),
        value: f.check.clone(),
        mono: false,
    });
    let diff = f
        .diff
        .iter()
        .map(|d| {
            let (class, sign) = match d.kind {
                DiffKind::Added => ("added", "+"),
                DiffKind::Removed => ("removed", "\u{2212}"),
                DiffKind::Gap => ("gap", ""),
                DiffKind::Context => ("context", ""),
            };
            DiffRow {
                class,
                sign,
                line: d.line.map(|n| n.to_string()).unwrap_or_default(),
                text: d.text.clone(),
            }
        })
        .collect();
    let (diff_title, diff_side) = split_diff_title(&f.diff_title);
    FindingView {
        id: f.id.clone(),
        anchor: f.id.to_lowercase(),
        label,
        label_class,
        oracle: f.oracle.clone(),
        count_line,
        sentence: f.sentence.clone(),
        facts,
        layers: layer_rows(&f.validation, strict_ran),
        note: f.note.clone(),
        diff_title,
        diff_side,
        diff,
        replay: f.replay.clone(),
    }
}

fn square(c: &CaseResult) -> Square {
    let (class, label) = match c.outcome {
        Outcome::Handled => ("", "passed"),
        Outcome::RejectedCleanly => ("rejected", "rejected cleanly"),
        Outcome::RuleBroken => ("rule", "rule broken"),
        Outcome::Crashed => ("crashed", "crashed"),
        Outcome::NotApplicable => ("notchecked", "not checked"),
    };
    let href = c
        .finding
        .as_ref()
        .map(|f| f.to_lowercase())
        .unwrap_or_else(|| c.id.clone());
    Square {
        id: c.id.clone(),
        href,
        class,
        label,
    }
}

fn importer_text(c: &CaseResult) -> (String, bool) {
    let step = c.step.as_ref().map(|s| s.code_text()).unwrap_or_default();
    match c.outcome {
        Outcome::Handled => (String::from("Handled"), false),
        Outcome::RejectedCleanly => (format!("Rejected, {step}"), false),
        Outcome::RuleBroken => (
            format!("Rule broken · {}", c.finding.clone().unwrap_or_default()),
            true,
        ),
        Outcome::Crashed => (
            format!(
                "Crashed, {step} · {}",
                c.finding.clone().unwrap_or_default()
            ),
            true,
        ),
        Outcome::NotApplicable => (String::from("Not run: nothing to change"), false),
    }
}

fn case_row(c: &CaseResult, strict_ran: bool) -> CaseRow {
    let mutation = if c.mutations.is_empty() {
        String::from("none")
    } else {
        c.mutations
            .iter()
            .map(|m| m.operator.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut tags = vec!["all"];
    if c.outcome.is_failure() {
        tags.push("failed");
    }
    if c.intent.is_invalid() {
        tags.push("invalid");
    }
    let (importer, bad) = importer_text(c);
    CaseRow {
        id: c.id.clone(),
        finding_anchor: c.finding.as_ref().map(|f| f.to_lowercase()),
        tags: tags.join(" "),
        mutation,
        intended: c.intent.label(),
        schema: c.validation.schema.short(Layer::Schema),
        references: c.validation.references.short(Layer::References),
        // The summary already says how to turn the HL7 validator on; one
        // short phrase per row keeps the column narrow.
        strict: if c.validation.strict.status == LayerStatus::NotChecked {
            String::from(if strict_ran { "Not checked" } else { "Not run" })
        } else {
            c.validation.strict.short(Layer::Strict)
        },
        strict_caution: c.validation.strict.status == LayerStatus::NotChecked,
        importer,
        bad,
    }
}

fn lede(r: &RunResults) -> String {
    let s = &r.summary;
    if s.failed == 0 {
        if s.ran == 0 {
            return String::from(
                "No operator found anything to change in the base bundle, so nothing was sent to the importer.",
            );
        }
        return match s.expected_invalid {
            0 => String::from("Every case was handled or rejected cleanly."),
            1 => String::from(
                "Every case was handled or rejected cleanly. 1 was meant to be invalid.",
            ),
            n => {
                format!("Every case was handled or rejected cleanly. {n} were meant to be invalid.")
            }
        };
    }
    let causes = r.findings.len();
    format!(
        "They come from {}. Each cause below is shrunk to the smallest bundle that still fails, with the one command that replays it.",
        plural(causes, "cause", "causes")
    )
}

fn build_model(r: &RunResults, meta: &ReportMeta) -> Model {
    let s = &r.summary;
    let counts = Counts {
        handled: s.handled,
        rejected: s.rejected_cleanly,
        rule: s.rule_broken,
        crashed: s.crashed,
        notchecked: s.not_applicable,
        failed: s.failed,
        invalid: s.expected_invalid,
    };
    let checked_with = if r.layers.strict {
        (
            String::from("R4 JSON schema, reference checker, HL7 validator."),
            None,
        )
    } else {
        (
            String::from("R4 JSON schema, reference checker."),
            Some(String::from("HL7 validator not run.")),
        )
    };
    let mut meta_rows = vec![
        MetaRow {
            label: "Target",
            value: r.target.describe.clone(),
            mono: true,
            caution: None,
        },
        MetaRow {
            label: "Base bundle",
            value: format!("{} · {}", r.base.path, short_hash(&r.base.sha256)),
            mono: true,
            caution: None,
        },
        MetaRow {
            label: "Seed",
            value: r.seed.to_string(),
            mono: true,
            caution: None,
        },
        MetaRow {
            label: "Operators",
            value: format!(
                "{} of {} in the registry",
                r.operators.len(),
                r.registry_size
            ),
            mono: false,
            caution: None,
        },
        MetaRow {
            label: "Checked with",
            value: checked_with.0,
            mono: false,
            caution: checked_with.1,
        },
    ];
    if let Some(t) = meta.total_seconds {
        meta_rows.push(MetaRow {
            label: "Time",
            value: if s.shrink_replays == 0 {
                format!("{t:.1} s")
            } else {
                format!(
                    "{t:.1} s, including {} to shrink",
                    plural(s.shrink_replays, "replay", "replays")
                )
            },
            mono: false,
            caution: None,
        });
    }
    let fonts = meta.embed_fonts.then(|| Fonts {
        sans400: b64(GEIST_400),
        sans500: b64(GEIST_500),
        sans600: b64(GEIST_600),
        mono400: b64(GEIST_MONO_400),
        mono500: b64(GEIST_MONO_500),
    });
    Model {
        version: r.tool_version.clone(),
        generated_at: meta.generated_at.clone(),
        scenario_path: r.scenario.path.clone(),
        verdict: r.verdict(),
        lede: lede(r),
        squares: r.cases.iter().map(square).collect(),
        counts,
        meta: meta_rows,
        findings: r
            .findings
            .iter()
            .map(|f| finding_view(f, r.layers.strict))
            .collect(),
        cases: r
            .cases
            .iter()
            .map(|c| case_row(c, r.layers.strict))
            .collect(),
        total: r.cases.len(),
        not_checked: r
            .not_checked
            .iter()
            .map(|n| Note {
                title: n.title.clone(),
                text: n.text.clone(),
            })
            .collect(),
        footer_inputs: format!(
            "{}/report.html · inputs {}, scenario {}",
            r.out_dir,
            short_hash(&r.base.sha256),
            short_hash(&r.scenario.sha256)
        ),
        guide_url: GUIDE_URL,
        fonts,
    }
}

/// Render report.html.
pub fn render_html(results: &RunResults, meta: &ReportMeta) -> Result<String, ReportError> {
    let mut env = minijinja::Environment::new();
    env.add_template("report.html", TEMPLATE)
        .map_err(|e| ReportError(e.to_string()))?;
    let tmpl = env
        .get_template("report.html")
        .map_err(|e| ReportError(e.to_string()))?;
    let model = build_model(results, meta);
    tmpl.render(&model).map_err(|e| ReportError(e.to_string()))
}

/// The replay command for one failing case: its own case folder when the run
/// wrote one, otherwise the finding's representative case.
fn case_replay(f: &Finding, c: &CaseResult) -> String {
    if c.dir.is_none() || c.id == f.representative {
        return f.replay.clone();
    }
    f.replay.replace(
        &format!("cases/{}", f.representative),
        &format!("cases/{}", c.id),
    )
}

/// Render junit.xml: one testcase per case; failures carry the finding sentence.
pub fn render_junit(results: &RunResults) -> Result<String, ReportError> {
    let mut report = Report::new("recordchaos");
    let mut suite = TestSuite::new(results.scenario.path.clone());
    for c in &results.cases {
        let name = if c.mutations.is_empty() {
            c.id.clone()
        } else {
            format!(
                "{} {}",
                c.id,
                c.mutations
                    .iter()
                    .map(|m| m.operator.as_str())
                    .collect::<Vec<_>>()
                    .join(" + ")
            )
        };
        let status = match c.outcome {
            Outcome::Handled | Outcome::RejectedCleanly => TestCaseStatus::success(),
            Outcome::NotApplicable => {
                let mut s = TestCaseStatus::skipped();
                s.set_message("no operator found anything to change");
                s
            }
            Outcome::RuleBroken | Outcome::Crashed => {
                let mut s = TestCaseStatus::non_success(NonSuccessKind::Failure);
                let finding = c.finding.as_deref().and_then(|id| results.finding(id));
                let sentence = finding
                    .map(|f| f.sentence.clone())
                    .unwrap_or_else(|| String::from("A check failed."));
                s.set_message(sentence);
                if let Some(f) = finding {
                    s.set_type(f.oracle.clone());
                    let mut text = format!("Finding {}", f.id);
                    if f.representative != c.id {
                        text.push_str(&format!(
                            ", shown for case {}; this case failed the same way",
                            f.representative
                        ));
                    }
                    text.push('\n');
                    if let Some(e) = &f.expected {
                        text.push_str(&format!("Expected: {e}\n"));
                    }
                    if let Some(g) = &f.got {
                        text.push_str(&format!("Got: {g}\n"));
                    }
                    text.push_str(&format!("Replay: {}\n", case_replay(f, c)));
                    s.set_description(text);
                }
                s
            }
        };
        let mut tc = TestCase::new(name, status);
        tc.set_classname(format!("recordchaos.{}", results.scenario.path));
        suite.add_test_case(tc);
    }
    report.add_test_suite(suite);
    report.to_string().map_err(|e| ReportError(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_parses() {
        let mut env = minijinja::Environment::new();
        assert!(env.add_template("report.html", TEMPLATE).is_ok());
    }

    /// A small run: three cases, one finding with many cases, no strict layer.
    fn sample(failed: usize, expected_invalid: usize, replays: usize) -> RunResults {
        let validation = serde_json::json!({
            "schema": {"status": "valid"},
            "references": {"status": "valid"},
            "strict": {"status": "not_checked"}
        });
        let case_ids: Vec<String> = (1..=12).map(|i| format!("c-{i:04}")).collect();
        let v = serde_json::json!({
            "format_version": 1,
            "tool": "recordchaos",
            "tool_version": "0.1.0",
            "rng": "ChaCha8Rng",
            "seed": 7,
            "fhir_version": "4.0.1",
            "scenario": {"path": "scenarios/demo.toml", "sha256": "ab".repeat(32)},
            "base": {"path": "fixtures/bundles/patient-a.json", "sha256": "cd".repeat(32)},
            "target": {"path": "t.toml", "sha256": "ef".repeat(32), "kind": "command", "describe": "./imp ingest"},
            "as_of": "2026-01-14",
            "layers": {"schema": true, "references": true, "strict": false},
            "operators": ["bundle.reorder_entries"],
            "registry_size": 22,
            "out_dir": "results",
            "summary": {
                "cases": 3, "ran": 3, "handled": 3 - failed, "rejected_cleanly": 0,
                "rule_broken": failed, "crashed": 0, "not_applicable": 0, "failed": failed,
                "expected_invalid": expected_invalid, "unexpected_validity": 0,
                "unexpected_invalidity": 0, "valid_edge_cases": 3,
                "valid_edge_schema_valid": 3, "findings": usize::from(failed > 0),
                "shrink_replays": replays
            },
            "cases": [
                {"id": "c-0001", "seed": 1, "mutations": [], "intent": "valid_edge",
                 "validation": validation, "outcome": if failed > 0 { "rule_broken" } else { "handled" },
                 "finding": if failed > 0 { Some("F1") } else { None }},
                {"id": "c-0002", "seed": 2, "mutations": [], "intent": "valid_edge",
                 "validation": validation, "outcome": "handled"},
                {"id": "c-0003", "seed": 3, "mutations": [], "intent": "valid_edge",
                 "validation": validation, "outcome": "handled"}
            ],
            "findings": if failed > 0 { serde_json::json!([{
                "id": "F1", "oracle": "no_crash", "kind": "crashed", "fingerprint": "x",
                "sentence": "The importer exited with code 101 when the entries were in a different order.",
                "cases": case_ids, "representative": "c-0001",
                "mutations_before": 2, "mutations_after": 1, "entries_removed": 0,
                "shrink_replays": replays,
                "got": "exit 101: thread 'main' panicked at main.rs:1:1:",
                "first_line": "thread 'main' panicked at main.rs:1:1:",
                "check": "Built in.", "mutations": [], "validation": validation,
                "diff_title": "Observation/obs-glucose-3 · 2 paths changed", "diff": [],
                "replay": "recordchaos replay results/cases/c-0001"
            }]) } else { serde_json::json!([]) },
            "not_checked": [{"title": "Terminology", "text": "Codes are synthetic."}]
        });
        serde_json::from_value(v).unwrap()
    }

    fn render(r: &RunResults, seconds: Option<f64>) -> String {
        let meta = ReportMeta {
            generated_at: Some(String::from("2026-11-15T00:00:00Z")),
            total_seconds: seconds,
            embed_fonts: false,
        };
        render_html(r, &meta).unwrap()
    }

    #[test]
    fn hidden_attribute_wins_over_flex_so_filters_hide_without_scripts() {
        // `.filters{display:flex}` used to override the `hidden` attribute,
        // so dead filter buttons showed with scripts off.
        let html = render(&sample(1, 0, 3), None);
        assert!(html.contains("[hidden]{display:none!important}"));
        assert!(html.contains("<div class=\"filters\" hidden>"));
    }

    #[test]
    fn data_theme_forces_a_theme_and_the_os_setting_is_the_default() {
        let html = render(&sample(1, 0, 3), None);
        // The OS setting applies unless data-theme="light" forces light.
        let media = "@media (prefers-color-scheme:dark){:root:not([data-theme=\"light\"]){";
        let forced = ":root[data-theme=\"dark\"]{";
        let block = |start: &str| {
            let at = html.find(start).map(|i| i + start.len()).unwrap();
            html[at..at + html[at..].find('}').unwrap()].to_string()
        };
        // Both dark blocks hold the same tokens, which are the reference
        // palette and must not drift.
        assert_eq!(block(media), block(forced));
        assert!(block(forced).starts_with("--ink:#e3e6eb;"));
        assert!(block(forced).ends_with("color-scheme:dark"));
        assert!(html.contains(":root{--ink:#0b1220;"));
    }

    #[test]
    fn long_replay_commands_wrap_and_copy_exactly() {
        let mut r = sample(1, 0, 3);
        let long = "recordchaos replay 'C:/Users/TECH ZONE/AppData/Local/Temp/a very long folder name/results/cases/c-0001'";
        r.findings[0].replay = long.to_string();
        let html = render(&r, None);
        assert!(html.contains(".replay .cmd{min-width:0;padding-block:6px;white-space:pre-wrap;overflow-wrap:anywhere}"));
        assert!(!html.contains(".replay .cmd{min-width:0;overflow-x:auto;white-space:nowrap}"));
        // minijinja escapes quotes and slashes; the browser decodes them back
        // to the exact command for the Copy button.
        let attr = long
            .replace('&', "&amp;")
            .replace('\'', "&#x27;")
            .replace('/', "&#x2f;");
        assert!(html.contains(&format!("data-copy=\"{attr}\"")), "{html}");
    }

    #[test]
    fn lede_counts_read_as_english() {
        let one = render(&sample(0, 1, 0), None);
        assert!(one.contains("1 was meant to be invalid."), "{one}");
        assert!(!one.contains("1 were"));
        let none = render(&sample(0, 0, 0), None);
        assert!(none.contains("Every case was handled or rejected cleanly.</p>"));
        assert!(!none.contains("0 were meant"));
        let many = render(&sample(0, 26, 0), None);
        assert!(many.contains("26 were meant to be invalid."));
    }

    #[test]
    fn time_row_omits_zero_replays_and_counts_one_replay() {
        let zero = render(&sample(0, 0, 0), Some(2.14));
        assert!(zero.contains("<dd class=\"\">2.1 s</dd>"), "{zero}");
        assert!(!zero.contains("0 replays"));
        let one = render(&sample(1, 0, 1), Some(2.0));
        assert!(one.contains("2.0 s, including 1 replay to shrink"));
    }

    #[test]
    fn long_case_lists_are_cut_after_five() {
        let html = render(&sample(1, 0, 3), None);
        assert!(html.contains(
            "12 cases · c-0001, c-0002, c-0003, c-0004, c-0005 and 7 more · shrunk from 2 mutations to 1"
        ));
        assert!(!html.contains("c-0012"));
        assert_eq!(case_list(&[String::from("a"), String::from("b")]), "a, b");
    }

    #[test]
    fn crash_first_line_is_not_repeated_when_got_holds_it() {
        let html = render(&sample(1, 0, 3), None);
        assert_eq!(html.matches("panicked at main.rs").count(), 1, "{html}");
        assert!(!html.contains(">First line<"));
    }

    #[test]
    fn diff_head_puts_the_count_on_the_right() {
        assert_eq!(
            split_diff_title("Observation/obs-glucose-3 · 2 paths changed"),
            (
                String::from("Observation/obs-glucose-3"),
                String::from("2 paths changed")
            )
        );
        assert_eq!(
            split_diff_title("bundle"),
            (String::from("bundle"), String::new())
        );
    }

    #[test]
    fn strict_column_is_short_and_does_not_break_inside_the_flag() {
        let html = render(&sample(1, 0, 3), None);
        // The case table says "Not run" and never wraps; the finding's own
        // validation table keeps the hint.
        assert!(html.contains("<td class=\"nowrap warn\">Not run</td>"));
        assert!(html.contains("Not run, add --strict"));
        assert!(html.contains("<td class=\"mono id\">"));
    }

    #[test]
    fn strict_text_when_strict_ran_but_skipped_a_case() {
        let mut v: Validation = serde_json::from_value(serde_json::json!({
            "schema": {"status": "valid"},
            "references": {"status": "valid"},
            "strict": {"status": "not_checked", "reason": "validator timed out"}
        }))
        .unwrap();
        assert_eq!(
            layer_text(&v, Layer::Strict, true),
            "Not checked: validator timed out"
        );
        assert_eq!(
            layer_text(&v, Layer::Strict, false),
            "Not run, add --strict"
        );
        v.strict.reason = None;
        assert_eq!(layer_text(&v, Layer::Strict, true), "Not checked");
    }

    #[test]
    fn footer_and_report_make_no_network_requests() {
        let html = render(&sample(1, 0, 3), None);
        assert!(html.contains("<strong>Synthetic test data only</strong>"));
        // A long --out path in the footer must wrap, not widen a phone screen.
        assert!(html.contains("footer.bottom .wrap>span{min-width:0;overflow-wrap:anywhere}"));
        // The only absolute URL is the footer link to the guide.
        let urls: Vec<&str> = html
            .match_indices("https://")
            .map(|(i, _)| &html[i..])
            .collect();
        assert_eq!(urls.len(), 1);
        assert!(urls[0].starts_with(GUIDE_URL));
        assert!(!html.contains("http://"));
    }

    #[test]
    fn fonts_are_embedded_and_named_as_the_css_uses_them() {
        let r = sample(1, 0, 3);
        let meta = ReportMeta {
            generated_at: None,
            total_seconds: None,
            embed_fonts: true,
        };
        let html = render_html(&r, &meta).unwrap();
        assert_eq!(
            html.matches("src:url(data:font/woff2;base64,d09GMg")
                .count(),
            5
        );
        assert_eq!(html.matches("font-family:\"Geist\";").count(), 3);
        assert_eq!(html.matches("font-family:\"Geist Mono\";").count(), 2);
        assert!(html.contains("font-family:Geist,"));
    }

    #[test]
    fn junit_replays_each_failing_case_from_its_own_folder() {
        let mut r = sample(1, 0, 3);
        r.cases[1].outcome = Outcome::RuleBroken;
        r.cases[1].finding = Some(String::from("F1"));
        r.cases[1].dir = Some(String::from("cases/c-0002"));
        let xml = render_junit(&r).unwrap();
        // The representative keeps the finding's own command.
        assert!(xml.contains("Finding F1\nExpected") || xml.contains("Finding F1\nGot"));
        // Another case in the same finding points at its own folder.
        assert!(xml.contains("Finding F1, shown for case c-0001; this case failed the same way"));
        assert!(xml.contains("Replay: recordchaos replay results/cases/c-0002"));
    }

    #[test]
    fn without_run_meta_there_is_no_time_and_no_dangling_separator() {
        // results.json carries no timestamps (SPEC section 11), so a report
        // rendered without run.toml facts must read cleanly.
        let r = sample(1, 0, 3);
        let html = render_html(&r, &ReportMeta::default()).unwrap();
        assert!(
            html.contains("<div class=\"runline\">scenarios&#x2f;demo.toml</div>"),
            "{html}"
        );
        assert!(!html.contains("<dt>Time</dt>"));
        assert!(!html.contains("· ·"));
        assert!(!html.contains("run 2026"));
    }

    #[test]
    fn fonts_are_woff2() {
        for f in [
            GEIST_400,
            GEIST_500,
            GEIST_600,
            GEIST_MONO_400,
            GEIST_MONO_500,
        ] {
            assert_eq!(&f[..4], b"wOF2");
        }
    }
}
