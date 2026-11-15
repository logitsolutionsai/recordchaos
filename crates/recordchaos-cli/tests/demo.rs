//! Integration: run the demo scenario against the buggy example importer,
//! find all four seeded bugs, shrink each to one mutation and replay each
//! with the same fingerprint; run against `--fixed` and find none.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use recordchaos_core::Limits;
use recordchaos_core::bundle;
use recordchaos_core::run::{self, RunOptions, Silent};
use recordchaos_core::scenario::TargetKind;
use recordchaos_core::scenario::{TargetConfig, load_scenario, parse_target};
use recordchaos_core::target::command::CommandTarget;
use recordchaos_core::validate::{SchemaValidator, Validators};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The example binary. Ask cargo to build it and report where the
/// executable is, so this works with any target-dir or build-dir layout.
fn importer() -> PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let out = Command::new(env!("CARGO"))
            .args([
                "build",
                "--example",
                "buggy_importer",
                "-p",
                "recordchaos",
                "--message-format=json",
            ])
            .current_dir(root())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "cannot build the buggy importer:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let path = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|msg| msg["reason"] == "compiler-artifact")
            .filter(|msg| msg["target"]["name"] == "buggy_importer")
            .find_map(|msg| msg["executable"].as_str().map(PathBuf::from))
            .expect("cargo did not report the buggy importer executable");
        assert!(
            path.exists(),
            "buggy importer not found at {}",
            path.display()
        );
        path
    })
    .clone()
}

fn target(fixed: bool, state: &Path) -> TargetConfig {
    let exe = importer().display().to_string().replace('\\', "/");
    let state = state.display().to_string().replace('\\', "/");
    let flag = if fixed { "\"--fixed\", " } else { "" };
    let text = format!(
        r#"
kind = "command"
reset   = ["{exe}", {flag}"--state", "{state}", "reset"]
ingest  = ["{exe}", {flag}"--state", "{state}", "ingest"]
observe = ["{exe}", {flag}"--state", "{state}", "observe"]
timeout_ms = 20000
[accept]
rejected_exit_codes = [2]
[view]
latest_glucose = "/latest/synthetic-glucose"
patient_count  = "/patients/count"
"#
    );
    parse_target(
        &text,
        Path::new("examples/targets/test-target.toml"),
        &Limits::default(),
    )
    .unwrap()
}

fn temp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("recordchaos-test-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run_demo(fixed: bool, out: &Path, state: &Path) -> (run::RunOutput, TargetConfig) {
    let limits = Limits::default();
    let scenario = load_scenario(&root().join("scenarios/demo.toml"), &limits).unwrap();
    let base = bundle::load(&scenario.base_path, &limits).unwrap();
    let cfg = target(fixed, state);
    let TargetKind::Command(c) = &cfg.kind else {
        panic!("command target expected")
    };
    let mut tgt = CommandTarget::new(c.clone());
    let vals = Validators::new(Some(SchemaValidator::bundled().unwrap()), None);
    let opts = RunOptions {
        as_of: None,
        jobs: 2,
        shrink_budget: 64,
        out_dir: String::from("results"),
    };
    let output = run::run(&scenario, &base, &cfg, &mut tgt, &vals, &opts, &mut Silent).unwrap();
    run::write_outputs(out, &output, &base, "tool = \"recordchaos\"\n").unwrap();
    (output, cfg)
}

#[test]
fn demo_finds_the_four_seeded_bugs_and_shrinks_and_replays_them() {
    let out = temp_dir("buggy");
    let (output, cfg) = run_demo(false, &out, &out.join("state.json"));
    let r = &output.results;
    let oracles: BTreeSet<&str> = r.findings.iter().map(|f| f.oracle.as_str()).collect();
    let expected: BTreeSet<&str> = [
        "latest_correction_selected",
        "no_crash",
        "same_as_base",
        "invalid_rejected",
    ]
    .into_iter()
    .collect();
    assert_eq!(
        oracles,
        expected,
        "findings: {:#?}",
        r.findings
            .iter()
            .map(|f| (&f.id, &f.oracle, &f.sentence))
            .collect::<Vec<_>>()
    );
    assert_eq!(r.exit_code(), 1);
    // Bug to operator.
    let op_of = |oracle: &str| -> BTreeSet<String> {
        r.findings
            .iter()
            .filter(|f| f.oracle == oracle)
            .flat_map(|f| f.mutations.iter().map(|m| m.operator.clone()))
            .collect()
    };
    assert!(op_of("no_crash").contains("reference.to_urn_uuid"));
    assert!(op_of("same_as_base").contains("bundle.reorder_entries"));
    assert!(op_of("invalid_rejected").contains("reference.dangling_target"));
    let latest = op_of("latest_correction_selected");
    assert!(
        latest.contains("observation.duplicate_with_changed_issued")
            || latest.contains("observation.correction_sequence")
    );
    for f in &r.findings {
        assert!(!f.flaky, "{} is flaky", f.id);
        assert_eq!(
            f.mutations_after, 1,
            "{} shrank to {} mutations",
            f.id, f.mutations_after
        );
        assert!(f.shrink_replays <= 64 * f.cases.len());
        // Replay gives the same fingerprint.
        let case_dir = out.join("cases").join(&f.representative);
        let TargetKind::Command(c) = &cfg.kind else {
            panic!()
        };
        let mut tgt = CommandTarget::new(c.clone());
        let replay = run::replay(&case_dir, &cfg, &mut tgt, &Limits::default()).unwrap();
        assert!(
            replay.same,
            "{}: replay fingerprint {:?}, recorded {:?}",
            f.id, replay.fingerprint, replay.expected_fingerprint
        );
    }
    // Every case folder has its files.
    for f in &r.findings {
        for name in [
            "bundle.json",
            "original.json",
            "case.toml",
            "patch.json",
            "observed.json",
            "expected.json",
        ] {
            assert!(
                out.join("cases")
                    .join(&f.representative)
                    .join(name)
                    .exists(),
                "{} missing {name}",
                f.representative
            );
        }
    }
}

#[test]
fn fixed_importer_has_no_false_alarms() {
    let out = temp_dir("fixed");
    let (output, _) = run_demo(true, &out, &out.join("state.json"));
    let r = &output.results;
    assert!(
        r.findings.is_empty(),
        "false alarms: {:#?}",
        r.findings
            .iter()
            .map(|f| (&f.id, &f.oracle, &f.sentence, &f.got))
            .collect::<Vec<_>>()
    );
    assert_eq!(r.exit_code(), 0);
    assert_eq!(
        r.summary.unexpected_invalidity, 0,
        "a valid-edge operator made a schema-invalid bundle"
    );
}

#[test]
fn same_seed_gives_byte_identical_outputs() {
    let a = temp_dir("det-a");
    let b = temp_dir("det-b");
    // The same state path, so the target file text (and its hash) is the same.
    let state = temp_dir("det-state").join("state.json");
    let _ = run_demo(false, &a, &state);
    let _ = run_demo(false, &b, &state);
    let ra = std::fs::read_to_string(a.join("results.json")).unwrap();
    let rb = std::fs::read_to_string(b.join("results.json")).unwrap();
    assert_eq!(ra, rb, "results.json differs between runs");
    let ja: recordchaos_core::results::RunResults = serde_json::from_str(&ra).unwrap();
    for f in &ja.findings {
        for name in ["bundle.json", "patch.json"] {
            let x = std::fs::read(a.join("cases").join(&f.representative).join(name)).unwrap();
            let y = std::fs::read(b.join("cases").join(&f.representative).join(name)).unwrap();
            assert_eq!(x, y, "{}/{name} differs", f.representative);
        }
    }
    // results.json reads back into the same structure.
    assert_eq!(serde_json::to_string_pretty(&ja).unwrap() + "\n", ra);
}

#[test]
fn reports_render_from_results() {
    let out = temp_dir("report");
    let (output, _) = run_demo(false, &out, &out.join("state.json"));
    let meta = recordchaos_report::ReportMeta {
        generated_at: None,
        total_seconds: None,
        embed_fonts: true,
    };
    let html = recordchaos_report::render_html(&output.results, &meta).unwrap();
    assert!(html.starts_with("<!doctype html>"));
    assert!(html.contains("cases broke the importer"));
    assert!(html.contains("What This Run Did Not Check"));
    assert!(html.contains("Built by <a href=\"https://logitsolutions.ai/guides/ai-in-medical-practices/\">Logit Solutions AI</a>"));
    assert!(
        !html.contains("https://fonts.googleapis.com"),
        "the report must make no network requests"
    );
    assert!(
        html.contains("font/woff2;base64,d09GMg"),
        "fonts are not embedded"
    );
    let junit = recordchaos_report::render_junit(&output.results).unwrap();
    assert!(junit.contains("<testsuites"));
    assert!(junit.contains("<failure"));
}

#[test]
fn cli_smoke() {
    let exe = env!("CARGO_BIN_EXE_recordchaos");
    let ok = Command::new(exe)
        .args(["operators", "list"])
        .output()
        .unwrap();
    assert!(ok.status.success());
    let text = String::from_utf8_lossy(&ok.stdout);
    assert_eq!(text.lines().count(), 22);
    let v = Command::new(exe)
        .args([
            "validate",
            "fixtures/bundles/patient-a.json",
            "--color",
            "never",
        ])
        .current_dir(root())
        .output()
        .unwrap();
    assert_eq!(
        v.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&v.stdout)
    );
    let bad = Command::new(exe)
        .args(["validate", "no-such-file.json"])
        .current_dir(root())
        .output()
        .unwrap();
    assert_eq!(bad.status.code(), Some(2));
    let strict = Command::new(exe)
        .args([
            "validate",
            "fixtures/bundles/patient-a.json",
            "--strict",
            "--validator-jar",
            "no-such.jar",
        ])
        .current_dir(root())
        .output()
        .unwrap();
    assert_eq!(strict.status.code(), Some(3));
}

/// The CLI writes the same results.json and report.html for the same seed,
/// even into different folders: no timestamps or wall times in either.
#[test]
fn cli_report_and_results_are_byte_identical_across_runs() {
    let exe = env!("CARGO_BIN_EXE_recordchaos");
    let dir = temp_dir("cli-det");
    let base = root().join("fixtures/bundles/patient-a.json");
    let scenario = dir.join("small.toml");
    std::fs::write(
        &scenario,
        format!(
            "fhir_version = \"4.0.1\"
base = \"{}\"
seed = 5
cases = 6
max_mutations_per_case = 2

[[operator]]
id = \"bundle.reorder_entries\"

[[operator]]
id = \"reference.dangling_target\"
",
            base.display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();
    let target_path = dir.join("target.toml");
    let t = target(false, &dir.join("state.json"));
    let TargetKind::Command(c) = &t.kind else {
        panic!()
    };
    let q = |v: &[String]| {
        v.iter()
            .map(|a| format!("\"{}\"", a.replace('\\', "/")))
            .collect::<Vec<_>>()
            .join(", ")
    };
    std::fs::write(
        &target_path,
        format!(
            "kind = \"command\"
reset = [{}]
ingest = [{}]
observe = [{}]
timeout_ms = 20000
[accept]
rejected_exit_codes = [2]
[view]
latest_glucose = \"/latest/synthetic-glucose\"
patient_count = \"/patients/count\"
",
            q(c.reset.as_deref().unwrap_or_default()),
            q(&c.ingest),
            q(&c.observe)
        ),
    )
    .unwrap();
    let mut outs = Vec::new();
    let mut raw_results = String::new();
    for name in ["out one", "out two"] {
        let out = dir.join(name);
        let st = Command::new(exe)
            .args(["--color", "never", "run"])
            .arg(&scenario)
            .arg("--target")
            .arg(&target_path)
            .arg("--out")
            .arg(name)
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(
            matches!(st.status.code(), Some(0 | 1)),
            "{}",
            String::from_utf8_lossy(&st.stderr)
        );
        let norm = |f: &str| {
            std::fs::read_to_string(out.join(f))
                .unwrap()
                .replace(name, "OUT")
                .replace(&name.replace(' ', "&#x20;"), "OUT")
        };
        outs.push((norm("results.json"), norm("report.html")));
        raw_results = std::fs::read_to_string(out.join("results.json")).unwrap();
    }
    assert_eq!(outs[0].0, outs[1].0, "results.json differs between runs");
    assert_eq!(outs[0].1, outs[1].1, "report.html differs between runs");
    // A replay line for a folder with a space in it is quoted.
    if raw_results.contains("recordchaos replay") {
        assert!(
            raw_results.contains("recordchaos replay 'out two/cases/"),
            "unquoted replay path"
        );
    }
}

/// HTML-escape text the way the report's minijinja templates do.
fn html_escaped(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
        .replace('/', "&#x2f;")
}

/// The real demo through the CLI, as a user runs it: from a folder that
/// holds `scenarios/demo.toml` and `fixtures/bundles/patient-a.json`, with
/// a relative `--out`. Returns (folder, terminal output). Run once and
/// shared by the snapshot and schema tests.
fn cli_demo() -> &'static (PathBuf, String) {
    static RUN: OnceLock<(PathBuf, String)> = OnceLock::new();
    RUN.get_or_init(|| {
        let dir = temp_dir("cli-demo");
        std::fs::create_dir_all(dir.join("scenarios")).unwrap();
        std::fs::create_dir_all(dir.join("fixtures/bundles")).unwrap();
        std::fs::copy(
            root().join("scenarios/demo.toml"),
            dir.join("scenarios/demo.toml"),
        )
        .unwrap();
        std::fs::copy(
            root().join("fixtures/bundles/patient-a.json"),
            dir.join("fixtures/bundles/patient-a.json"),
        )
        .unwrap();
        let exe = importer().display().to_string().replace('\\', "/");
        std::fs::write(
            dir.join("buggy.toml"),
            format!(
                "kind = \"command\"
reset = [\"{exe}\", \"--state\", \"state.json\", \"reset\"]
ingest = [\"{exe}\", \"--state\", \"state.json\", \"ingest\"]
observe = [\"{exe}\", \"--state\", \"state.json\", \"observe\"]
timeout_ms = 20000
[accept]
rejected_exit_codes = [2]
[view]
latest_glucose = \"/latest/synthetic-glucose\"
patient_count = \"/patients/count\"
"
            ),
        )
        .unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_recordchaos"))
            .args([
                "--color",
                "never",
                "run",
                "scenarios/demo.toml",
                "--target",
                "buggy.toml",
                "--out",
                "results",
                "--jobs",
                "2",
            ])
            .current_dir(&dir)
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let text = String::from_utf8_lossy(&out.stdout).replace(&exe, "[IMPORTER]");
        (dir, text)
    })
}

/// Settings that hide what differs between machines and runs: timings,
/// hashes of input files (line endings differ between checkouts) and the
/// embedded font data.
fn snapshot_settings() -> insta::Settings {
    let mut s = insta::Settings::clone_current();
    s.set_prepend_module_to_snapshot(false);
    s.add_filter(r"(?m)[ ]+\d+\.\d+ s$", "  [TIME]");
    s.add_filter(r"\([0-9a-f]{4}…[0-9a-f]{4}\)", "([HASH])");
    s.add_filter(r"[0-9a-f]{64}", "[SHA256]");
    s.add_filter(r"base64,[A-Za-z0-9+/=]+", "base64,[FONT]");
    s
}

#[test]
fn demo_terminal_output_snapshot() {
    let (_, text) = cli_demo();
    snapshot_settings().bind(|| insta::assert_snapshot!("demo_terminal", text));
}

#[test]
fn demo_report_html_snapshot() {
    let (dir, _) = cli_demo();
    let exe = importer().display().to_string().replace('\\', "/");
    let html = std::fs::read_to_string(dir.join("results/report.html"))
        .unwrap()
        .replace(&html_escaped(&exe), "[IMPORTER]");
    assert!(
        !html.contains("Users"),
        "a machine path leaked into the report"
    );
    snapshot_settings().bind(|| insta::assert_snapshot!("demo_report_html", html));
}

/// Keys in `value` that the schema does not declare, following `$ref`,
/// `properties` and `items`. The hand-written schema has no
/// `additionalProperties: false`, so this keeps it honest.
fn undeclared(
    schema: &serde_json::Value,
    root: &serde_json::Value,
    value: &serde_json::Value,
    at: &str,
    out: &mut Vec<String>,
) {
    let mut node = schema;
    while let Some(r) = node.get("$ref").and_then(|r| r.as_str()) {
        node = root
            .pointer(r.trim_start_matches('#'))
            .unwrap_or_else(|| panic!("bad $ref {r}"));
    }
    match value {
        serde_json::Value::Object(map) => {
            let Some(props) = node.get("properties").and_then(|p| p.as_object()) else {
                return;
            };
            for (k, v) in map {
                match props.get(k) {
                    Some(sub) => undeclared(sub, root, v, &format!("{at}/{k}"), out),
                    None => out.push(format!("{at}/{k}")),
                }
            }
        }
        serde_json::Value::Array(items) => {
            if let Some(sub) = node.get("items") {
                for (i, v) in items.iter().enumerate() {
                    undeclared(sub, root, v, &format!("{at}/{i}"), out);
                }
            }
        }
        _ => {}
    }
}

#[test]
fn results_json_matches_the_published_schema() {
    let (dir, _) = cli_demo();
    let schema: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(root().join("docs/results-schema.json")).unwrap(),
    )
    .unwrap();
    let validator = jsonschema::draft202012::options()
        .should_validate_formats(true)
        .build(&schema)
        .unwrap();
    let results: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("results/results.json")).unwrap())
            .unwrap();
    let errors: Vec<String> = validator
        .iter_errors(&results)
        .map(|e| format!("{}: {e}", e.instance_path()))
        .collect();
    assert!(
        errors.is_empty(),
        "results.json fails the schema:\n{}",
        errors.join("\n")
    );
    let mut extra = Vec::new();
    undeclared(&schema, &schema, &results, "", &mut extra);
    extra.sort();
    extra.dedup();
    assert!(
        extra.is_empty(),
        "results.json has keys the schema does not declare: {extra:?}"
    );
    // The schema is not vacuous: a results.json without its summary fails.
    let mut broken = results.clone();
    broken.as_object_mut().unwrap().remove("summary");
    assert!(!validator.is_valid(&broken));
}

/// `recordchaos init` embeds a copy of patient-a.json from inside the crate,
/// so the published package builds on its own. The copy must not drift.
#[test]
fn starter_bundle_is_the_patient_a_fixture() {
    let copy =
        std::fs::read(root().join("crates/recordchaos-cli/assets/starter-bundle.json")).unwrap();
    let fixture = std::fs::read(root().join("fixtures/bundles/patient-a.json")).unwrap();
    assert!(
        copy == fixture,
        "update crates/recordchaos-cli/assets/starter-bundle.json from fixtures/bundles/patient-a.json"
    );
}
