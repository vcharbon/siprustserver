//! The **run's clock**, as `timing.json` in the run bundle: when the run
//! started, when it settled, and the ceiling it had.
//!
//! Named apart from [`crate::document::Timing`], which is the DOCUMENT's expect
//! and settle budgets: one states what a run is allowed, the other what one run
//! took.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// When the run started, when it settled, and the ceiling it had.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct RunTiming {
    /// Milliseconds on the run's own clock at the first flow step.
    pub started_at_ms: u64,
    /// Milliseconds at which the settle phase completed. Absent where it never
    /// did — which is itself a failure the verdict states.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settled_at_ms: Option<u64>,
    /// The settle budget the document declared for this run.
    pub settle_budget_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_that_never_settled_does_not_carry_the_field_at_all() {
        let unsettled =
            RunTiming { started_at_ms: 0, settled_at_ms: None, settle_budget_ms: 32_000 };
        let text = serde_json::to_string(&unsettled).unwrap();
        assert!(!text.contains("settled_at_ms"), "{text}");
        assert_eq!(serde_json::from_str::<RunTiming>(&text).unwrap(), unsettled);

        let settled = RunTiming { settled_at_ms: Some(1420), ..unsettled };
        let text = serde_json::to_string(&settled).unwrap();
        assert_eq!(serde_json::from_str::<RunTiming>(&text).unwrap(), settled);
    }

    #[test]
    fn an_unknown_timing_field_is_refused_rather_than_ignored() {
        let text = r#"{"started_at_ms":0,"settle_budget_ms":32000,"settled_at_us":5}"#;
        assert!(serde_json::from_str::<RunTiming>(text).is_err());
    }
}
