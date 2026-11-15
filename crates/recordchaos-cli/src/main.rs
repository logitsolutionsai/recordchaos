//! recordchaos: breaks synthetic HL7® FHIR® R4 patient record bundles on
//! purpose to test whether your importer copes.
//!
//! Exit codes: 0 no failures, 1 failures found, 2 invalid input or config,
//! 3 target or validator unavailable, 4 internal error.

mod term;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand, ValueEnum};
use recordchaos_core::bundle;
use recordchaos_core::error::{EXIT_CLEAN, EXIT_FAILURES, Error, Result, read_text, write_text};
use recordchaos_core::generate::{default_as_of, generate};
use recordchaos_core::json::{canonical, canonical_of, short_hash};
use recordchaos_core::operators::{REGISTRY, find, markdown_doc, registry::caught_by_text};
use recordchaos_core::results::RunResults;
use recordchaos_core::run::{self, Progress, RunOptions};
use recordchaos_core::scenario::{
    TargetConfig, TargetKind, display_path, load_scenario, load_target,
};
use recordchaos_core::target::Target;
use recordchaos_core::target::command::CommandTarget;
use recordchaos_core::target::http::{HttpTarget, check_host};
use recordchaos_core::validate::{
    Layer, LayerStatus, SchemaValidator, StrictValidator, Validators,
};
use recordchaos_core::{Limits, TOOL_VERSION, synthetic, time};
use recordchaos_report::{ReportMeta, render_html, render_junit};
use term::{ColorChoice, Term};

#[derive(Parser, Debug)]
#[command(
    name = "recordchaos",
    version,
    about = "Breaks synthetic HL7 FHIR R4 patient record bundles on purpose to test your importer.",
    after_help = "Exit codes: 0 no failures, 1 failures found, 2 invalid input or config, 3 target or validator unavailable, 4 internal error.\nSynthetic test data only."
)]
struct Cli {
    /// Print only the verdict and errors.
    #[arg(long, global = true)]
    quiet: bool,
    /// Machine output on stdout; logs on stderr.
    #[arg(long, global = true)]
    json: bool,
    /// When to use colour. NO_COLOR is honoured with auto.
    #[arg(long, global = true, value_enum, default_value_t = ColorChoice::Auto)]
    color: ColorChoice,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum ReportFormat {
    Html,
    Json,
    Junit,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum ListFormat {
    Text,
    Markdown,
    Json,
}

#[derive(Subcommand, Debug)]
enum OperatorsCmd {
    /// List the 22 operators.
    List {
        #[arg(long, value_enum, default_value_t = ListFormat::Text)]
        format: ListFormat,
    },
    /// Describe one operator: what it changes, when it applies, an example.
    Describe { id: String },
}

fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Validate a bundle on every layer.
    Validate {
        bundle: PathBuf,
        /// Also run the HL7 Java validator (needs Java 17 and validator_cli.jar).
        #[arg(long)]
        strict: bool,
        /// Path to validator_cli.jar (or set RECORDCHAOS_VALIDATOR_JAR).
        #[arg(long)]
        validator_jar: Option<PathBuf>,
    },
    /// List or describe the mutation operators.
    Operators {
        #[command(subcommand)]
        cmd: OperatorsCmd,
    },
    /// Generate cases only, with no target.
    Generate {
        scenario: PathBuf,
        #[arg(long)]
        out: PathBuf,
        /// Reference date for time operators (default: the latest date in the base bundle).
        #[arg(long)]
        as_of: Option<String>,
        #[arg(long, default_value_t = default_jobs())]
        jobs: usize,
        /// Continue when the base bundle has values that look like real identifiers.
        #[arg(long)]
        i_confirm_synthetic: bool,
    },
    /// Generate cases, run them on a target, check, shrink and report.
    Run {
        scenario: PathBuf,
        #[arg(long)]
        target: PathBuf,
        #[arg(long, default_value = "results")]
        out: PathBuf,
        #[arg(long)]
        strict: bool,
        #[arg(long)]
        validator_jar: Option<PathBuf>,
        #[arg(long)]
        as_of: Option<String>,
        /// Threads for validation. Cases run on the target one at a time.
        #[arg(long, default_value_t = default_jobs())]
        jobs: usize,
        /// Replays allowed per failure while shrinking.
        #[arg(long, default_value_t = 64)]
        shrink_budget: usize,
        /// Allow a non-loopback HTTP target that is listed in allow_hosts.
        #[arg(long)]
        allow_remote: bool,
        /// Continue when the base bundle has values that look like real identifiers.
        #[arg(long)]
        i_confirm_synthetic: bool,
    },
    /// Replay a case folder against the target.
    Replay {
        case_dir: PathBuf,
        /// Target file (default: the one recorded in case.toml).
        #[arg(long)]
        target: Option<PathBuf>,
        #[arg(long)]
        allow_remote: bool,
    },
    /// Write the report again from a results folder.
    Report {
        results_dir: PathBuf,
        #[arg(long, value_enum, default_value_t = ReportFormat::Html)]
        format: ReportFormat,
    },
    /// Write a starter scenario and target into a folder.
    Init {
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },
}

fn main() -> ExitCode {
    // A panic is an internal error: one line on stderr and exit 4, never
    // Rust's default exit 101 with a backtrace hint.
    std::panic::set_hook(Box::new(|info| {
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| String::from("unknown panic"));
        let place = info
            .location()
            .map(|l| format!(" at {}:{}", l.file(), l.line()))
            .unwrap_or_default();
        let _ = writeln!(
            std::io::stderr(),
            "✗ internal error{place}: {}. Please report it with the command you ran.",
            msg.lines().next().unwrap_or("")
        );
        std::process::exit(recordchaos_core::error::EXIT_INTERNAL);
    }));
    let cli = Cli::parse();
    let t = Term::new(cli.color, cli.json, cli.quiet);
    let code = match dispatch(&cli, &t) {
        Ok(code) => code,
        Err(e) => {
            t.error(e.message());
            e.exit_code()
        }
    };
    ExitCode::from(u8::try_from(code).unwrap_or(4))
}

fn dispatch(cli: &Cli, t: &Term) -> Result<i32> {
    match &cli.command {
        Cmd::Validate {
            bundle,
            strict,
            validator_jar,
        } => cmd_validate(cli, t, bundle, *strict, validator_jar.as_deref()),
        Cmd::Operators { cmd } => cmd_operators(cli, t, cmd),
        Cmd::Generate {
            scenario,
            out,
            as_of,
            jobs,
            i_confirm_synthetic,
        } => cmd_generate(
            cli,
            t,
            scenario,
            out,
            as_of.clone(),
            *jobs,
            *i_confirm_synthetic,
        ),
        Cmd::Run {
            scenario,
            target,
            out,
            strict,
            validator_jar,
            as_of,
            jobs,
            shrink_budget,
            allow_remote,
            i_confirm_synthetic,
        } => cmd_run(
            cli,
            t,
            RunArgs {
                scenario,
                target,
                out,
                strict: *strict,
                validator_jar: validator_jar.as_deref(),
                as_of: as_of.clone(),
                jobs: *jobs,
                shrink_budget: *shrink_budget,
                allow_remote: *allow_remote,
                i_confirm_synthetic: *i_confirm_synthetic,
            },
        ),
        Cmd::Replay {
            case_dir,
            target,
            allow_remote,
        } => cmd_replay(cli, t, case_dir, target.as_deref(), *allow_remote),
        Cmd::Report {
            results_dir,
            format,
        } => cmd_report(t, results_dir, *format),
        Cmd::Init { dir } => cmd_init(t, dir),
    }
}

fn validators(strict: bool, jar: Option<&Path>) -> Result<Validators> {
    let schema = SchemaValidator::bundled()?;
    let strict = if strict {
        Some(StrictValidator::locate(jar)?)
    } else {
        None
    };
    Ok(Validators::new(Some(schema), strict))
}

fn now_utc() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    time::format_with_offset(i64::try_from(secs).unwrap_or(0), "", 0)
}

fn check_synthetic(t: &Term, value: &serde_json::Value, confirmed: bool) -> Result<()> {
    let suspects = synthetic::scan(value);
    if suspects.is_empty() {
        return Ok(());
    }
    t.warn(&format!(
        "the base bundle has {} values that look like real identifiers:",
        suspects.len()
    ));
    for s in suspects.iter().take(5) {
        t.warn(&format!("  {} at {}: {:?}", s.kind, s.path, s.sample));
    }
    t.warn("This check is a heuristic. It does not prove that a bundle is de-identified.");
    if confirmed {
        return Ok(());
    }
    Err(Error::input(
        "use synthetic data only. If this bundle is synthetic, pass --i-confirm-synthetic to continue.",
    ))
}

// validate ------------------------------------------------------------------

fn cmd_validate(cli: &Cli, t: &Term, path: &Path, strict: bool, jar: Option<&Path>) -> Result<i32> {
    let limits = Limits::default();
    let loaded = bundle::load(path, &limits)?;
    let v = validators(strict, jar)?.validate(&loaded.value);
    if cli.json {
        println!("{}", canonical_of(&v)?.trim_end());
    }
    t.line(&t.muted(&format!(
        "recordchaos {TOOL_VERSION} · {} ({})",
        loaded.source,
        short_hash(&loaded.sha256)
    )));
    t.blank();
    for l in [Layer::Schema, Layer::References, Layer::Strict] {
        let r = v.get(l);
        let (mark, text) = match r.status {
            LayerStatus::Valid => (t.muted("✓"), r.short(l)),
            LayerStatus::Invalid => (t.fail("✗"), r.short(l)),
            LayerStatus::NotChecked => (
                t.caution("○"),
                r.reason.clone().unwrap_or_else(|| r.short(l)),
            ),
        };
        t.line(&format!("{mark} {:<12}{text}", l.name()));
        for i in r.issues.iter().take(10) {
            t.line(&format!("    {} {}", t.muted(&i.path), i.message));
        }
    }
    t.blank();
    let invalid = [Layer::Schema, Layer::References, Layer::Strict]
        .iter()
        .any(|l| v.get(*l).status == LayerStatus::Invalid);
    if invalid {
        t.always(&t.bold("The bundle is invalid on at least one layer."));
        Ok(EXIT_FAILURES)
    } else {
        let not_run = if v.strict.status == LayerStatus::NotChecked {
            " HL7 validator not run."
        } else {
            ""
        };
        t.always(&t.bold(&format!(
            "The bundle is valid on every layer that ran.{not_run}"
        )));
        Ok(EXIT_CLEAN)
    }
}

// operators -----------------------------------------------------------------

fn cmd_operators(cli: &Cli, t: &Term, cmd: &OperatorsCmd) -> Result<i32> {
    match cmd {
        OperatorsCmd::List { format } => {
            let format = if cli.json { ListFormat::Json } else { *format };
            match format {
                ListFormat::Markdown => print!("{}", markdown_doc()),
                ListFormat::Json => {
                    let list: Vec<serde_json::Value> = REGISTRY
                        .iter()
                        .map(|o| {
                            serde_json::json!({
                                "number": o.info.number,
                                "id": o.info.id,
                                "version": o.info.version,
                                "summary": o.info.summary,
                                "intent": o.info.intent,
                                "caught_by": caught_by_text(o),
                                "view_effect": o.info.view_effect,
                            })
                        })
                        .collect();
                    println!("{}", canonical(&serde_json::Value::Array(list))?.trim_end());
                }
                ListFormat::Text => {
                    for o in REGISTRY.iter() {
                        t.line(&format!(
                            "{:>2}  {:<46}{:<30}{}",
                            o.info.number,
                            o.info.id,
                            o.info.intent.label(),
                            t.muted(&caught_by_text(o))
                        ));
                    }
                }
            }
            Ok(EXIT_CLEAN)
        }
        OperatorsCmd::Describe { id } => {
            let op = find(id).ok_or_else(|| {
                Error::input(format!(
                    "unknown operator {id:?}. Run `recordchaos operators list`."
                ))
            })?;
            let i = &op.info;
            t.line(&t.bold(&format!("{}. {}", i.number, i.id)));
            t.line(&format!("{}.", i.summary));
            t.blank();
            for (k, v) in [
                ("Version", i.version.to_string()),
                ("Intended", i.intent.label().to_string()),
                ("Caught by", caught_by_text(op)),
                ("Effect", i.view_effect.to_string()),
                ("Eligible", i.eligibility.to_string()),
                ("Changes", i.paths.to_string()),
                ("Reversible", i.reversible.to_string()),
            ] {
                t.line(&format!("{}{v}", t.muted(&format!("{k:<12}"))));
            }
            for a in i.args {
                t.line(&format!(
                    "{}{} (default {}): {}",
                    t.muted(&format!("{:<12}", "Argument")),
                    a.name,
                    a.default,
                    a.help
                ));
            }
            t.blank();
            t.line(&format!(
                "{}{}",
                t.muted(&format!("{:<12}", "Before")),
                i.before
            ));
            t.line(&format!(
                "{}{}",
                t.muted(&format!("{:<12}", "After")),
                i.after
            ));
            Ok(EXIT_CLEAN)
        }
    }
}

// generate ------------------------------------------------------------------

fn cmd_generate(
    cli: &Cli,
    t: &Term,
    scenario_path: &Path,
    out: &Path,
    as_of: Option<String>,
    jobs: usize,
    confirm: bool,
) -> Result<i32> {
    let limits = Limits::default();
    let scenario = load_scenario(scenario_path, &limits)?;
    let base = bundle::load(&scenario.base_path, &limits)?;
    check_synthetic(t, &base.value, confirm)?;
    let vals = validators(false, None)?;
    run::check_base(&base, &vals)?;
    let as_of = as_of
        .or_else(|| scenario.as_of.clone())
        .unwrap_or_else(|| default_as_of(&base.value));
    let started = std::time::Instant::now();
    let cases = generate(&base.value, &scenario, &as_of);
    let refs: Vec<&serde_json::Value> = cases.iter().map(|c| &c.bundle).collect();
    let validations = vals.validate_all(&refs, jobs);
    let mut list = Vec::new();
    for (c, v) in cases.iter().zip(validations.iter()) {
        let dir = out.join("cases").join(&c.id);
        write_text(&dir.join("bundle.json"), &canonical(&c.bundle)?)?;
        write_text(&dir.join("patch.json"), &canonical_of(&c.patch)?)?;
        if !c.pre_steps.is_empty() {
            write_text(
                &dir.join("pre-steps.json"),
                &canonical(&serde_json::Value::Array(c.pre_steps.clone()))?,
            )?;
        }
        list.push(serde_json::json!({
            "id": c.id,
            "seed": c.seed,
            "intent": c.intent(),
            "mutations": c.mutations,
            "not_applicable": c.not_applicable,
            "validation": v,
        }));
    }
    let summary = serde_json::json!({
        "tool": "recordchaos",
        "tool_version": TOOL_VERSION,
        "rng": recordchaos_core::RNG_NAME,
        "seed": scenario.seed,
        "scenario": scenario.path,
        "base": base.source,
        "base_sha256": base.sha256,
        "as_of": as_of,
        "cases": list,
    });
    write_text(&out.join("cases.json"), &canonical(&summary)?)?;
    if cli.json {
        println!("{}", canonical(&summary)?.trim_end());
    }
    let schema_ok = validations
        .iter()
        .filter(|v| v.schema.status == LayerStatus::Valid)
        .count();
    let refs_ok = validations
        .iter()
        .filter(|v| v.references.status == LayerStatus::Valid)
        .count();
    t.line(&t.muted(&format!(
        "recordchaos {TOOL_VERSION} · seed {} · base {} ({})",
        scenario.seed,
        base.source,
        short_hash(&base.sha256)
    )));
    t.blank();
    t.stage(
        "generate",
        &format!("{} cases", cases.len()),
        Some(started.elapsed().as_secs_f64()),
    );
    t.stage(
        "validate",
        &format!(
            "schema {schema_ok}/{} · references {refs_ok}/{} · {}",
            cases.len(),
            cases.len(),
            t.caution("strict not run")
        ),
        None,
    );
    t.blank();
    t.always(&format!(
        "{} {}",
        t.muted("cases    "),
        display_path(&out.join("cases"))
    ));
    Ok(EXIT_CLEAN)
}

// run -----------------------------------------------------------------------

struct RunArgs<'a> {
    scenario: &'a Path,
    target: &'a Path,
    out: &'a Path,
    strict: bool,
    validator_jar: Option<&'a Path>,
    as_of: Option<String>,
    jobs: usize,
    shrink_budget: usize,
    allow_remote: bool,
    i_confirm_synthetic: bool,
}

struct TermProgress<'a> {
    t: &'a Term,
}

impl Progress for TermProgress<'_> {
    fn stage(&mut self, name: &str, text: &str, seconds: Option<f64>) {
        match name {
            "outcomes" => {
                // `✓ 18 handled   ✓ 3 rejected cleanly   ✗ 3 rule broken   ✗ 1 crashed`
                let parts: Vec<&str> = text.split(" · ").collect();
                let mut out = String::from("          ");
                for (i, p) in parts.iter().enumerate() {
                    out.push_str(&outcome_part(self.t, i, p));
                    out.push_str("   ");
                }
                self.t.line(out.trim_end());
            }
            "validate" if text.contains("strict not run") => {
                let (head, _) = text.split_once("strict not run").unwrap_or((text, ""));
                self.t.stage(
                    name,
                    &format!("{head}{}", self.t.caution("strict not run (add --strict)")),
                    None,
                );
            }
            _ => self.t.stage(name, text, seconds),
        }
    }
}

/// One count on the outcomes line. The first two counts (handled, rejected
/// cleanly) are good and get ✓; the last two (rule broken, crashed) are
/// failures and get ✗. A zero count gets no symbol and is muted, so a ✓ never
/// sits next to "0 handled" and a ✗ never next to "0 crashed".
fn outcome_part(t: &Term, index: usize, part: &str) -> String {
    if part.starts_with("0 ") {
        t.muted(&format!("  {part}"))
    } else if index >= 2 {
        t.fail(&format!("✗ {part}"))
    } else {
        format!("{} {part}", t.muted("✓"))
    }
}

fn make_target(cfg: &TargetConfig, allow_remote: bool) -> Result<Box<dyn Target>> {
    match &cfg.kind {
        TargetKind::Command(c) => Ok(Box::new(CommandTarget::new(c.clone()))),
        TargetKind::Http(h) => {
            check_host(&h.base_url, &cfg.allow_hosts, allow_remote)?;
            Ok(Box::new(HttpTarget::new(h.clone())))
        }
    }
}

/// Report metadata for `recordchaos report`. Like `run`, it leaves the run
/// time out, so a regenerated report.html matches the original byte for byte.
fn read_run_meta(_dir: &Path) -> ReportMeta {
    ReportMeta {
        generated_at: None,
        total_seconds: None,
        embed_fonts: true,
    }
}

fn run_toml(
    a: &RunArgs<'_>,
    results: &RunResults,
    timings: &run::Timings,
    vals: &Validators,
    started: &str,
    finished: &str,
) -> Result<String> {
    let mut root = toml::Table::new();
    let s = |v: &str| toml::Value::String(v.to_string());
    root.insert(String::from("tool"), s("recordchaos"));
    root.insert(String::from("tool_version"), s(TOOL_VERSION));
    root.insert(String::from("rng"), s(recordchaos_core::RNG_NAME));
    root.insert(
        String::from("seed"),
        toml::Value::Integer(i64::try_from(results.seed).unwrap_or(i64::MAX)),
    );
    root.insert(String::from("started_at"), s(started));
    root.insert(String::from("finished_at"), s(finished));
    root.insert(String::from("scenario"), s(&results.scenario.path));
    root.insert(String::from("scenario_sha256"), s(&results.scenario.sha256));
    root.insert(String::from("target"), s(&results.target.path));
    root.insert(String::from("target_sha256"), s(&results.target.sha256));
    root.insert(String::from("base"), s(&results.base.path));
    root.insert(String::from("base_sha256"), s(&results.base.sha256));
    root.insert(String::from("as_of"), s(&results.as_of));
    root.insert(
        String::from("jobs"),
        toml::Value::Integer(i64::try_from(a.jobs).unwrap_or(1)),
    );
    root.insert(
        String::from("shrink_budget"),
        toml::Value::Integer(i64::try_from(a.shrink_budget).unwrap_or(64)),
    );
    let mut validators_t = toml::Table::new();
    validators_t.insert(
        String::from("schema"),
        s("bundled R4 fhir.schema.json (4.0.1), jsonschema 0.61"),
    );
    validators_t.insert(
        String::from("references"),
        s(&format!("recordchaos {TOOL_VERSION}")),
    );
    validators_t.insert(
        String::from("strict"),
        s(&vals
            .strict
            .as_ref()
            .map(|v| v.version.clone())
            .unwrap_or_else(|| String::from("not run"))),
    );
    root.insert(String::from("validators"), toml::Value::Table(validators_t));
    let mut tm = toml::Table::new();
    for (k, v) in [
        ("generate_s", timings.generate_s),
        ("validate_s", timings.validate_s),
        ("baseline_s", timings.baseline_s),
        ("run_s", timings.run_s),
        ("shrink_s", timings.shrink_s),
        ("total_s", timings.total_s),
    ] {
        tm.insert(
            k.to_string(),
            toml::Value::Float((v * 1000.0).round() / 1000.0),
        );
    }
    root.insert(String::from("timings"), toml::Value::Table(tm));
    let cases = results.summary.ran.max(1) as f64;
    root.insert(
        String::from("cases_per_second"),
        toml::Value::Float(((cases / timings.run_s.max(1e-9)) * 10.0).round() / 10.0),
    );
    toml::to_string_pretty(&root)
        .map_err(|e| Error::internal(format!("cannot write run.toml: {e}")))
}

fn cmd_run(cli: &Cli, t: &Term, a: RunArgs<'_>) -> Result<i32> {
    let limits = Limits::default();
    let started = now_utc();
    let scenario = load_scenario(a.scenario, &limits)?;
    let target_cfg = load_target(a.target, &limits)?;
    let base = bundle::load(&scenario.base_path, &limits)?;
    check_synthetic(t, &base.value, a.i_confirm_synthetic)?;
    let mut target = make_target(&target_cfg, a.allow_remote)?;
    let vals = validators(a.strict, a.validator_jar)?;
    let argv: Vec<String> = std::env::args()
        .skip(1)
        .map(|a| recordchaos_core::scenario::shell_arg(&a))
        .collect();
    t.line(&format!("{}recordchaos {}", t.muted("$ "), argv.join(" ")));
    t.line(&t.muted(&format!(
        "recordchaos {TOOL_VERSION} · seed {} · base {} ({})",
        scenario.seed,
        base.source,
        short_hash(&base.sha256)
    )));
    t.blank();
    let out_display = display_path(a.out);
    let opts = RunOptions {
        as_of: a.as_of.clone(),
        jobs: a.jobs,
        shrink_budget: a.shrink_budget,
        out_dir: out_display.clone(),
    };
    let mut progress = TermProgress { t };
    let output = run::run(
        &scenario,
        &base,
        &target_cfg,
        target.as_mut(),
        &vals,
        &opts,
        &mut progress,
    )?;
    let finished = now_utc();
    let toml_text = run_toml(
        &a,
        &output.results,
        &output.timings,
        &vals,
        &started,
        &finished,
    )?;
    run::write_outputs(a.out, &output, &base, &toml_text)?;
    // No timestamps or wall times in the report: the same seed and inputs
    // give a byte-identical report.html. They live in run.toml only.
    let meta = ReportMeta {
        generated_at: None,
        total_seconds: None,
        embed_fonts: true,
    };
    let html = render_html(&output.results, &meta).map_err(|e| Error::internal(e.to_string()))?;
    write_text(&a.out.join("report.html"), &html)?;
    let junit = render_junit(&output.results).map_err(|e| Error::internal(e.to_string()))?;
    write_text(&a.out.join("junit.xml"), &junit)?;

    let r = &output.results;
    if cli.json {
        println!("{}", canonical_of(r)?.trim_end());
    }
    if !r.findings.is_empty() {
        t.blank();
    }
    for f in &r.findings {
        let shrink = if f.flaky {
            t.caution(f.reproduced.as_deref().unwrap_or("flaky"))
        } else if f.mutations_after < f.mutations_before {
            format!(
                "{} {} (from {})",
                f.mutations_after,
                if f.mutations_after == 1 {
                    "mutation"
                } else {
                    "mutations"
                },
                f.mutations_before
            )
        } else {
            String::from("already minimal")
        };
        let cases = format!(
            "{} {}",
            f.cases.len(),
            if f.cases.len() == 1 { "case" } else { "cases" }
        );
        let head = format!("✗ {:<3} {:<28}", f.id, f.oracle);
        t.line(&format!("{} {cases:<9} {shrink}", t.fail_bold(&head)));
        t.line(&format!("      {}", f.sentence));
        t.line(&t.link(&format!("      {}", f.replay)));
    }
    t.blank();
    t.always(&t.bold(&format!("{}.", r.verdict())));
    let code = r.exit_code();
    t.always(&format!(
        "{}{}   {}{}   {}",
        t.muted("report    "),
        display_path(&a.out.join("report.html")),
        t.muted("junit "),
        display_path(&a.out.join("junit.xml")),
        t.muted(&format!("exit {code}"))
    ));
    Ok(code)
}

// replay --------------------------------------------------------------------

fn cmd_replay(
    cli: &Cli,
    t: &Term,
    case_dir: &Path,
    target: Option<&Path>,
    allow_remote: bool,
) -> Result<i32> {
    let limits = Limits::default();
    let case_text = read_text(&case_dir.join("case.toml"), 1024 * 1024)?;
    let table: toml::Table = toml::from_str(&case_text).map_err(|e| {
        recordchaos_core::error::toml_error(
            &display_path(&case_dir.join("case.toml")),
            &case_text,
            &e,
        )
    })?;
    let target_path = match target {
        Some(p) => p.to_path_buf(),
        None => PathBuf::from(
            table
                .get("target")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| Error::input("case.toml names no target; pass --target"))?,
        ),
    };
    let cfg = load_target(&target_path, &limits)?;
    let mut tgt = make_target(&cfg, allow_remote)?;
    let outcome = run::replay(case_dir, &cfg, tgt.as_mut(), &limits)?;
    if cli.json {
        let v = serde_json::json!({
            "case": display_path(case_dir),
            "expected_fingerprint": outcome.expected_fingerprint,
            "fingerprint": outcome.fingerprint,
            "same_fingerprint": outcome.same,
            "passes": outcome.passes,
            "oracles": outcome.oracles,
        });
        println!("{}", canonical(&v)?.trim_end());
    }
    let id = table
        .get("id")
        .and_then(toml::Value::as_str)
        .unwrap_or("case");
    if outcome.passes {
        t.always(&format!(
            "{} {id} passes now. Every applicable check passed.",
            t.muted("✓")
        ));
        return Ok(EXIT_CLEAN);
    }
    let sentence = outcome.sentence.clone().unwrap_or_default();
    if outcome.same {
        t.always(&format!(
            "{} {id} still fails with the same fingerprint.",
            t.fail("✗")
        ));
    } else {
        t.always(&format!(
            "{} {id} fails with a different fingerprint than recorded.",
            t.fail("✗")
        ));
        if let Some(e) = &outcome.expected_fingerprint {
            t.line(&t.muted(&format!("      recorded {e}")));
        }
        if let Some(g) = &outcome.fingerprint {
            t.line(&t.muted(&format!("      now      {g}")));
        }
    }
    t.always(&format!("      {sentence}"));
    Ok(EXIT_FAILURES)
}

// report --------------------------------------------------------------------

fn cmd_report(t: &Term, dir: &Path, format: ReportFormat) -> Result<i32> {
    let text = read_text(&dir.join("results.json"), 512 * 1024 * 1024)?;
    let results: RunResults = serde_json::from_str(&text).map_err(|e| {
        Error::input(format!(
            "{} is not a recordchaos results.json: {e}",
            display_path(&dir.join("results.json"))
        ))
    })?;
    match format {
        ReportFormat::Json => {
            println!("{}", canonical_of(&results)?.trim_end());
        }
        ReportFormat::Html => {
            let html = render_html(&results, &read_run_meta(dir))
                .map_err(|e| Error::internal(e.to_string()))?;
            let path = dir.join("report.html");
            write_text(&path, &html)?;
            t.always(&format!("{}{}", t.muted("report    "), display_path(&path)));
        }
        ReportFormat::Junit => {
            let xml = render_junit(&results).map_err(|e| Error::internal(e.to_string()))?;
            let path = dir.join("junit.xml");
            write_text(&path, &xml)?;
            t.always(&format!("{}{}", t.muted("junit     "), display_path(&path)));
        }
    }
    Ok(results.exit_code())
}

// init ----------------------------------------------------------------------

/// A synthetic starter bundle, so `init` works outside this repository.
const STARTER_BUNDLE: &str = include_str!("../assets/starter-bundle.json");

const STARTER_SCENARIO: &str = r#"# A starter recordchaos scenario. base is a synthetic bundle written by
# `recordchaos init`; point it at your own synthetic bundle when ready.
fhir_version = "4.0.1"
base = "recordchaos-base.json"
seed = 1
cases = 25
max_mutations_per_case = 3

[[operator]]
id = "observation.duplicate_with_changed_issued"
select = { resource_type = "Observation" }
args = { days_later = 3 }
weight = 2

[[operator]]
id = "reference.to_urn_uuid"
select = { resource_type = "Patient" }

[[operator]]
id = "bundle.reorder_entries"

[[operator]]
id = "reference.dangling_target"
"#;

const STARTER_TARGET: &str = r#"# A starter command target. Commands are argument arrays, run without a shell.
kind = "command"
reset   = ["./my-importer", "reset"]
ingest  = ["./my-importer", "ingest"]    # bundle on stdin
observe = ["./my-importer", "observe"]   # view JSON on stdout
timeout_ms = 5000

[accept]
rejected_exit_codes = [2]

[view]
# Name = JSON Pointer into the JSON your importer prints on observe.
latest_glucose = "/latest/synthetic-glucose"
patient_count  = "/patients/count"
"#;

fn cmd_init(t: &Term, dir: &Path) -> Result<i32> {
    let files = [
        ("recordchaos-scenario.toml", STARTER_SCENARIO),
        ("recordchaos-target.toml", STARTER_TARGET),
        ("recordchaos-base.json", STARTER_BUNDLE),
    ];
    for (name, _) in &files {
        let p = dir.join(name);
        if p.exists() {
            return Err(Error::input(format!(
                "{} already exists; nothing was written",
                display_path(&p)
            )));
        }
    }
    for (name, text) in &files {
        let p = dir.join(name);
        write_text(&p, text)?;
        t.always(&format!("{} {}", t.muted("wrote"), display_path(&p)));
    }
    t.line("Edit both files, then run: recordchaos run recordchaos-scenario.toml --target recordchaos-target.toml");
    Ok(EXIT_CLEAN)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starter_scenario_names_the_starter_bundle() {
        let limits = Limits::default();
        let s = recordchaos_core::scenario::parse_scenario(
            STARTER_SCENARIO,
            Path::new("recordchaos-scenario.toml"),
            &limits,
        )
        .unwrap();
        assert_eq!(display_path(&s.base_path), "recordchaos-base.json");
        assert!(bundle::parse(STARTER_BUNDLE, &limits, "starter").is_ok());
    }

    #[test]
    fn zero_counts_get_no_tick_or_cross() {
        let t = Term::new(ColorChoice::Never, false, false);
        assert_eq!(outcome_part(&t, 0, "8 handled"), "✓ 8 handled");
        assert_eq!(
            outcome_part(&t, 1, "0 rejected cleanly"),
            "  0 rejected cleanly"
        );
        assert_eq!(outcome_part(&t, 2, "3 rule broken"), "✗ 3 rule broken");
        assert_eq!(outcome_part(&t, 3, "0 crashed"), "  0 crashed");
    }
}
