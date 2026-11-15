//! In-process Python targets. The trait lives here; the implementation is in
//! the Python bindings crate (`python/`), where the pytest plugin drives each
//! case and passes the observed view back as JSON.

use serde_json::Value;

use super::{StepOutcome, View};

/// A target that runs inside the calling process. The pytest plugin
/// implements the same contract in Python: load each step, then report the
/// view with `case.check(view=...)`.
pub trait InProcessTarget {
    /// Load the steps of one case in order and return the observed view.
    fn run_case(&mut self, steps: &[Value]) -> (StepOutcome, View);
}
