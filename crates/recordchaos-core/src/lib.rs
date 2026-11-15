//! recordchaos core engine.
//!
//! Loads a clean synthetic HL7 FHIR R4 bundle, makes broken or awkward copies
//! with 22 deterministic mutation operators, validates each copy on up to three
//! layers, runs it against an importer (a command or an HTTP target), checks the
//! result with oracles and shrinks every failure to the smallest bundle that
//! still fails.
//!
//! Bundles are kept as `serde_json::Value` so that invalid FHIR survives a round
//! trip. Nothing here calls a language model or a network host other than the
//! declared target.

pub mod bundle;
pub mod diff;
pub mod error;
pub mod generate;
pub mod json;
pub mod operators;
pub mod oracle;
pub mod refs;
pub mod results;
pub mod rng;
pub mod run;
pub mod scenario;
pub mod shrink;
pub mod synthetic;
pub mod target;
pub mod time;
pub mod validate;

pub use error::{Error, Result};

/// Tool name used in every output file.
pub const TOOL_NAME: &str = "recordchaos";
/// Tool version, from Cargo.
pub const TOOL_VERSION: &str = env!("CARGO_PKG_VERSION");
/// The random number generator, written into every results file.
pub const RNG_NAME: &str = "ChaCha8Rng (rand_chacha 0.9)";
/// The only FHIR version supported in v1.
pub const FHIR_VERSION: &str = "4.0.1";
/// Version of the results.json format.
pub const RESULTS_FORMAT_VERSION: u32 = 1;

/// Hard limits from the spec (section 15). All can be configured lower.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_bundle_bytes: usize,
    pub max_resources: usize,
    pub max_cases: usize,
    pub max_mutations_per_case: usize,
    pub max_view_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_bundle_bytes: 10 * 1024 * 1024,
            max_resources: 5_000,
            max_cases: 1_000,
            max_mutations_per_case: 25,
            max_view_bytes: 1024 * 1024,
        }
    }
}
