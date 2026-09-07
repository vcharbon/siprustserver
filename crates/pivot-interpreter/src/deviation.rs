//! **Deviations** (`PCAP2TEST_PIVOT_V3.md` §11): the non-compliance an emission
//! must reproduce.
//!
//! Every violation lives in one block, so what breaks the rules is greppable;
//! there are no inline tier-1 overrides on a step. `kind` is an OPEN token, so a
//! document naming a kind this interpreter does not implement still PARSES and
//! still lints.
//!
//! **A deviation this interpreter cannot EMIT fails the run, by name.** Three
//! populations reach that rule and all are refused: a kind the format does not
//! define, a defined kind whose emission path is not built, and a `preserve`
//! token no emission path guarantees. A kind silently ignored is a replay that
//! no longer reproduces the defect it exists for — it would run green while
//! emitting a compliant message, which is the one failure mode this block
//! exists to prevent.

use pivot_schema::bundle::Failure;
use pivot_schema::deviation::{CseqValue, Deviation};

/// What a deviation entry asks of an emission, and whether this interpreter can
/// do it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Withhold the named automatic. A withheld ACK is this, not a hole in the
    /// flow.
    SuppressAuto,
    /// One header's grammar is deliberately broken: the stored value goes out
    /// untouched and the renderer stands down for that header alone.
    MalformedHeader { header: String, preserve: Vec<String> },
    /// The stored message rides EXACTLY as the document holds it: every stated
    /// header, in the stated order, under the stated casing. The tier-1 lines
    /// the document never stores are still the stack's (§8).
    Verbatim { preserve: Vec<String> },
    /// The stored header order survives emission — the same guarantee
    /// [`Effect::Verbatim`] makes about order, asked for on its own.
    RawOrder { preserve: Vec<String> },
    /// The request carries this CSeq instead of the one the stack would number.
    CseqOverride { value: CseqValue },
    /// A kind the format DEFINES whose emission path this interpreter does not
    /// have. Compiles, lints, and refuses to run.
    NotEmittable { kind: String },
    /// A `preserve` token this interpreter does not guarantee. Honouring the
    /// tokens it knows and ignoring the rest would emit a message that does not
    /// preserve what the entry asked for.
    PreserveUnsupported { kind: String, token: String },
    /// A kind this interpreter does not model at all.
    Unknown { kind: String },
}

/// The `preserve` tokens an emission path guarantees: the frozen block's stated
/// order, and the name casing the document states.
const PRESERVABLE: [&str; 2] = ["header-order", "casing"];

impl Effect {
    /// Why a refused effect cannot run, in the words a verdict carries.
    pub fn refusal(&self) -> Option<&'static str> {
        match self {
            Effect::SuppressAuto
            | Effect::MalformedHeader { .. }
            | Effect::Verbatim { .. }
            | Effect::RawOrder { .. }
            | Effect::CseqOverride { .. } => None,
            Effect::NotEmittable { .. } => {
                Some("this interpreter has no emission path for it, and emitting a compliant \
                      message instead would not reproduce the defect")
            }
            Effect::PreserveUnsupported { .. } => {
                Some("this interpreter does not guarantee that property through emission, and \
                      emitting without it would not reproduce the defect")
            }
            Effect::Unknown { .. } => Some("this interpreter does not model this kind"),
        }
    }
}

/// The effect a deviation entry asks for.
///
/// A defined kind whose payload the plan already holds becomes the emission
/// property it names; a kind this interpreter cannot emit, and a `preserve`
/// token it cannot guarantee, become refusals the run states by name.
pub fn effect(deviation: &Deviation) -> Effect {
    let unsupported = |kind: &str| {
        deviation
            .preserve
            .iter()
            .find(|token| !PRESERVABLE.contains(&token.as_str()))
            .map(|token| Effect::PreserveUnsupported {
                kind: kind.to_string(),
                token: token.clone(),
            })
    };
    match deviation.kind.as_str() {
        "suppress-auto" => Effect::SuppressAuto,
        "malformed-header" => match &deviation.header {
            // The plan refuses a header-less `malformed-header`, so reaching
            // here means the entry was built outside compilation.
            None => Effect::NotEmittable { kind: deviation.kind.clone() },
            Some(header) => Effect::MalformedHeader {
                header: header.clone(),
                preserve: deviation.preserve.clone(),
            },
        },
        "verbatim-emission" => unsupported("verbatim-emission")
            .unwrap_or_else(|| Effect::Verbatim { preserve: deviation.preserve.clone() }),
        "raw-order" => unsupported("raw-order")
            .unwrap_or_else(|| Effect::RawOrder { preserve: deviation.preserve.clone() }),
        // The plan refuses a value-less `cseq-override`, so reaching here
        // without one means the entry was built outside compilation.
        "cseq-override" => match &deviation.value {
            None => Effect::NotEmittable { kind: deviation.kind.clone() },
            Some(value) => Effect::CseqOverride { value: value.clone() },
        },
        other => Effect::Unknown { kind: other.to_string() },
    }
}

/// Why a `cseq-override` cannot be emitted on this message, if it cannot.
///
/// The override replaces a number the STACK chooses. Two messages carry a CSeq
/// number that is not the stack's to choose — a response's is the key of the
/// server transaction it answers, and a CANCEL's is matched hop by hop — so an
/// override on one of them cannot be honoured, and emitting the compliant
/// number while the document asks for another would run green without
/// reproducing the defect. An ACK's number is a UAC-core choice like any other
/// request's, and an override reaches it.
pub fn cseq_override_refusal(status: Option<u16>, method: Option<&str>) -> Option<&'static str> {
    match (status, method) {
        (Some(_), _) => Some(
            "a response repeats the CSeq of the request it answers (RFC 3261 §8.2.6.2)",
        ),
        (None, Some(m)) if m.eq_ignore_ascii_case("CANCEL") => Some(
            "a CANCEL carries the CSeq number of the request it cancels (RFC 3261 §9.1)",
        ),
        _ => None,
    }
}

/// What one step's deviations amount to, resolved once at emission time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StepEffects {
    /// The automatic this step names is withheld.
    pub suppressed: bool,
    /// Headers whose stored value is emitted untouched, grammar and all.
    pub malformed_headers: Vec<String>,
    /// The deviations asking the stated header block to ride in its stated
    /// order and casing. The emission is VERIFIED against the document, so the
    /// property is held rather than hoped for.
    pub preserve_stored_block: Vec<String>,
    /// The CSeq this step emits instead of the stack's own, with the deviation
    /// that states it.
    pub cseq_override: Option<(String, CseqValue)>,
    /// Entries the run must refuse, each with the reason, in document order.
    pub refused: Vec<(Deviation, &'static str)>,
}

impl StepEffects {
    /// Fold the deviations that apply to one step.
    pub fn of<'a>(deviations: impl IntoIterator<Item = &'a Deviation>) -> StepEffects {
        let mut out = StepEffects::default();
        for deviation in deviations {
            let effect = effect(deviation);
            if let Some(reason) = effect.refusal() {
                out.refused.push((deviation.clone(), reason));
                continue;
            }
            match effect {
                Effect::SuppressAuto => out.suppressed = true,
                Effect::MalformedHeader { header, .. } => out.malformed_headers.push(header),
                Effect::Verbatim { .. } | Effect::RawOrder { .. } => {
                    out.preserve_stored_block.push(deviation.id.clone())
                }
                Effect::CseqOverride { value } => {
                    out.cseq_override = Some((deviation.id.clone(), value))
                }
                Effect::NotEmittable { .. }
                | Effect::PreserveUnsupported { .. }
                | Effect::Unknown { .. } => {
                    unreachable!("a refused effect is handled above")
                }
            }
        }
        out
    }

    /// The failures a run must state before emitting this step, if any.
    pub fn refusals(&self) -> Vec<Failure> {
        self.refused
            .iter()
            .map(|(deviation, reason)| Failure::DeviationUnimplemented {
                deviation: deviation.id.clone(),
                kind: deviation.kind.clone(),
                step: deviation.step.clone(),
                reason: (*reason).to_string(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deviation(text: &str) -> Deviation {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn withholding_an_automatic_and_breaking_one_header_state_their_own_effect() {
        assert_eq!(
            effect(&deviation(r#"{"id":"d3","kind":"suppress-auto","step":"s12"}"#)),
            Effect::SuppressAuto
        );
        assert_eq!(
            effect(&deviation(
                r#"{"id":"d5","kind":"malformed-header","step":"s8","header":"Refer-To","preserve":["header-value"]}"#
            )),
            Effect::MalformedHeader {
                header: "Refer-To".into(),
                preserve: vec!["header-value".into()]
            }
        );
        let effects = StepEffects::of([&deviation(
            r#"{"id":"d5","kind":"malformed-header","step":"s8","header":"Refer-To","preserve":["header-value"]}"#,
        )]);
        assert!(effects.refusals().is_empty());
        assert_eq!(effects.malformed_headers, ["Refer-To"]);
    }

    /// The three emission paths the format defines, each folded into the
    /// property the emission then holds itself to.
    #[test]
    fn the_emission_kinds_fold_into_the_property_they_state() {
        let verbatim = deviation(
            r#"{"id":"d1","kind":"verbatim-emission","step":"s1","preserve":["header-order","casing"]}"#,
        );
        assert_eq!(
            effect(&verbatim),
            Effect::Verbatim { preserve: vec!["header-order".into(), "casing".into()] }
        );
        let raw_order =
            deviation(r#"{"id":"d2","kind":"raw-order","leg":"A","preserve":["header-order"]}"#);
        assert_eq!(effect(&raw_order), Effect::RawOrder { preserve: vec!["header-order".into()] });
        let absolute = deviation(r#"{"id":"d4","kind":"cseq-override","leg":"A","value":42}"#);
        assert_eq!(effect(&absolute), Effect::CseqOverride { value: CseqValue::Absolute(42) });

        let effects = StepEffects::of([&verbatim, &raw_order, &absolute]);
        assert!(effects.refusals().is_empty(), "{:#?}", effects.refusals());
        assert_eq!(effects.preserve_stored_block, ["d1", "d2"]);
        assert_eq!(effects.cseq_override, Some(("d4".into(), CseqValue::Absolute(42))));
    }

    /// The false-success guard: a property the emission cannot GUARANTEE must
    /// never run green under a document that asked for it.
    #[test]
    fn a_preserve_token_no_emission_path_guarantees_refuses_the_run_by_name() {
        let entry = deviation(
            r#"{"id":"d1","kind":"verbatim-emission","step":"s1","preserve":["header-order","absolute-order"]}"#,
        );
        assert_eq!(
            effect(&entry),
            Effect::PreserveUnsupported {
                kind: "verbatim-emission".into(),
                token: "absolute-order".into()
            }
        );
        let refusals = StepEffects::of([&entry]).refusals();
        assert_eq!(refusals.len(), 1);
        assert!(
            matches!(&refusals[0], Failure::DeviationUnimplemented { kind, reason, .. }
                if kind == "verbatim-emission" && reason.contains("does not guarantee")),
            "{:?}",
            refusals[0]
        );
    }

    /// The CSeq a CANCEL or a response carries is copied, not chosen — so an
    /// override on one of them is refused rather than dropped.
    #[test]
    fn a_cseq_override_is_refused_where_the_stack_does_not_choose_the_number() {
        assert!(cseq_override_refusal(Some(200), Some("INVITE")).is_some());
        assert!(cseq_override_refusal(Some(487), Some("CANCEL")).is_some());
        assert!(cseq_override_refusal(None, Some("CANCEL")).is_some());
        assert!(cseq_override_refusal(None, Some("cancel")).is_some());
        for method in ["INVITE", "ACK", "ack", "BYE", "INFO", "UPDATE", "REFER", "PRACK"] {
            assert!(cseq_override_refusal(None, Some(method)).is_none(), "{method}");
        }
    }

    #[test]
    fn an_unknown_kind_parses_and_then_refuses_to_run() {
        let unknown = deviation(r#"{"id":"d9","kind":"drop-every-third-packet","leg":"A"}"#);
        assert_eq!(effect(&unknown), Effect::Unknown { kind: "drop-every-third-packet".into() });
        let refusals = StepEffects::of([&unknown]).refusals();
        assert_eq!(refusals.len(), 1);
        assert!(
            matches!(&refusals[0], Failure::DeviationUnimplemented { kind, reason, .. }
                if kind == "drop-every-third-packet" && reason.contains("does not model"))
        );
    }

    #[test]
    fn several_deviations_on_one_step_fold_without_losing_any() {
        let verbatim = deviation(
            r#"{"id":"d1","kind":"verbatim-emission","step":"s1","preserve":["header-order"]}"#,
        );
        let malformed = deviation(
            r#"{"id":"d2","kind":"malformed-header","step":"s1","header":"Refer-To","preserve":["header-value"]}"#,
        );
        let unknown = deviation(r#"{"id":"d3","kind":"drop-every-third-packet","step":"s1"}"#);
        let effects = StepEffects::of([&verbatim, &malformed, &unknown]);
        assert_eq!(effects.malformed_headers, ["Refer-To"]);
        assert_eq!(effects.preserve_stored_block, ["d1"]);
        assert_eq!(effects.refusals().len(), 1, "the unemittable one is still refused");
    }
}
