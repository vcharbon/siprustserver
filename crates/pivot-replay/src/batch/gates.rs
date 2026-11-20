//! The gates the batch's end reads, each answered apart: one verification
//! line of the batch per gate the lane wires (ADR-0060 appendix §8),
//! carried on the `end` line's `gates`. A gate the lane wires none of has no
//! line; what fails a gate is its finding, a gate whose end read nothing it
//! could judge is not checkable, and the batch's `dirty` is the failing
//! gates' findings joined in read order.

use serde::Serialize;

use crate::verifications::Answer;

/// One gate of the batch's end, as the `end` line states it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Gate {
    pub name: &'static str,
    pub answer: Answer,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl Gate {
    /// The gate `name` over its `finding`: a pass where there is none, else a
    /// failure stating it.
    pub fn over(name: &'static str, finding: Option<String>) -> Gate {
        Gate {
            name,
            answer: if finding.is_some() { Answer::Fail } else { Answer::Pass },
            message: finding,
        }
    }

    /// The gate `name` its end could not read, for the reason `why`.
    pub fn unchecked(name: &'static str, why: String) -> Gate {
        Gate { name, answer: Answer::NotCheckable, message: Some(why) }
    }
}

/// What the batch's end read the SUT still holding: the failing gates'
/// findings joined in `gates` order, `None` where every gate passed.
pub fn dirty_of(gates: &[Gate]) -> Option<String> {
    let findings: Vec<&str> = gates
        .iter()
        .filter(|gate| gate.answer == Answer::Fail)
        .filter_map(|gate| gate.message.as_deref())
        .collect();
    (!findings.is_empty()).then(|| findings.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only a failing gate's finding is dirt: a passing gate and one its end
    /// could not read are not, and the findings keep the order the end read.
    #[test]
    fn the_dirt_is_the_failing_gates_findings_in_read_order() {
        let gates = [
            Gate::over("first", Some("one left".into())),
            Gate::unchecked("second", "unread".into()),
            Gate::over("third", None),
            Gate::over("fourth", Some("two left".into())),
        ];
        assert_eq!(dirty_of(&gates).as_deref(), Some("one left; two left"));
        assert_eq!(dirty_of(&gates[1..3]), None);
    }
}
