//! **Accessor resolution** (`PCAP2TEST_PIVOT_V3.md` §8.1): `${leg:…}`,
//! `${early:…}`, `${step:…}` and `${num:…}` resolved against runner state and
//! the lane's identity binding.
//!
//! An accessor is looked for in EVERY string a document carries; there is no
//! position where a `${…}` is treated as literal text. A value the run has not
//! produced is not a value: reading one is a failure that names the accessor and
//! the reason, never an empty substitution.
//!
//! Arithmetic is structural: `{ from, delta }` and nothing else is computable.

use pivot_schema::accessor::{Accessor, Computed, EarlyField, LegField, StepField};
use pivot_schema::bundle::{BindingError, IdentityBindings};

use crate::state::RunState;

/// Why a `${…}` cannot be answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// The text carries a `${…}` that is not an accessor.
    Malformed { text: String, detail: String },
    /// The run has not touched this leg.
    LegUnknown { accessor: String, leg: String },
    /// The leg exists, but has not learned this field yet.
    LegFieldUnset { accessor: String, leg: String, field: &'static str },
    /// No step of this run declares that `early` id.
    EarlyUnknown { accessor: String, early: String },
    /// Two legs declare the id, so it names two dialogs.
    EarlyAmbiguous { accessor: String, early: String },
    /// The fork exists, but nothing has put this fact on the wire yet.
    EarlyFieldUnset { accessor: String, early: String, field: &'static str },
    /// The step has not produced a message yet.
    StepNotRun { accessor: String, step: String },
    /// The step ran, but its message carried no such field.
    StepFieldUnset { accessor: String, step: String, field: String },
    /// The `alt` has not committed to a branch yet.
    BranchNotCommitted { accessor: String, alt: String },
    /// The lane's identity binding cannot answer.
    Binding { accessor: String, source: BindingError },
    /// `{ from, delta }` over a value that is not a number.
    NotNumeric { accessor: String, value: String },
    /// `{ from, delta }` whose result leaves the CSeq range.
    OutOfRange { accessor: String, value: i64 },
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::Malformed { text, detail } => write!(f, "{text:?}: {detail}"),
            ResolveError::LegUnknown { accessor, leg } => {
                write!(f, "{accessor}: the run has not touched leg {leg:?}")
            }
            ResolveError::LegFieldUnset { accessor, leg, field } => {
                write!(f, "{accessor}: leg {leg:?} has not learned its {field}")
            }
            ResolveError::EarlyUnknown { accessor, early } => {
                write!(f, "{accessor}: no step declares early dialog {early:?}")
            }
            ResolveError::EarlyAmbiguous { accessor, early } => write!(
                f,
                "{accessor}: two legs declare early dialog {early:?}, so it names two dialogs"
            ),
            ResolveError::EarlyFieldUnset { accessor, early, field } => {
                write!(f, "{accessor}: early dialog {early:?} has not learned its {field}")
            }
            ResolveError::StepNotRun { accessor, step } => {
                write!(f, "{accessor}: step {step:?} has not run")
            }
            ResolveError::StepFieldUnset { accessor, step, field } => {
                write!(f, "{accessor}: step {step:?} carried no {field}")
            }
            ResolveError::BranchNotCommitted { accessor, alt } => {
                write!(f, "{accessor}: alt {alt:?} has not committed to a branch")
            }
            ResolveError::Binding { accessor, source } => write!(f, "{accessor}: {source}"),
            ResolveError::NotNumeric { accessor, value } => {
                write!(f, "{accessor}: {value:?} is not a number, so no delta applies")
            }
            ResolveError::OutOfRange { accessor, value } => {
                write!(f, "{accessor}: {value} is outside the CSeq range")
            }
        }
    }
}

impl std::error::Error for ResolveError {}

/// Resolves accessors against one run's state and one lane's bindings.
pub struct Resolver<'a> {
    state: &'a RunState,
    bindings: &'a IdentityBindings,
}

impl<'a> Resolver<'a> {
    pub fn new(state: &'a RunState, bindings: &'a IdentityBindings) -> Self {
        Resolver { state, bindings }
    }

    /// Substitute every `${…}` in `text`. A text with no accessor comes back
    /// unchanged, and a malformed one is a failure rather than literal text.
    pub fn text(&self, text: &str) -> Result<String, ResolveError> {
        if !Accessor::present_in(text) {
            return Ok(text.to_string());
        }
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find("${") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let Some(end) = after.find('}') else {
                return Err(ResolveError::Malformed {
                    text: text.to_string(),
                    detail: "accessor is not terminated by `}`".into(),
                });
            };
            let accessor = Accessor::parse_body(&after[..end]).map_err(|detail| {
                ResolveError::Malformed { text: text.to_string(), detail }
            })?;
            out.push_str(&self.accessor(&accessor)?);
            rest = &after[end + 1..];
        }
        out.push_str(rest);
        Ok(out)
    }

    /// One accessor's value.
    pub fn accessor(&self, accessor: &Accessor) -> Result<String, ResolveError> {
        let name = accessor.to_string();
        match accessor {
            Accessor::Leg { leg, field } => self.leg_field(&name, leg, *field),
            Accessor::Early { early, field } => self.early_field(&name, early, *field),
            Accessor::Step { step, field } => self.step_field(&name, step, field),
            Accessor::Number { name: identity, form } => self
                .bindings
                .resolve(identity, form)
                .map(str::to_string)
                .map_err(|source| ResolveError::Binding { accessor: name, source }),
        }
    }

    fn leg_field(
        &self,
        name: &str,
        leg: &str,
        field: LegField,
    ) -> Result<String, ResolveError> {
        let state = self
            .state
            .leg(leg)
            .ok_or_else(|| ResolveError::LegUnknown {
                accessor: name.to_string(),
                leg: leg.to_string(),
            })?;
        let unset = |what: &'static str| ResolveError::LegFieldUnset {
            accessor: name.to_string(),
            leg: leg.to_string(),
            field: what,
        };
        match field {
            LegField::CallId => state.call_id.clone().ok_or_else(|| unset("Call-ID")),
            LegField::LocalTag => state.local_tag.clone().ok_or_else(|| unset("local tag")),
            LegField::RemoteTag => state.remote_tag.clone().ok_or_else(|| unset("remote tag")),
            LegField::RemoteTarget => {
                state.remote_target.clone().ok_or_else(|| unset("remote target"))
            }
            // A route set is ORDERED and may legitimately be empty — an empty
            // route set is a fact, not an unset field.
            LegField::RouteSet => Ok(state.route_set.join(", ")),
            LegField::CseqLocal => {
                state.cseq_local.map(|n| n.to_string()).ok_or_else(|| unset("local CSeq"))
            }
            LegField::CseqRemote => {
                state.cseq_remote.map(|n| n.to_string()).ok_or_else(|| unset("remote CSeq"))
            }
            LegField::Rseq => state.rseq.map(|n| n.to_string()).ok_or_else(|| unset("RSeq")),
        }
    }

    /// One fork's own fact. The tag is minted before the run speaks, so it
    /// always answers; the RSeq answers once a reliable provisional has ridden
    /// this fork (RFC 3262 §3).
    fn early_field(
        &self,
        name: &str,
        early: &str,
        field: EarlyField,
    ) -> Result<String, ResolveError> {
        let state = self.state.early(early).ok_or_else(|| ResolveError::EarlyUnknown {
            accessor: name.to_string(),
            early: early.to_string(),
        })?;
        if state.shared {
            return Err(ResolveError::EarlyAmbiguous {
                accessor: name.to_string(),
                early: early.to_string(),
            });
        }
        match field {
            EarlyField::Tag => Ok(state.tag.clone()),
            EarlyField::Rseq => {
                state.rseq.map(|n| n.to_string()).ok_or_else(|| ResolveError::EarlyFieldUnset {
                    accessor: name.to_string(),
                    early: early.to_string(),
                    field: "RSeq",
                })
            }
        }
    }

    fn step_field(
        &self,
        name: &str,
        step: &str,
        field: &StepField,
    ) -> Result<String, ResolveError> {
        if let StepField::Branch = field {
            return self
                .state
                .branch(step)
                .map(str::to_string)
                .ok_or_else(|| ResolveError::BranchNotCommitted {
                    accessor: name.to_string(),
                    alt: step.to_string(),
                });
        }
        let outcome = self.state.step(step).ok_or_else(|| ResolveError::StepNotRun {
            accessor: name.to_string(),
            step: step.to_string(),
        })?;
        let unset = |what: &str| ResolveError::StepFieldUnset {
            accessor: name.to_string(),
            step: step.to_string(),
            field: what.to_string(),
        };
        match field {
            StepField::Header(header) => {
                outcome.header(header).map(str::to_string).ok_or_else(|| unset(header))
            }
            StepField::Cseq => outcome.cseq.map(|n| n.to_string()).ok_or_else(|| unset("CSeq")),
            StepField::Rseq => outcome.rseq.map(|n| n.to_string()).ok_or_else(|| unset("RSeq")),
            StepField::Status => {
                outcome.status.map(|n| n.to_string()).ok_or_else(|| unset("status"))
            }
            StepField::Branch => unreachable!("handled above"),
        }
    }

    /// `{ from, delta }`: the referenced value plus a signed offset, as a CSeq.
    pub fn computed(&self, computed: &Computed) -> Result<u32, ResolveError> {
        let name = computed.from.to_string();
        let raw = self.accessor(&computed.from)?;
        let base: i64 = raw.parse().map_err(|_| ResolveError::NotNumeric {
            accessor: name.clone(),
            value: raw.clone(),
        })?;
        let sum = base + computed.delta;
        u32::try_from(sum).map_err(|_| ResolveError::OutOfRange { accessor: name, value: sum })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::StepOutcome;

    fn state() -> RunState {
        let mut state = RunState::new();
        let leg = state.leg_mut("B");
        leg.call_id = Some("call-b@host".into());
        leg.local_tag = Some("uas1-tag-3".into());
        leg.remote_tag = Some("b2bua-tag-7".into());
        leg.remote_target = Some("sip:b2bua@10.0.0.9:5080".into());
        leg.route_set = vec!["<sip:p1@10.0.0.1;lr>".into(), "<sip:p2@10.0.0.2;lr>".into()];
        leg.cseq_local = Some(4);
        leg.cseq_remote = Some(11);
        leg.rseq = Some(2);
        state.record_step(
            "s7",
            StepOutcome {
                status: Some(183),
                cseq: Some(11),
                cseq_method: Some("INVITE".into()),
                rseq: Some(2),
                method: None,
                headers: vec![
                    ("To".into(), "<sip:bob@h>;tag=b2bua-tag-7".into()),
                    ("RSeq".into(), "2".into()),
                ]
                .into(),
            },
        );
        state.commit_branch("a1", "cancelled");
        state.mint_early("f1", "B", "B-r1a2b3-early-f1");
        state.mint_early("f2", "B", "B-r1a2b3-early-f2");
        state.record_early_rseq("f1", 1);
        state.record_early_rseq("f2", 7001);
        state
    }

    fn bindings() -> IdentityBindings {
        IdentityBindings::new()
            .bind("caller", "private", "0009001")
            .bind("transferee", "e164", "+33000900006")
    }

    #[test]
    fn every_leg_field_resolves_off_runner_state() {
        let state = state();
        let bindings = bindings();
        let r = Resolver::new(&state, &bindings);
        for (accessor, expected) in [
            ("${leg:B.call-id}", "call-b@host"),
            ("${leg:B.local-tag}", "uas1-tag-3"),
            ("${leg:B.remote-tag}", "b2bua-tag-7"),
            ("${leg:B.remote-target}", "sip:b2bua@10.0.0.9:5080"),
            ("${leg:B.route-set}", "<sip:p1@10.0.0.1;lr>, <sip:p2@10.0.0.2;lr>"),
            ("${leg:B.cseq.local}", "4"),
            ("${leg:B.cseq.remote}", "11"),
            ("${leg:B.rseq}", "2"),
        ] {
            assert_eq!(r.text(accessor).unwrap(), expected, "{accessor}");
        }
    }

    /// K7's answer: a leg ringing two forks has two To-tags and two RSeq
    /// spaces, and `${leg:B.rseq}` publishes ONE of each. The early namespace
    /// reads the fork.
    #[test]
    fn each_fork_publishes_its_own_tag_and_rseq_where_the_leg_publishes_one() {
        let state = state();
        let bindings = bindings();
        let r = Resolver::new(&state, &bindings);
        for (accessor, expected) in [
            ("${early:f1.tag}", "B-r1a2b3-early-f1"),
            ("${early:f2.tag}", "B-r1a2b3-early-f2"),
            ("${early:f1.rseq}", "1"),
            ("${early:f2.rseq}", "7001"),
            // The leg's own reading is the LAST one sighted, and stays so.
            ("${leg:B.rseq}", "2"),
        ] {
            assert_eq!(r.text(accessor).unwrap(), expected, "{accessor}");
        }
        // The whole point: composing the RAck of ONE fork.
        assert_eq!(
            r.text("${early:f2.rseq} ${leg:B.cseq.remote} INVITE").unwrap(),
            "7001 11 INVITE"
        );
    }

    /// A fork the document does not declare, a fork two legs declare, and a
    /// fork nothing has rung yet are three different refusals, each by name.
    #[test]
    fn a_fork_that_answers_nothing_is_a_named_refusal_not_an_empty_string() {
        let mut state = state();
        state.mint_early("f3", "B", "B-r1a2b3-early-f3");
        state.mint_early("f3", "C", "C-r1a2b3-early-f3");
        let bindings = bindings();
        let r = Resolver::new(&state, &bindings);
        assert!(matches!(
            r.text("${early:f9.tag}").unwrap_err(),
            ResolveError::EarlyUnknown { .. }
        ));
        assert!(matches!(
            r.text("${early:f3.tag}").unwrap_err(),
            ResolveError::EarlyAmbiguous { .. }
        ));
        let mut unrung = RunState::new();
        unrung.mint_early("f1", "B", "B-r1a2b3-early-f1");
        let r = Resolver::new(&unrung, &bindings);
        assert_eq!(r.text("${early:f1.tag}").unwrap(), "B-r1a2b3-early-f1");
        assert_eq!(
            r.text("${early:f1.rseq}").unwrap_err(),
            ResolveError::EarlyFieldUnset {
                accessor: "${early:f1.rseq}".into(),
                early: "f1".into(),
                field: "RSeq",
            }
        );
    }

    #[test]
    fn every_step_field_resolves_off_what_the_step_saw() {
        let state = state();
        let bindings = bindings();
        let r = Resolver::new(&state, &bindings);
        for (accessor, expected) in [
            ("${step:s7.status}", "183"),
            ("${step:s7.cseq}", "11"),
            ("${step:s7.rseq}", "2"),
            ("${step:s7.header.To}", "<sip:bob@h>;tag=b2bua-tag-7"),
            // Header identity is case-insensitive on the wire.
            ("${step:s7.header.rseq}", "2"),
            ("${a1.branch}", "cancelled"),
        ] {
            let text = accessor.replace("${a1.", "${step:a1.");
            assert_eq!(r.text(&text).unwrap(), expected, "{accessor}");
        }
    }

    #[test]
    fn a_number_accessor_resolves_through_the_lane_binding_not_the_document() {
        let state = state();
        let bindings = bindings();
        let r = Resolver::new(&state, &bindings);
        assert_eq!(
            r.text("<sip:${num:transferee:e164}@example.invalid>").unwrap(),
            "<sip:+33000900006@example.invalid>"
        );
        let err = r.text("${num:transferee:private}").unwrap_err();
        assert!(matches!(err, ResolveError::Binding { .. }), "{err}");
    }

    #[test]
    fn several_accessors_compose_inside_one_header_value() {
        let state = state();
        let bindings = bindings();
        let r = Resolver::new(&state, &bindings);
        assert_eq!(
            r.text("<sip:x@h?Replaces=${leg:B.call-id}%3Bto-tag%3D${leg:B.remote-tag}>").unwrap(),
            "<sip:x@h?Replaces=call-b@host%3Bto-tag%3Db2bua-tag-7>"
        );
    }

    /// The ct-* need (§8.1): a header composed on ONE leg out of ANOTHER leg's
    /// dialog identity and the lane's own number allocation. Nothing scopes an
    /// accessor to the leg that emits it — a leg accessor names its leg — so a
    /// Refer-To on the transferor's leg reads the transferee's dialog.
    #[test]
    fn a_refer_to_composes_a_number_and_another_leg_s_dialog_identity() {
        let state = state();
        let bindings = bindings();
        let r = Resolver::new(&state, &bindings);
        assert_eq!(
            r.text(
                "<sip:${num:transferee:e164}@example.invalid?Replaces=${leg:B.call-id}\
                 %3Bto-tag%3D${leg:B.remote-tag}%3Bfrom-tag%3D${leg:B.local-tag}>"
            )
            .unwrap(),
            "<sip:+33000900006@example.invalid?Replaces=call-b@host\
             %3Bto-tag%3Db2bua-tag-7%3Bfrom-tag%3Duas1-tag-3>"
        );
    }

    /// §8.1 lists the leg fields, and the list is closed: a field it does not
    /// state is a malformed accessor at compile, never an empty substitution.
    #[test]
    fn a_leg_field_the_spec_does_not_state_is_malformed_not_empty() {
        let state = state();
        let bindings = bindings();
        let r = Resolver::new(&state, &bindings);
        for text in ["${leg:B.contact}", "${leg:B.cseq}", "${leg:B.branch}"] {
            assert!(
                matches!(r.text(text).unwrap_err(), ResolveError::Malformed { .. }),
                "{text} resolved to something"
            );
        }
    }

    #[test]
    fn a_value_the_run_has_not_produced_is_a_failure_not_an_empty_string() {
        let state = state();
        let bindings = bindings();
        let r = Resolver::new(&state, &bindings);
        assert!(matches!(
            r.text("${leg:C.call-id}").unwrap_err(),
            ResolveError::LegUnknown { .. }
        ));
        assert!(matches!(
            r.text("${step:s99.status}").unwrap_err(),
            ResolveError::StepNotRun { .. }
        ));
        assert!(matches!(
            r.text("${step:s7.header.Refer-To}").unwrap_err(),
            ResolveError::StepFieldUnset { .. }
        ));
        assert!(matches!(
            r.text("${step:a2.branch}").unwrap_err(),
            ResolveError::BranchNotCommitted { .. }
        ));
        assert!(matches!(
            r.text("tag=${leg:B.remote-tag").unwrap_err(),
            ResolveError::Malformed { .. }
        ));
    }

    #[test]
    fn an_unset_leg_field_names_itself_rather_than_resolving_empty() {
        let mut state = RunState::new();
        state.leg_mut("A");
        let bindings = bindings();
        let r = Resolver::new(&state, &bindings);
        let err = r.text("${leg:A.remote-tag}").unwrap_err();
        assert_eq!(
            err,
            ResolveError::LegFieldUnset {
                accessor: "${leg:A.remote-tag}".into(),
                leg: "A".into(),
                field: "remote tag",
            }
        );
        // An empty route set is a FACT, not an unset field.
        assert_eq!(r.text("${leg:A.route-set}").unwrap(), "");
    }

    #[test]
    fn arithmetic_is_a_reference_plus_a_delta() {
        let state = state();
        let bindings = bindings();
        let r = Resolver::new(&state, &bindings);
        let computed: Computed =
            serde_json::from_str(r#"{"from":"${step:s7.cseq}","delta":3}"#).unwrap();
        assert_eq!(r.computed(&computed).unwrap(), 14);
        let negative: Computed =
            serde_json::from_str(r#"{"from":"${step:s7.cseq}","delta":-20}"#).unwrap();
        assert!(matches!(
            r.computed(&negative).unwrap_err(),
            ResolveError::OutOfRange { .. }
        ));
        let textual: Computed =
            serde_json::from_str(r#"{"from":"${step:s7.header.To}","delta":1}"#).unwrap();
        assert!(matches!(
            r.computed(&textual).unwrap_err(),
            ResolveError::NotNumeric { .. }
        ));
    }
}
