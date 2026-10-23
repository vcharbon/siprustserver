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
            let accessor = Accessor::parse_body(&after[..end])
                .map_err(|detail| ResolveError::Malformed { text: text.to_string(), detail })?;
            out.push_str(&self.accessor(&accessor)?);
            rest = &after[end + 1..];
        }
        out.push_str(rest);
        Ok(out)
    }

    /// One accessor's value, or why this run cannot answer it.
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

    fn leg_field(&self, name: &str, leg: &str, field: LegField) -> Result<String, ResolveError> {
        let state = self.state.leg(leg).ok_or_else(|| ResolveError::LegUnknown {
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
            return self.state.branch(step).map(str::to_string).ok_or_else(|| {
                ResolveError::BranchNotCommitted {
                    accessor: name.to_string(),
                    alt: step.to_string(),
                }
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
        let base: i64 = raw
            .parse()
            .map_err(|_| ResolveError::NotNumeric { accessor: name.clone(), value: raw.clone() })?;
        let sum = base + computed.delta;
        u32::try_from(sum).map_err(|_| ResolveError::OutOfRange { accessor: name, value: sum })
    }
}

#[cfg(test)]
mod tests;
