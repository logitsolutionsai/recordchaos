//! Scenario and target files (TOML).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::Limits;
use crate::error::{Error, Result, read_text};
use crate::json::{sha256_hex, toml_to_json};
use crate::operators::{Select, find};
use crate::target::command::CommandConfig;
use crate::target::http::{HttpConfig, HttpRequestSpec};
use crate::target::{AcceptRules, ViewEntry};

const MAX_CONFIG_BYTES: usize = 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScenarioFile {
    fhir_version: Option<String>,
    base: String,
    seed: u64,
    cases: usize,
    #[serde(default = "default_max_mutations")]
    max_mutations_per_case: usize,
    as_of: Option<String>,
    #[serde(default, rename = "operator")]
    operators: Vec<OperatorFile>,
}

fn default_max_mutations() -> usize {
    3
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OperatorFile {
    id: String,
    #[serde(default)]
    select: Select,
    #[serde(default)]
    args: toml::Table,
    weight: Option<u32>,
}

/// One operator line of a scenario.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OperatorEntry {
    pub id: String,
    pub select: Select,
    pub args: Map<String, Value>,
    pub weight: u32,
}

/// A resolved scenario.
#[derive(Clone, Debug, PartialEq)]
pub struct Scenario {
    /// Path as given, with forward slashes.
    pub path: String,
    pub sha256: String,
    /// `base` as written in the file.
    pub base_as_written: String,
    /// `base` resolved against the scenario's folder.
    pub base_path: PathBuf,
    pub seed: u64,
    pub cases: usize,
    pub max_mutations_per_case: usize,
    pub as_of: Option<String>,
    pub operators: Vec<OperatorEntry>,
}

/// Display form of a path: forward slashes, so outputs match on every OS.
///
/// `.` and `name/..` segments are folded away lexically, so
/// `scenarios/../fixtures/a.json` shows as `fixtures/a.json`.
pub fn display_path(p: &Path) -> String {
    let text = p.display().to_string().replace('\\', "/");
    let absolute = text.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    for seg in text.split('/') {
        match seg {
            "" | "." => {}
            ".." if out.last().is_some_and(|l| *l != ".." && !l.ends_with(':')) => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    let joined = out.join("/");
    if absolute {
        format!("/{joined}")
    } else if joined.is_empty() {
        String::from(".")
    } else {
        joined
    }
}

/// A path as a shell argument for a command the user may copy: quoted when
/// it has spaces or shell characters, so `recordchaos replay` lines work
/// when pasted into bash, zsh or PowerShell.
pub fn shell_arg(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-:+@,=".contains(c));
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

fn resolve_against(file: &Path, rel: &str) -> PathBuf {
    let p = Path::new(rel);
    if p.is_absolute() {
        return p.to_path_buf();
    }
    match file.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(p),
        _ => p.to_path_buf(),
    }
}

/// Parse scenario text. `path` is used to resolve `base` and in messages.
pub fn parse_scenario(text: &str, path: &Path, limits: &Limits) -> Result<Scenario> {
    let file: ScenarioFile = toml::from_str(text)
        .map_err(|e| crate::error::toml_error(&display_path(path), text, &e))?;
    if let Some(v) = &file.fhir_version {
        if v != crate::FHIR_VERSION {
            return Err(Error::input(format!(
                "{}: fhir_version {v} is not supported; v1 supports {} (R4) only",
                display_path(path),
                crate::FHIR_VERSION
            )));
        }
    }
    if file.cases == 0 || file.cases > limits.max_cases {
        return Err(Error::input(format!(
            "{}: cases must be between 1 and {}",
            display_path(path),
            limits.max_cases
        )));
    }
    if file.max_mutations_per_case == 0
        || file.max_mutations_per_case > limits.max_mutations_per_case
    {
        return Err(Error::input(format!(
            "{}: max_mutations_per_case must be between 1 and {}",
            display_path(path),
            limits.max_mutations_per_case
        )));
    }
    if let Some(a) = &file.as_of {
        if crate::time::parse(a).is_none_or(|t| t.has_time || t.date_parts != 3) {
            return Err(Error::input(format!(
                "{}: as_of must be a date like 2026-01-31",
                display_path(path)
            )));
        }
    }
    if file.operators.is_empty() {
        return Err(Error::input(format!(
            "{}: list at least one [[operator]]",
            display_path(path)
        )));
    }
    let mut operators = Vec::new();
    for op in file.operators {
        if find(&op.id).is_none() {
            return Err(Error::input(format!(
                "{}: unknown operator {:?}. Run `recordchaos operators list` to see all 22.",
                display_path(path),
                op.id
            )));
        }
        let args = match toml_to_json(&toml::Value::Table(op.args)) {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        operators.push(OperatorEntry {
            id: op.id,
            select: op.select,
            args,
            weight: op.weight.unwrap_or(1),
        });
    }
    if operators.iter().all(|o| o.weight == 0) {
        return Err(Error::input(format!(
            "{}: every operator has weight 0",
            display_path(path)
        )));
    }
    Ok(Scenario {
        path: display_path(path),
        sha256: sha256_hex(text.as_bytes()),
        base_path: resolve_against(path, &file.base),
        base_as_written: file.base,
        seed: file.seed,
        cases: file.cases,
        max_mutations_per_case: file.max_mutations_per_case,
        as_of: file.as_of,
        operators,
    })
}

/// Load a scenario file.
pub fn load_scenario(path: &Path, limits: &Limits) -> Result<Scenario> {
    let text = read_text(path, MAX_CONFIG_BYTES)?;
    parse_scenario(&text, path, limits)
}

// ---------------------------------------------------------------------------
// Targets

/// A comparison a user can declare in the target file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind {
    Equals,
    NotEquals,
    Exists,
    Absent,
    CountEquals,
    SameAsBase,
}

/// Which cases a user oracle applies to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppliesTo {
    #[default]
    Valid,
    Invalid,
    All,
}

/// A user-declared oracle. No expression language and no code execution.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UserOracle {
    pub id: String,
    pub view: String,
    pub check: CheckKind,
    pub value: Option<Value>,
    pub applies_to: AppliesTo,
    pub sentence: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UserOracleFile {
    id: String,
    view: String,
    check: CheckKind,
    value: Option<toml::Value>,
    #[serde(default)]
    applies_to: AppliesTo,
    sentence: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AcceptFile {
    #[serde(default)]
    rejected_exit_codes: Vec<i32>,
    flag_view: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetFile {
    kind: String,
    reset: Option<toml::Value>,
    ingest: Option<toml::Value>,
    observe: Option<Vec<String>>,
    timeout_ms: Option<u64>,
    cwd: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    base_url: Option<String>,
    #[serde(default)]
    allow_hosts: Vec<String>,
    #[serde(default)]
    accept: AcceptFile,
    #[serde(default)]
    view: BTreeMap<String, toml::Value>,
    #[serde(default, rename = "oracle")]
    oracles: Vec<UserOracleFile>,
}

/// A resolved target configuration.
#[derive(Clone, Debug, PartialEq)]
pub enum TargetKind {
    Command(CommandConfig),
    Http(HttpConfig),
}

/// A loaded target file.
#[derive(Clone, Debug, PartialEq)]
pub struct TargetConfig {
    pub path: String,
    pub sha256: String,
    pub kind: TargetKind,
    pub allow_hosts: Vec<String>,
    pub oracles: Vec<UserOracle>,
}

impl TargetConfig {
    pub fn view_spec(&self) -> &BTreeMap<String, ViewEntry> {
        match &self.kind {
            TargetKind::Command(c) => &c.view,
            TargetKind::Http(h) => &h.view,
        }
    }
}

fn string_array(v: &toml::Value, what: &str, path: &str) -> Result<Vec<String>> {
    let arr = v.as_array().ok_or_else(|| {
        Error::input(format!(
            "{path}: {what} must be an array of strings, run without a shell"
        ))
    })?;
    let mut out = Vec::new();
    for item in arr {
        out.push(
            item.as_str()
                .ok_or_else(|| Error::input(format!("{path}: {what} must be an array of strings")))?
                .to_string(),
        );
    }
    if out.is_empty() {
        return Err(Error::input(format!("{path}: {what} is empty")));
    }
    Ok(out)
}

fn request_spec(
    v: &toml::Value,
    what: &str,
    file: &Path,
    default_method: &str,
) -> Result<HttpRequestSpec> {
    let path_s = display_path(file);
    let t = v.as_table().ok_or_else(|| {
        Error::input(format!(
            "{path_s}: {what} must be a table like {{ method = \"POST\", path = \"/\" }}"
        ))
    })?;
    for key in t.keys() {
        if !["method", "path", "content_type", "body", "as_transaction"].contains(&key.as_str()) {
            return Err(Error::input(format!(
                "{path_s}: unknown key {key:?} in {what}"
            )));
        }
    }
    let get = |k: &str| t.get(k).and_then(toml::Value::as_str).map(str::to_string);
    let body = match get("body") {
        Some(rel) => {
            let p = resolve_against(file, &rel);
            Some(read_text(&p, MAX_CONFIG_BYTES)?)
        }
        None => None,
    };
    Ok(HttpRequestSpec {
        method: get("method")
            .unwrap_or_else(|| default_method.to_string())
            .to_ascii_uppercase(),
        path: get("path").unwrap_or_else(|| String::from("/")),
        content_type: get("content_type").unwrap_or_else(|| String::from("application/fhir+json")),
        body,
        as_transaction: t
            .get("as_transaction")
            .and_then(toml::Value::as_bool)
            .unwrap_or(false),
    })
}

/// Parse target text.
pub fn parse_target(text: &str, path: &Path, limits: &Limits) -> Result<TargetConfig> {
    let path_s = display_path(path);
    let file: TargetFile =
        toml::from_str(text).map_err(|e| crate::error::toml_error(&path_s, text, &e))?;
    let mut oracles = Vec::new();
    for o in file.oracles {
        oracles.push(UserOracle {
            id: o.id,
            view: o.view,
            check: o.check,
            value: o.value.as_ref().map(toml_to_json),
            applies_to: o.applies_to,
            sentence: o.sentence,
        });
    }
    let accept = AcceptRules {
        rejected_exit_codes: file.accept.rejected_exit_codes,
        flag_view: file.accept.flag_view,
    };
    let timeout_ms = file.timeout_ms.unwrap_or(5_000);
    let kind = match file.kind.as_str() {
        "command" => {
            let ingest = string_array(
                file.ingest
                    .as_ref()
                    .ok_or_else(|| Error::input(format!("{path_s}: ingest is required")))?,
                "ingest",
                &path_s,
            )?;
            let observe = file
                .observe
                .ok_or_else(|| Error::input(format!("{path_s}: observe is required")))?;
            if observe.is_empty() {
                return Err(Error::input(format!("{path_s}: observe is empty")));
            }
            let reset = match &file.reset {
                Some(v) => Some(string_array(v, "reset", &path_s)?),
                None => None,
            };
            let mut view = BTreeMap::new();
            for (name, v) in &file.view {
                let pointer = v.as_str().ok_or_else(|| {
                    Error::input(format!(
                        "{path_s}: view.{name} must be a JSON Pointer string"
                    ))
                })?;
                if !pointer.is_empty() && !pointer.starts_with('/') {
                    return Err(Error::input(format!(
                        "{path_s}: view.{name} must start with /"
                    )));
                }
                view.insert(
                    name.clone(),
                    ViewEntry {
                        pointer: pointer.to_string(),
                        get: None,
                    },
                );
            }
            TargetKind::Command(CommandConfig {
                reset,
                ingest,
                observe,
                timeout_ms,
                cwd: file.cwd.as_deref().map(|c| resolve_against(path, c)),
                env: file.env,
                accept,
                view,
                max_view_bytes: limits.max_view_bytes,
            })
        }
        "http" => {
            let base_url = file
                .base_url
                .ok_or_else(|| Error::input(format!("{path_s}: base_url is required")))?;
            if crate::target::http::parse_url(&base_url).is_none() {
                return Err(Error::input(format!(
                    "{path_s}: base_url {base_url:?} is not an http or https URL"
                )));
            }
            let ingest = match &file.ingest {
                Some(v) => request_spec(v, "ingest", path, "POST")?,
                None => return Err(Error::input(format!("{path_s}: ingest is required"))),
            };
            let reset = match &file.reset {
                Some(v) => Some(request_spec(v, "reset", path, "POST")?),
                None => None,
            };
            let mut view = BTreeMap::new();
            for (name, v) in &file.view {
                let t = v.as_table().ok_or_else(|| {
                    Error::input(format!(
                        "{path_s}: view.{name} must be a table with get and pointer"
                    ))
                })?;
                let get = t
                    .get("get")
                    .and_then(toml::Value::as_str)
                    .map(str::to_string);
                let pointer = t
                    .get("pointer")
                    .and_then(toml::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if get.is_none() {
                    return Err(Error::input(format!(
                        "{path_s}: view.{name}.get is required"
                    )));
                }
                view.insert(name.clone(), ViewEntry { pointer, get });
            }
            TargetKind::Http(HttpConfig {
                base_url,
                ingest,
                reset,
                view,
                timeout_ms,
                accept,
                max_view_bytes: limits.max_view_bytes,
            })
        }
        other => {
            return Err(Error::input(format!(
                "{path_s}: kind must be \"command\" or \"http\", not {other:?}"
            )));
        }
    };
    Ok(TargetConfig {
        path: path_s,
        sha256: sha256_hex(text.as_bytes()),
        kind,
        allow_hosts: file.allow_hosts,
        oracles,
    })
}

/// Load a target file.
pub fn load_target(path: &Path, limits: &Limits) -> Result<TargetConfig> {
    let text = read_text(path, MAX_CONFIG_BYTES)?;
    parse_target(&text, path, limits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_paths_fold_dot_segments() {
        assert_eq!(
            display_path(Path::new("scenarios/../fixtures/bundles/patient-a.json")),
            "fixtures/bundles/patient-a.json"
        );
        assert_eq!(display_path(Path::new("./results")), "results");
        assert_eq!(display_path(Path::new("../x/./y")), "../x/y");
        assert_eq!(display_path(Path::new(r"C:\a\..\b")), "C:/b");
        assert_eq!(display_path(Path::new("C:/..")), "C:/..");
    }

    #[test]
    fn shell_args_quote_spaces() {
        assert_eq!(shell_arg("results/cases/c-0001"), "results/cases/c-0001");
        assert_eq!(
            shell_arg("C:/Users/TECH ZONE/out/cases/c-0001"),
            "'C:/Users/TECH ZONE/out/cases/c-0001'"
        );
        assert_eq!(shell_arg("it's"), r"'it'\''s'");
        assert_eq!(shell_arg(""), "''");
    }

    const SCENARIO: &str = r#"
fhir_version = "4.0.1"
base = "../fixtures/bundles/patient-a.json"
seed = 49302
cases = 25
max_mutations_per_case = 3

[[operator]]
id = "observation.duplicate_with_changed_issued"
select = { resource_type = "Observation", code = "synthetic-glucose" }
args = { days_later = 3 }
weight = 3

[[operator]]
id = "bundle.reorder_entries"
"#;

    #[test]
    fn parses_the_spec_scenario() {
        let s = parse_scenario(
            SCENARIO,
            Path::new("scenarios/late-correction.toml"),
            &Limits::default(),
        )
        .unwrap();
        assert_eq!(s.seed, 49302);
        assert_eq!(s.operators.len(), 2);
        assert_eq!(s.operators[0].weight, 3);
        assert_eq!(
            s.operators[0]
                .args
                .get("days_later")
                .and_then(Value::as_i64),
            Some(3)
        );
        assert_eq!(
            display_path(&s.base_path),
            "fixtures/bundles/patient-a.json"
        );
    }

    #[test]
    fn rejects_unknown_operators_and_versions() {
        let bad = SCENARIO.replace("bundle.reorder_entries", "bundle.nope");
        assert!(parse_scenario(&bad, Path::new("s.toml"), &Limits::default()).is_err());
        let r5 = SCENARIO.replace("4.0.1", "5.0.0");
        assert!(parse_scenario(&r5, Path::new("s.toml"), &Limits::default()).is_err());
    }

    #[test]
    fn parses_the_spec_command_target() {
        let text = r#"
kind = "command"
reset   = ["./target/debug/examples/buggy_importer", "reset"]
ingest  = ["./target/debug/examples/buggy_importer", "ingest"]
observe = ["./target/debug/examples/buggy_importer", "observe"]
timeout_ms = 5000

[accept]
rejected_exit_codes = [2]

[view]
latest_glucose = "/latest/synthetic-glucose"
patient_count  = "/patients/count"
"#;
        let t = parse_target(
            text,
            Path::new("examples/targets/buggy-importer.toml"),
            &Limits::default(),
        )
        .unwrap();
        match t.kind {
            TargetKind::Command(c) => {
                assert_eq!(c.accept.rejected_exit_codes, vec![2]);
                assert_eq!(c.view.len(), 2);
            }
            TargetKind::Http(_) => panic!("expected a command target"),
        }
    }

    #[test]
    fn parses_an_http_target() {
        let text = r#"
kind = "http"
base_url = "http://127.0.0.1:8080/fhir"
ingest = { method = "POST", path = "/", content_type = "application/fhir+json", as_transaction = true }
[view.latest_glucose]
get = "/Observation?patient=pat-a&code=https://example.org/recordchaos/codes|synthetic-glucose&_sort=-date&_count=1"
pointer = "/entry/0/resource"
"#;
        let t = parse_target(text, Path::new("t.toml"), &Limits::default()).unwrap();
        match t.kind {
            TargetKind::Http(h) => {
                assert!(h.ingest.as_transaction);
                assert!(
                    h.view["latest_glucose"]
                        .locator()
                        .contains("synthetic-glucose")
                );
            }
            TargetKind::Command(_) => panic!("expected an http target"),
        }
    }

    #[test]
    fn commands_must_be_arrays() {
        let text = "kind = \"command\"\ningest = \"sh -c importer\"\nobserve = [\"x\"]\n";
        assert!(parse_target(text, Path::new("t.toml"), &Limits::default()).is_err());
    }
}
