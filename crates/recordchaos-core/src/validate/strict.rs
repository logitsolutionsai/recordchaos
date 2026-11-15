//! Strict layer: the HL7 Java validator (`validator_cli.jar`, tested with
//! 7.0.6) as a subprocess. Optional: it needs Java 17 and the jar, which the
//! user downloads (about 205 MB). We never bundle it.
//!
//! Command: `java -jar validator_cli.jar <folder> -version 4.0.1 -tx n/a -allow-example-urls true -output <result.json>`.
//! `-version` is required because the default is R5; `-tx n/a` turns off the
//! terminology server.
//!
//! One validator run checks a whole folder of bundles. Loading the FHIR
//! packages takes most of a run (about 30 s on the test machine), and each
//! bundle after that takes well under a second, so a run's cases go to one
//! JVM, not one JVM each. The validator writes a Bundle with one
//! OperationOutcome per file, named by its `operationoutcome-file`
//! extension, so every case keeps its own result. Validator 7.0.6 also has a
//! `server <port>` mode; a run would still pay the package load once, so it
//! would gain little and add a port and a process to manage.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;

use super::{Issue, LayerResult, fhirpath_to_pointer};
use crate::error::{Error, Result};
use crate::target::command::run_process;

/// Environment variable that names the jar.
pub const JAR_ENV: &str = "RECORDCHAOS_VALIDATOR_JAR";
/// One-line fix shown when the strict layer is unavailable.
pub const FIX: &str = "Install Java 17 or later (on PATH or in JAVA_HOME) and download validator_cli.jar 7.0.6 from https://github.com/hapifhir/org.hl7.fhir.core/releases, then pass --validator-jar <path> or set RECORDCHAOS_VALIDATOR_JAR.";
/// Bundles per validator run. Bounds the validator's memory and the size of
/// one temporary folder; larger sets take several runs.
pub const BATCH: usize = 500;
/// Extra timeout per bundle in a run, on top of `StrictValidator::timeout`.
pub const PER_BUNDLE_TIMEOUT: Duration = Duration::from_secs(5);
/// Extension on each OperationOutcome that names the file it is about.
const FILE_EXTENSION: &str = "http://hl7.org/fhir/StructureDefinition/operationoutcome-file";

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A located Java and validator jar.
#[derive(Clone, Debug)]
pub struct StrictValidator {
    pub java: String,
    pub jar: PathBuf,
    /// Text for run.toml, e.g. the jar file name and size.
    pub version: String,
    /// Timeout for one validator run, before the per-bundle allowance.
    pub timeout: Duration,
}

/// The Java program: `$JAVA_HOME/bin/java` when JAVA_HOME names a folder
/// that has one, otherwise `java` from PATH.
fn java_program() -> String {
    if let Some(home) = std::env::var_os("JAVA_HOME") {
        let bin = PathBuf::from(home).join("bin");
        for name in ["java.exe", "java"] {
            let p = bin.join(name);
            if p.is_file() {
                return p.display().to_string();
            }
        }
    }
    String::from("java")
}

/// The file name a bundle gets in the validator's input folder.
pub fn input_name(index: usize) -> String {
    format!("b{index:06}.json")
}

/// The input index named by an `operationoutcome-file` value, which the
/// validator writes as a full path in folder mode.
fn input_index(file: &str) -> Option<usize> {
    let name = file.rsplit(['/', '\\']).next()?;
    name.strip_prefix('b')?.strip_suffix(".json")?.parse().ok()
}

impl StrictValidator {
    /// Find Java and the jar. `Err` is an unavailable error (exit 3).
    pub fn locate(jar: Option<&Path>) -> Result<Self> {
        let jar = match jar {
            Some(p) => p.to_path_buf(),
            None => match std::env::var_os(JAR_ENV) {
                Some(v) => PathBuf::from(v),
                None => PathBuf::from("validator_cli.jar"),
            },
        };
        let meta = std::fs::metadata(&jar).map_err(|_| {
            Error::unavailable(format!(
                "HL7 validator jar not found at {}. {FIX}",
                jar.display()
            ))
        })?;
        let java = java_program();
        let probe = run_process(
            &[java.clone(), String::from("-version")],
            None,
            Duration::from_secs(30),
            None,
            &BTreeMap::new(),
            64 * 1024,
        )
        .map_err(|e| Error::unavailable(format!("java not found ({e}). {FIX}")))?;
        if probe.code != Some(0) {
            return Err(Error::unavailable(format!("java -version failed. {FIX}")));
        }
        let java_line = crate::target::first_line(&probe.stderr);
        let name = jar
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        Ok(Self {
            java,
            version: format!("{name} ({} bytes), {java_line}", meta.len()),
            jar,
            timeout: Duration::from_secs(300),
        })
    }

    /// Validate one bundle. A validator that cannot run gives not_checked,
    /// never valid.
    pub fn validate(&self, bundle: &Value) -> LayerResult {
        self.validate_many(&[bundle])
            .pop()
            .unwrap_or_else(|| LayerResult::not_checked("HL7 validator returned no result"))
    }

    /// Validate many bundles with one validator run per [`BATCH`] bundles.
    /// The results follow the input order. A bundle the validator gave no
    /// result for is not_checked, never valid.
    pub fn validate_many(&self, bundles: &[&Value]) -> Vec<LayerResult> {
        let mut out = Vec::with_capacity(bundles.len());
        for chunk in bundles.chunks(BATCH) {
            out.extend(self.run_batch(chunk));
        }
        out
    }

    fn run_batch(&self, bundles: &[&Value]) -> Vec<LayerResult> {
        let every = |r: LayerResult| vec![r; bundles.len()];
        if bundles.is_empty() {
            return Vec::new();
        }
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("recordchaos-strict-{}-{n}", std::process::id()));
        let input = dir.join("in");
        if let Err(e) = std::fs::create_dir_all(&input) {
            return every(LayerResult::not_checked(format!(
                "HL7 validator not run: cannot create {}: {e}",
                input.display()
            )));
        }
        let output = dir.join("result.json");
        let mut written = Vec::with_capacity(bundles.len());
        let mut failed = None;
        for (i, bundle) in bundles.iter().enumerate() {
            let path = input.join(input_name(i));
            let wrote = crate::json::canonical(bundle)
                .map_err(|e| e.to_string())
                .and_then(|text| {
                    std::fs::write(&path, text)
                        .map_err(|e| format!("cannot write {}: {e}", path.display()))
                });
            written.push(path);
            if let Err(e) = wrote {
                failed = Some(e);
                break;
            }
        }
        let layers = if let Some(e) = failed {
            every(LayerResult::not_checked(format!(
                "HL7 validator not run: {e}"
            )))
        } else {
            let argv = strict_argv(&self.java, &self.jar, &input, &output);
            let count = u32::try_from(bundles.len()).unwrap_or(u32::MAX);
            let timeout = self.timeout + PER_BUNDLE_TIMEOUT.saturating_mul(count);
            match run_process(&argv, None, timeout, None, &BTreeMap::new(), 1024 * 1024) {
                Err(e) => every(LayerResult::not_checked(format!(
                    "HL7 validator not run: {e}"
                ))),
                Ok(out) if out.timed_out => every(LayerResult::not_checked(format!(
                    "HL7 validator timed out after {} s. Its first run downloads FHIR packages into ~/.fhir/packages; run it once by hand to fill that cache",
                    timeout.as_secs()
                ))),
                Ok(out) => match std::fs::read_to_string(&output) {
                    Ok(json_text) => parse_outcomes(&json_text, bundles.len()),
                    Err(_) => every(LayerResult::not_checked(format!(
                        "HL7 validator wrote no result (exit {:?}): {}",
                        out.code,
                        crate::target::first_line(&out.stdout)
                    ))),
                },
            }
        };
        // Our own temporary files only.
        for path in &written {
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_file(&output);
        let _ = std::fs::remove_dir(&input);
        let _ = std::fs::remove_dir(&dir);
        layers
    }
}

/// The validator command line. `input` is a folder of bundles.
///
/// - `-Dfile.encoding=UTF-8`: on Windows the JVM default is windows-1252,
///   which misreads non-ASCII narrative (operator 22).
/// - `-version 4.0.1`: the default is R5.
/// - `-tx n/a`: no terminology server.
/// - `-allow-example-urls true`: the fixtures use `https://example.org/...`
///   code and identifier systems (SPEC section 14), which the validator
///   otherwise reports as errors on every resource, so every bundle,
///   including the clean base, would be "invalid" at this layer.
pub fn strict_argv(java: &str, jar: &Path, input: &Path, output: &Path) -> Vec<String> {
    vec![
        java.to_string(),
        String::from("-Dfile.encoding=UTF-8"),
        String::from("-jar"),
        jar.display().to_string(),
        input.display().to_string(),
        String::from("-version"),
        String::from("4.0.1"),
        String::from("-tx"),
        String::from("n/a"),
        String::from("-allow-example-urls"),
        String::from("true"),
        String::from("-output"),
        output.display().to_string(),
    ]
}

/// Map the validator's output for `count` input files to one result per
/// file, in input order. The output is a Bundle of OperationOutcomes, each
/// naming its file, or a single OperationOutcome when there was one input.
pub fn parse_outcomes(text: &str, count: usize) -> Vec<LayerResult> {
    let every = |r: LayerResult| vec![r; count];
    let doc: Value = match serde_json::from_str(text) {
        Ok(d) => d,
        Err(e) => {
            return every(LayerResult::not_checked(format!(
                "HL7 validator output is not JSON: {e}"
            )));
        }
    };
    match doc.get("resourceType").and_then(Value::as_str) {
        Some("OperationOutcome") if count == 1 => vec![outcome_layer(&doc)],
        Some("Bundle") => {
            let mut found: Vec<Option<LayerResult>> = vec![None; count];
            let outcomes: Vec<&Value> = doc
                .get("entry")
                .and_then(Value::as_array)
                .map(|es| es.iter().filter_map(|e| e.get("resource")).collect())
                .unwrap_or_default();
            for oo in outcomes {
                let file = oo
                    .get("extension")
                    .and_then(Value::as_array)
                    .and_then(|xs| {
                        xs.iter()
                            .find(|x| x.get("url").and_then(Value::as_str) == Some(FILE_EXTENSION))
                    })
                    .and_then(|x| x.get("valueString"))
                    .and_then(Value::as_str);
                if let Some(slot) = file.and_then(input_index).and_then(|i| found.get_mut(i)) {
                    *slot = Some(outcome_layer(oo));
                }
            }
            found
                .into_iter()
                .map(|f| {
                    f.unwrap_or_else(|| {
                        LayerResult::not_checked("HL7 validator gave no result for this bundle")
                    })
                })
                .collect()
        }
        _ => every(LayerResult::not_checked(
            "HL7 validator output is not an OperationOutcome",
        )),
    }
}

/// Map the validator's output for a single input file.
pub fn parse_outcome(text: &str) -> LayerResult {
    parse_outcomes(text, 1)
        .pop()
        .unwrap_or_else(|| LayerResult::not_checked("HL7 validator returned no result"))
}

/// One OperationOutcome as a layer result. Only `error` and `fatal` issues
/// make a bundle invalid.
fn outcome_layer(oo: &Value) -> LayerResult {
    let mut issues = Vec::new();
    let list = oo
        .get("issue")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    for issue in list {
        let severity = issue.get("severity").and_then(Value::as_str).unwrap_or("");
        if severity != "error" && severity != "fatal" {
            continue;
        }
        let location = issue
            .pointer("/expression/0")
            .or_else(|| issue.pointer("/location/0"))
            .and_then(Value::as_str)
            .unwrap_or("Bundle");
        let message = issue
            .pointer("/details/text")
            .or_else(|| issue.get("diagnostics"))
            .and_then(Value::as_str)
            .unwrap_or("error")
            .to_string();
        issues.push(Issue {
            path: fhirpath_to_pointer(location),
            message,
        });
    }
    LayerResult::from_issues(issues)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::LayerStatus;

    #[test]
    fn parses_operation_outcome() {
        let text = r#"{"resourceType": "OperationOutcome", "issue": [
            {"severity": "error", "code": "invariant", "details": {"text": "per-1: If present, start SHALL have a lower value than end"},
             "expression": ["Bundle.entry[2].resource.ofType(Encounter).period"]},
            {"severity": "information", "code": "informational", "details": {"text": "All OK"}}
        ]}"#;
        let r = parse_outcome(text);
        assert_eq!(r.status, LayerStatus::Invalid);
        assert_eq!(r.issues.len(), 1);
        assert_eq!(r.issues[0].path, "/entry/2/resource/period");
    }

    #[test]
    fn information_only_is_valid() {
        let text = r#"{"resourceType": "OperationOutcome", "issue": [{"severity": "information", "code": "informational"}]}"#;
        assert_eq!(parse_outcome(text).status, LayerStatus::Valid);
    }

    /// The shape validator 7.0.6 writes for a folder: one OperationOutcome
    /// per file, named by a full path, in any order.
    fn folder_output(files: &[(&str, bool)]) -> String {
        let entries: Vec<Value> = files
            .iter()
            .map(|(file, bad)| {
                let issue = if *bad {
                    serde_json::json!([{"severity": "error", "code": "structure",
                        "details": {"text": "Observation.status: minimum required = 1, but only found 0"},
                        "expression": ["Bundle.entry[4].resource/*Observation/obs-glucose-1*/"]}])
                } else {
                    serde_json::json!([{"severity": "warning", "code": "invariant",
                        "details": {"text": "dom-6"}}])
                };
                serde_json::json!({"resource": {"resourceType": "OperationOutcome",
                    "extension": [
                        {"url": "http://hl7.org/fhir/tools/StructureDefinition/validator-version", "valueString": "7.0.6"},
                        {"url": FILE_EXTENSION, "valueString": file}],
                    "issue": issue}})
            })
            .collect();
        serde_json::json!({"resourceType": "Bundle", "type": "collection", "entry": entries})
            .to_string()
    }

    #[test]
    fn folder_output_gives_each_case_its_own_result() {
        let text = folder_output(&[
            (
                r"C:\Users\TECH ZONE\AppData\Local\Temp\recordchaos-strict-1-0\in\b000002.json",
                false,
            ),
            (
                r"C:\Users\TECH ZONE\AppData\Local\Temp\recordchaos-strict-1-0\in\b000000.json",
                false,
            ),
            ("/tmp/recordchaos-strict-1-0/in/b000001.json", true),
        ]);
        let r = parse_outcomes(&text, 4);
        assert_eq!(r.len(), 4);
        assert_eq!(r[0].status, LayerStatus::Valid);
        assert_eq!(r[1].status, LayerStatus::Invalid);
        assert_eq!(r[1].issues[0].path, "/entry/4/resource");
        assert_eq!(r[2].status, LayerStatus::Valid);
        // No outcome for b000003: not checked, never valid.
        assert_eq!(r[3].status, LayerStatus::NotChecked);
        assert!(r[3].reason.as_deref().unwrap_or("").contains("no result"));
    }

    #[test]
    fn unreadable_output_marks_every_case_not_checked() {
        let r = parse_outcomes("not json", 3);
        assert_eq!(r.len(), 3);
        assert!(r.iter().all(|l| l.status == LayerStatus::NotChecked));
    }

    #[test]
    fn missing_java_marks_every_case_not_checked() {
        let v = StrictValidator {
            java: String::from("recordchaos-no-such-java"),
            jar: PathBuf::from("validator_cli.jar"),
            version: String::new(),
            timeout: Duration::from_secs(5),
        };
        let a = serde_json::json!({"resourceType": "Bundle", "type": "collection", "entry": []});
        let b = serde_json::json!({"resourceType": "Bundle", "type": "batch", "entry": []});
        let r = v.validate_many(&[&a, &b]);
        assert_eq!(r.len(), 2);
        for l in &r {
            assert_eq!(l.status, LayerStatus::NotChecked);
            assert!(
                l.reason
                    .as_deref()
                    .unwrap_or("")
                    .starts_with("HL7 validator not run")
            );
        }
        assert_eq!(v.validate(&a).status, LayerStatus::NotChecked);
    }

    #[test]
    fn input_names_round_trip() {
        assert_eq!(input_name(7), "b000007.json");
        assert_eq!(input_index(r"C:\x y\in\b000007.json"), Some(7));
        assert_eq!(input_index("/tmp/in/b000123.json"), Some(123));
        assert_eq!(input_index("/tmp/in/other.json"), None);
    }

    #[test]
    fn command_line_allows_example_urls_and_reads_utf8() {
        let argv = strict_argv(
            "java",
            Path::new("validator_cli.jar"),
            Path::new("in"),
            Path::new("out.json"),
        );
        let has_pair = |a: &str, b: &str| argv.windows(2).any(|w| w[0] == a && w[1] == b);
        // Without this every fixture resource fails on its example.org systems.
        assert!(has_pair("-allow-example-urls", "true"), "{argv:?}");
        assert!(has_pair("-version", "4.0.1"));
        assert!(has_pair("-tx", "n/a"));
        assert!(has_pair("-output", "out.json"));
        // JVM options go before -jar; the input folder follows the jar.
        assert_eq!(argv[1], "-Dfile.encoding=UTF-8");
        assert_eq!(argv[2], "-jar");
        assert_eq!(argv[4], "in");
    }

    #[test]
    fn missing_jar_is_unavailable() {
        let err =
            StrictValidator::locate(Some(Path::new("no-such-dir/validator_cli.jar"))).unwrap_err();
        assert_eq!(err.exit_code(), crate::error::EXIT_UNAVAILABLE);
    }
}
