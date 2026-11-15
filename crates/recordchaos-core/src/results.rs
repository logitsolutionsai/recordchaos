//! results.json types. The hand-written JSON Schema lives in
//! docs/results-schema.json. Nothing here holds a timestamp or a timing, so
//! the same seed and inputs give a byte-identical file; timings live in
//! run.toml only.

use serde::{Deserialize, Serialize};

use crate::diff::DiffLine;
use crate::generate::MutationRecord;
use crate::operators::Intent;
use crate::oracle::OracleResult;
use crate::target::StepOutcome;
use crate::validate::Validation;

/// How a case ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Every applicable oracle passed and the target accepted the case.
    Handled,
    /// The target rejected the case and that was allowed.
    RejectedCleanly,
    /// The target ran, but a rule failed.
    RuleBroken,
    /// 5xx, timeout, or an exit code that is not 0 or a rejected code.
    Crashed,
    /// No operator found anything to change; the case did not run.
    NotApplicable,
}

impl Outcome {
    pub fn label(self) -> &'static str {
        match self {
            Outcome::Handled => "Handled",
            Outcome::RejectedCleanly => "Rejected cleanly",
            Outcome::RuleBroken => "Rule broken",
            Outcome::Crashed => "Crashed",
            Outcome::NotApplicable => "Not checked",
        }
    }

    pub fn is_failure(self) -> bool {
        matches!(self, Outcome::RuleBroken | Outcome::Crashed)
    }
}

/// A file the run read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputRef {
    pub path: String,
    pub sha256: String,
}

/// The target as recorded in results.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetInfo {
    pub path: String,
    pub sha256: String,
    pub kind: String,
    pub describe: String,
}

/// Which layers ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayersRun {
    pub schema: bool,
    pub references: bool,
    pub strict: bool,
}

/// Counts for the verdict line and the README's measured results.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub cases: usize,
    pub ran: usize,
    pub handled: usize,
    pub rejected_cleanly: usize,
    pub rule_broken: usize,
    pub crashed: usize,
    pub not_applicable: usize,
    pub failed: usize,
    pub expected_invalid: usize,
    pub unexpected_validity: usize,
    pub unexpected_invalidity: usize,
    pub valid_edge_cases: usize,
    pub valid_edge_schema_valid: usize,
    pub findings: usize,
    pub shrink_replays: usize,
}

/// One case in results.json.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaseResult {
    pub id: String,
    pub seed: u64,
    pub mutations: Vec<MutationRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_applicable: Vec<String>,
    pub intent: Intent,
    pub validation: Validation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validity_note: Option<String>,
    pub outcome: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<StepOutcome>,
    #[serde(default)]
    pub oracles: Vec<OracleResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// Folder under the results folder, e.g. `cases/c-0017`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
}

/// Failures with the same final fingerprint, grouped into one cause.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub id: String,
    pub oracle: String,
    /// `crashed` or `rule_broken`.
    pub kind: String,
    pub fingerprint: String,
    pub sentence: String,
    pub cases: Vec<String>,
    /// The case whose shrunk bundle the finding shows.
    pub representative: String,
    pub mutations_before: usize,
    pub mutations_after: usize,
    pub entries_removed: usize,
    pub shrink_replays: usize,
    #[serde(default)]
    pub flaky: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reproduced: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub got: Option<String>,
    /// What the check read, e.g. "Built in. Reads /latest/synthetic-glucose".
    pub check: String,
    pub mutations: Vec<MutationRecord>,
    pub validation: Validation,
    pub diff_title: String,
    pub diff: Vec<DiffLine>,
    pub replay: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_line: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// One item of "What this run did not check".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    pub title: String,
    pub text: String,
}

/// The whole results.json.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunResults {
    pub format_version: u32,
    pub tool: String,
    pub tool_version: String,
    pub rng: String,
    pub seed: u64,
    pub fhir_version: String,
    pub scenario: InputRef,
    pub base: InputRef,
    pub target: TargetInfo,
    pub as_of: String,
    pub layers: LayersRun,
    pub operators: Vec<String>,
    pub registry_size: usize,
    pub out_dir: String,
    pub summary: Summary,
    pub cases: Vec<CaseResult>,
    pub findings: Vec<Finding>,
    pub not_checked: Vec<Note>,
}

impl RunResults {
    /// The verdict line, e.g. "4 of 25 cases broke the importer".
    pub fn verdict(&self) -> String {
        let s = &self.summary;
        if s.failed == 0 {
            if s.ran == 0 {
                String::from("No case ran")
            } else {
                format!("None of {} cases broke the importer", s.ran)
            }
        } else {
            format!("{} of {} cases broke the importer", s.failed, s.ran)
        }
    }

    pub fn exit_code(&self) -> i32 {
        if self.summary.failed > 0 {
            crate::error::EXIT_FAILURES
        } else {
            crate::error::EXIT_CLEAN
        }
    }

    pub fn finding(&self, id: &str) -> Option<&Finding> {
        self.findings.iter().find(|f| f.id == id)
    }
}
