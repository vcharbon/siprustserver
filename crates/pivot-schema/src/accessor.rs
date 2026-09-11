//! Accessors (`PCAP2TEST_PIVOT_V3.md` §8): the `${…}` grammar by which a
//! document names a value the RUN produced rather than a value the capture
//! held.
//!
//! Four namespaces, and the order matters. **Leg accessors are primary**:
//! `${leg:B.remote-tag}` resolves against the runner's own dialog state, which
//! is what asserting a dialog's own tags, composing a header out of them or
//! routing through a proxy actually needs. **Early-dialog accessors**
//! (`${early:f2.tag}`) name ONE fork of a forking leg by the `early` id a step
//! carries (§6.1): a leg accessor is single-valued and a leg ringing two forks
//! has two To-tags and two RSeq spaces (RFC 3262 §3, RFC 3261 §12.1.1), so
//! per-fork facts have their own namespace. **Step accessors are secondary**: `${step:s7.header.To}`
//! covers the rare specific-message case. A body is only ever a match target,
//! never extracted and resent. **Number accessors** (`${num:transferee:e164}`)
//! resolve against the document's identity registry and the lane's binding of
//! it, so a number-bearing header composes symbolically instead of freezing
//! the number one capture happened to carry.
//!
//! Arithmetic is STRUCTURAL, not an expression language: [`Computed`] is
//! `{ from, delta }` and nothing else is computable. An accessor that needed
//! more than that would be a program, and this document does not run programs.

use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One resolved-at-run-time reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Accessor {
    /// Dialog state of a leg.
    Leg { leg: String, field: LegField },
    /// State of ONE early dialog, named by the `early` id its steps carry.
    Early { early: String, field: EarlyField },
    /// A field of one already-seen message.
    Step { step: String, field: StepField },
    /// A registered identity, in one of its dial forms, as the lane bound it.
    Number { name: String, form: String },
}

/// What a leg accessor reads off runner dialog state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegField {
    CallId,
    LocalTag,
    RemoteTag,
    RemoteTarget,
    RouteSet,
    CseqLocal,
    CseqRemote,
    Rseq,
}

/// What an early-dialog accessor reads off ONE fork of a forking leg.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EarlyField {
    /// The To-tag this fork answers under (RFC 3261 §12.1.1).
    Tag,
    /// The RSeq of the last reliable provisional that rode this fork
    /// (RFC 3262 §3). Each fork numbers its own RSeq space.
    Rseq,
}

/// What a step accessor reads off a message the run already handled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepField {
    /// A header of the message, by name.
    Header(String),
    Cseq,
    Rseq,
    Status,
    /// Which branch of an `alt` node ran. Only an `alt` id answers it.
    Branch,
}

impl LegField {
    const NAMES: [(&'static str, LegField); 8] = [
        ("call-id", LegField::CallId),
        ("local-tag", LegField::LocalTag),
        ("remote-tag", LegField::RemoteTag),
        ("remote-target", LegField::RemoteTarget),
        ("route-set", LegField::RouteSet),
        ("cseq.local", LegField::CseqLocal),
        ("cseq.remote", LegField::CseqRemote),
        ("rseq", LegField::Rseq),
    ];

    fn name(self) -> &'static str {
        Self::NAMES.iter().find(|(_, f)| *f == self).expect("every field is named").0
    }
}

impl EarlyField {
    const NAMES: [(&'static str, EarlyField); 2] =
        [("tag", EarlyField::Tag), ("rseq", EarlyField::Rseq)];

    fn name(self) -> &'static str {
        Self::NAMES.iter().find(|(_, f)| *f == self).expect("every field is named").0
    }
}

impl fmt::Display for StepField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StepField::Header(name) => write!(f, "header.{name}"),
            StepField::Cseq => f.write_str("cseq"),
            StepField::Rseq => f.write_str("rseq"),
            StepField::Status => f.write_str("status"),
            StepField::Branch => f.write_str("branch"),
        }
    }
}

impl Accessor {
    /// The id this accessor targets — a leg id, a step id or an identity name.
    pub fn target(&self) -> &str {
        match self {
            Accessor::Leg { leg, .. } => leg,
            Accessor::Early { early, .. } => early,
            Accessor::Step { step, .. } => step,
            Accessor::Number { name, .. } => name,
        }
    }

    /// Parse the INSIDE of a `${…}`, or say why it is not an accessor.
    pub fn parse_body(body: &str) -> Result<Self, String> {
        // Ids carry no `.`: the first dot separates the id from the field, so
        // `cseq.local` and `header.X-Foo` stay readable as one field name.
        let (namespace, rest) = body.split_once(':').ok_or_else(|| {
            format!("accessor {body:?} states no namespace (`leg:`, `early:`, `step:` or `num:`)")
        })?;
        // The number namespace separates on `:` throughout: an identity name is
        // an id, a dial form is an open plan token, and neither is a field path.
        if namespace == "num" {
            let (name, form) = rest
                .split_once(':')
                .ok_or_else(|| format!("accessor {body:?} names an identity but no dial form"))?;
            if name.is_empty() || form.is_empty() {
                return Err(format!(
                    "accessor {body:?} states an empty identity name or dial form"
                ));
            }
            return Ok(Accessor::Number { name: name.to_string(), form: form.to_string() });
        }
        let (id, field) = rest
            .split_once('.')
            .ok_or_else(|| format!("accessor {body:?} states an id but no field"))?;
        if id.is_empty() {
            return Err(format!("accessor {body:?} names no {namespace}"));
        }
        match namespace {
            "leg" => {
                let found = LegField::NAMES.iter().find(|(name, _)| *name == field);
                match found {
                    Some((_, f)) => Ok(Accessor::Leg { leg: id.to_string(), field: *f }),
                    None => Err(format!(
                        "leg field {field:?} is none of {}",
                        LegField::NAMES.map(|(n, _)| n).join(", ")
                    )),
                }
            }
            "early" => {
                let found = EarlyField::NAMES.iter().find(|(name, _)| *name == field);
                match found {
                    Some((_, f)) => Ok(Accessor::Early { early: id.to_string(), field: *f }),
                    None => Err(format!(
                        "early-dialog field {field:?} is none of {}",
                        EarlyField::NAMES.map(|(n, _)| n).join(", ")
                    )),
                }
            }
            "step" => {
                let field = match field {
                    "cseq" => StepField::Cseq,
                    "rseq" => StepField::Rseq,
                    "status" => StepField::Status,
                    "branch" => StepField::Branch,
                    other => match other.strip_prefix("header.") {
                        Some("") => return Err("accessor `header.` names no header".into()),
                        Some(name) => StepField::Header(name.to_string()),
                        None => {
                            return Err(format!(
                                "step field {other:?} is none of header.<name>, cseq, rseq, status, branch"
                            ));
                        }
                    },
                };
                Ok(Accessor::Step { step: id.to_string(), field })
            }
            other => Err(format!(
                "accessor namespace {other:?} is none of `leg`, `early`, `step`, `num`"
            )),
        }
    }

    /// Every `${…}` in `text`, each parsed or refused. An unterminated `${`
    /// is itself a refusal, so a truncated accessor cannot pass as literal
    /// text.
    pub fn scan(text: &str) -> Vec<Result<Accessor, String>> {
        let mut out = Vec::new();
        let mut rest = text;
        while let Some(start) = rest.find("${") {
            let after = &rest[start + 2..];
            match after.find('}') {
                Some(end) => {
                    out.push(Accessor::parse_body(&after[..end]));
                    rest = &after[end + 1..];
                }
                None => {
                    out.push(Err(format!(
                        "accessor {:?} is not terminated by `}}`",
                        &rest[start..]
                    )));
                    break;
                }
            }
        }
        out
    }

    /// Whether `text` carries any `${…}` at all — what the interpreter's
    /// resolver tests before scanning.
    pub fn present_in(text: &str) -> bool {
        text.contains("${")
    }

    /// Whether `text` carries a RUN-TIME accessor — one that reads dialog
    /// state at run time (`leg:`/`early:`/`step:`), or any malformed `${…}`.
    /// A `num:` composition is not one: it resolves statically through the
    /// document's own identities and declared dial forms, so a captured
    /// number-bearing header may carry it and the generator-subset gate
    /// tests exactly this (the accessor validator still checks it resolves).
    pub fn run_time_in(text: &str) -> bool {
        Self::scan(text).into_iter().any(|found| !matches!(found, Ok(Accessor::Number { .. })))
    }
}

impl fmt::Display for Accessor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Accessor::Leg { leg, field } => write!(f, "${{leg:{leg}.{}}}", field.name()),
            Accessor::Early { early, field } => write!(f, "${{early:{early}.{}}}", field.name()),
            Accessor::Step { step, field } => write!(f, "${{step:{step}.{field}}}"),
            Accessor::Number { name, form } => write!(f, "${{num:{name}:{form}}}"),
        }
    }
}

impl FromStr for Accessor {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let body = s
            .strip_prefix("${")
            .and_then(|s| s.strip_suffix('}'))
            .ok_or_else(|| format!("accessor {s:?} is not wrapped in `${{…}}`"))?;
        Accessor::parse_body(body)
    }
}

crate::string_token!(
    Accessor,
    "A run-time value: `${leg:<id>.<field>}`, `${early:<id>.<field>}`, `${step:<id>.<field>}` or `${num:<identity>:<form>}`.",
    "^\\$\\{((leg|early|step):[^.{}]+\\.[^{}]+|num:[^:{}]+:[^:{}]+)\\}$"
);

/// The one computable form: a referenced value plus a signed offset. There is
/// no expression language, deliberately — `{ from, delta }` covers "the CSeq
/// the peer sent, plus one", which is every arithmetic the corpus and the
/// ported tests need.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Computed {
    /// The accessor the value is read from.
    pub from: Accessor,
    /// Signed offset applied to it.
    pub delta: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_documented_accessor_round_trips() {
        for token in [
            "${leg:B.call-id}",
            "${leg:B.local-tag}",
            "${leg:B.remote-tag}",
            "${leg:B.remote-target}",
            "${leg:B.route-set}",
            "${leg:B.cseq.local}",
            "${leg:B.cseq.remote}",
            "${leg:B.rseq}",
            "${early:f1.tag}",
            "${early:f2.rseq}",
            "${step:s7.header.To}",
            "${step:s7.cseq}",
            "${step:s7.rseq}",
            "${step:s7.status}",
            "${step:a1.branch}",
            "${num:caller:private}",
            "${num:called-0-1:trunk-composed}",
        ] {
            assert_eq!(token.parse::<Accessor>().unwrap().to_string(), token, "{token}");
        }
    }

    #[test]
    fn a_number_accessor_names_an_identity_and_the_form_to_dial_it_in() {
        let accessor: Accessor = "${num:transferee:e164}".parse().unwrap();
        assert_eq!(accessor, Accessor::Number { name: "transferee".into(), form: "e164".into() });
        assert_eq!(accessor.target(), "transferee");
    }

    #[test]
    fn a_number_accessor_without_both_halves_is_refused() {
        for token in ["${num:transferee}", "${num::e164}", "${num:transferee:}", "${num:}"] {
            assert!(token.parse::<Accessor>().is_err(), "{token:?} should be refused");
        }
    }

    /// §6.1: an early id names ONE fork, and the fork's own To-tag and RSeq are
    /// what a leg accessor cannot state — a leg has one of each and a forking
    /// leg has two.
    #[test]
    fn an_early_accessor_names_a_fork_and_the_fact_read_off_it() {
        let tag: Accessor = "${early:f2.tag}".parse().unwrap();
        assert_eq!(tag, Accessor::Early { early: "f2".into(), field: EarlyField::Tag });
        assert_eq!(tag.target(), "f2");
        let rseq: Accessor = "${early:f2.rseq}".parse().unwrap();
        assert_eq!(rseq, Accessor::Early { early: "f2".into(), field: EarlyField::Rseq });
    }

    #[test]
    fn a_field_outside_the_vocabulary_is_refused_by_name() {
        for token in [
            "${leg:B.local-target}",
            "${step:s7.body}",
            "${dialog:B.call-id}",
            "${leg:B}",
            "${leg:.call-id}",
            "${step:s7.header.}",
            "leg:B.call-id",
            "${early:f1.local-tag}",
            "${early:f1.remote-tag}",
            "${early:f1}",
            "${early:.tag}",
        ] {
            assert!(token.parse::<Accessor>().is_err(), "{token:?} should be refused");
        }
    }

    #[test]
    fn scanning_finds_every_accessor_embedded_in_a_header_value() {
        let value = "<sip:x@h>;?X-Dialog=${leg:B.call-id}%3Bto-tag%3D${leg:B.remote-tag}";
        let found: Vec<String> =
            Accessor::scan(value).into_iter().map(|a| a.unwrap().to_string()).collect();
        assert_eq!(found, ["${leg:B.call-id}", "${leg:B.remote-tag}"]);
        assert!(Accessor::scan("no accessors here").is_empty());
        assert!(!Accessor::present_in("no accessors here"));
    }

    #[test]
    fn an_unterminated_accessor_is_a_refusal_not_literal_text() {
        let found = Accessor::scan("<sip:x@h>;tag=${leg:B.remote-tag");
        assert_eq!(found.len(), 1);
        assert!(found[0].is_err());
    }

    #[test]
    fn arithmetic_is_a_reference_plus_a_delta_and_nothing_else() {
        let computed: Computed =
            serde_json::from_str(r#"{"from":"${step:s7.cseq}","delta":1}"#).unwrap();
        assert_eq!(computed.delta, 1);
        assert_eq!(computed.from.target(), "s7");
        assert!(
            serde_json::from_str::<Computed>(r#"{"from":"${step:s7.cseq}","times":2}"#).is_err()
        );
    }
}
