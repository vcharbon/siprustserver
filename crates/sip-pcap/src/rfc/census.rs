//! The census: one sweep's worth of detector output, aggregated across a
//! corpus and printed as a report.
//!
//! Denominators travel with the hits. A rule's violation count means one thing
//! against ten occasions and another against ten thousand, and the share of
//! occasions the capture could not DECIDE is the price of each detector's
//! conservatism — stated, so a reader never mistakes an undecidable occasion
//! for a clean one.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::doc::FlowsDoc;

use rfc_rules::rules::retransmit::Class as RungClass;

use super::{scan_with, Evidence, Hit, RfcRule};

/// What one rule found across the whole sweep.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuleTally {
    pub hits: u64,
    /// Documents carrying at least one hit of this rule.
    pub documents: u64,
    /// Occasions the rule could have been broken — the rule's own population.
    pub occasions: u64,
    /// Of those, the ones the capture could DECIDE. `hits ⊆ decided ⊆
    /// occasions`.
    pub decided: u64,
    /// Hits by emitter role token.
    pub by_role: BTreeMap<String, u64>,
    /// Hits whose emitter forwarded the message on rather than originating the
    /// behaviour (see [`Hit::relayed`]).
    pub relayed: u64,
    /// The rule's own distribution over its hits: the cancel rule's
    /// capture-time gap between the CANCEL and the 2xx, the PRACK rule's
    /// observed window the UAC had, the ACK rule's corroboration shape. Short
    /// gaps are the ones a distant capture point could have ordered wrongly.
    pub buckets: BTreeMap<String, u64>,
}

/// One hit, located in the corpus.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocatedHit {
    /// Path of the flows document, as given to the sweep.
    pub document: String,
    /// The capture the document came from — the document's own directory name
    /// in the corpus layout.
    pub capture: String,
    #[serde(flatten)]
    pub hit: Hit,
}

/// A document the sweep could not read. Recorded rather than dropped: a census
/// that silently skipped a document would understate every count.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadFailure {
    pub document: String,
    pub reason: String,
}

/// The whole sweep: what was scanned, what each rule found, and every hit.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Census {
    /// Documents read and scanned.
    pub documents: u64,
    pub groups: u64,
    pub legs: u64,
    pub messages: u64,
    pub rules: BTreeMap<String, RuleTally>,
    pub hits: Vec<LocatedHit>,
    pub failures: Vec<ReadFailure>,
    /// Rules run beside the WIRE vocabulary to take their baseline (see
    /// [`scan_with`]). Sweep configuration, not a result: it is not reported.
    #[serde(skip)]
    pub candidates: Vec<RfcRule>,
}

impl Census {
    /// A census with every known rule present at zero — a rule with no hits is
    /// a reported result, not a missing key.
    pub fn new() -> Self {
        Census::with_candidates(&[])
    }

    /// A census that also runs `candidates` — rules taking their corpus
    /// baseline — each present at zero like the WIRE rules.
    pub fn with_candidates(candidates: &[RfcRule]) -> Self {
        let mut census = Census { candidates: candidates.to_vec(), ..Census::default() };
        for rule in RfcRule::WIRE.iter().chain(candidates) {
            census.rules.insert(rule.token().to_string(), RuleTally::default());
        }
        census
    }

    /// Scan one document into the census.
    pub fn absorb(&mut self, document: &str, capture: &str, doc: &FlowsDoc) {
        self.documents += 1;
        self.groups += doc.groups.len() as u64;
        self.legs += doc.legs.len() as u64;
        self.messages += doc.legs.iter().map(|l| l.msgs.len() as u64).sum::<u64>();
        let scan = scan_with(doc, &self.candidates);
        // Each detector brings its own denominators; the census only adds them
        // up under the token the detector counted them against.
        for (token, population) in scan.population {
            let tally = self.rules.entry(token.to_string()).or_default();
            tally.occasions += population.occasions;
            tally.decided += population.decided;
        }
        let mut seen: BTreeSet<&'static str> = BTreeSet::new();
        for hit in scan.hits {
            let token = hit.rule.token();
            let tally = self.rules.entry(token.to_string()).or_default();
            tally.hits += 1;
            if seen.insert(token) {
                tally.documents += 1;
            }
            *tally.by_role.entry(hit.emitter_role.token().to_string()).or_default() += 1;
            if hit.relayed {
                tally.relayed += 1;
            }
            *tally.buckets.entry(bucket_of(&hit.evidence).to_string()).or_default() += 1;
            self.hits.push(LocatedHit {
                document: document.to_string(),
                capture: capture.to_string(),
                hit,
            });
        }
    }

    /// Record a document that could not be read.
    pub fn fail(&mut self, document: &str, reason: String) {
        self.failures.push(ReadFailure { document: document.to_string(), reason });
    }

    /// Fold another census in — the shape a parallel sweep merges with.
    pub fn merge(&mut self, other: Census) {
        self.documents += other.documents;
        self.groups += other.groups;
        self.legs += other.legs;
        self.messages += other.messages;
        for (token, tally) in other.rules {
            let mine = self.rules.entry(token).or_default();
            mine.hits += tally.hits;
            mine.documents += tally.documents;
            mine.occasions += tally.occasions;
            mine.decided += tally.decided;
            mine.relayed += tally.relayed;
            for (role, n) in tally.by_role {
                *mine.by_role.entry(role).or_default() += n;
            }
            for (bucket, n) in tally.buckets {
                *mine.buckets.entry(bucket).or_default() += n;
            }
        }
        self.hits.extend(other.hits);
        self.failures.extend(other.failures);
    }

    /// Put the sweep's output in a stable order, so two runs over one corpus
    /// produce byte-identical reports whatever order the workers finished in.
    pub fn sort(&mut self) {
        self.hits.sort_by(|a, b| {
            (&a.capture, &a.document, a.hit.rule, a.hit.leg, a.hit.evidence.anchor()).cmp(&(
                &b.capture,
                &b.document,
                b.hit.rule,
                b.hit.leg,
                b.hit.evidence.anchor(),
            ))
        });
        self.failures.sort_by(|a, b| a.document.cmp(&b.document));
    }

    /// The human summary, one block per rule.
    pub fn summary(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "scanned {} document(s): {} group(s), {} leg(s), {} message(s); {} unreadable\n",
            self.documents,
            self.groups,
            self.legs,
            self.messages,
            self.failures.len()
        ));
        for (token, tally) in &self.rules {
            out.push_str(&format!(
                "{token}: {} hit(s) in {} document(s)\n  \
                 {} occasion(s) → {} the capture could decide → {} violation(s)\n",
                tally.hits, tally.documents, tally.occasions, tally.decided, tally.hits
            ));
            if tally.hits == 0 {
                continue;
            }
            for (role, n) in &tally.by_role {
                out.push_str(&format!("  emitter {role}: {n}\n"));
            }
            out.push_str(&format!(
                "  originated {} / relayed onward {}\n",
                tally.hits - tally.relayed,
                tally.relayed
            ));
            for (bucket, n) in &tally.buckets {
                out.push_str(&format!("  {bucket}: {n}\n"));
            }
        }
        out
    }
}

/// Which distribution bucket a hit falls in — the rule's own measure, named so
/// the report needs no legend.
fn bucket_of(evidence: &Evidence) -> &'static str {
    match evidence {
        Evidence::Cancelled { gap_us, .. } => match gap_us {
            0..=999 => "gap <1ms",
            1_000..=9_999 => "gap <10ms",
            _ => "gap >=10ms",
        },
        Evidence::Unacked { window_us, .. } => match window_us {
            0..=1_999_999 => "window <2s",
            2_000_000..=9_999_999 => "window <10s",
            _ => "window >=10s",
        },
        // What the leg did NEXT, which is what a reader weighs an absence by.
        Evidence::NoAck { retransmits, bye_after_us, .. } => match (retransmits, bye_after_us) {
            (1.., _) => "the UAS reran its 2xx ladder",
            (0, Some(_)) => "torn down un-ACKed",
            (0, None) => "the dialog went silent",
        },
        // What the emitter had already done with the transaction it cancelled.
        Evidence::LateCancel { since_final_us, .. } => match since_final_us {
            0..=99_999 => "cancel <100ms after the final",
            100_000..=999_999 => "cancel <1s after the final",
            _ => "cancel >=1s after the final",
        },
        // WHAT about the plan changed, which is what a reader weighs the
        // divergence by: the stream table, where the media goes, or what rides
        // it.
        Evidence::SecondAnswerDiverged { first_plan, second_plan, .. } => {
            if first_plan.len() != second_plan.len() {
                "the stream count changed"
            } else if first_plan.first() != second_plan.first() {
                "the connection address moved"
            } else {
                "a stream's port or formats changed"
            }
        }
        // Not census rules (RuleId::WIRE) — no capture hit carries these yet,
        // so each names its own measure and no bucket of theirs is counted.
        Evidence::Uncleared { .. } => "uncleared",
        Evidence::UnknownRack { .. } => "rack names no known invite",
        Evidence::Overlapping { .. } => "provisionals overlapped",
        Evidence::RseqGap { .. } => "rseq gap",
        Evidence::OutOfOrderRack { .. } => "rack out of order",
        Evidence::MultipleFinals { .. } => "finals disagreed",
        Evidence::CancelRouteDiverged { .. } => "cancel route diverged",
        Evidence::EagerCancel { .. } => "cancel before any provisional",
        Evidence::CseqReused { .. } => "in-dialog cseq reused",
        Evidence::CseqNotContiguous { .. } => "in-dialog cseq not contiguous",
        Evidence::ResponseCseqUnmatched { .. } => "response cseq off its transaction",
        Evidence::AckCseqUnmatched { .. } => "ack cseq names no invite",
        Evidence::MidDialogUriChanged { .. } => "in-dialog uri rewritten",
        Evidence::MidDialogRouteDiverged { .. } => "in-dialog route set not reproduced",
        Evidence::MidDialogWireTargetDiverged { .. } => "sent past the derived destination",
        Evidence::RecordRouteMisplaced { .. } => "record-route off a dialog-creating response",
        Evidence::RportNotEchoed { .. } => "rport not echoed",
        Evidence::CapabilitiesNotAdvertised { .. } => "allow/supported not advertised",
        Evidence::ExtraTryingForwarded { .. } => "downstream 100 forwarded",
        Evidence::UnknownDialogRequest { .. } => "in-dialog request for an unknown dialog",
        Evidence::RejectionNotIssued { .. } => "unrecognised method or extension served",
        Evidence::ResponseHeadersMissing { .. } => "response carries none of the headers it owes",
        Evidence::NoTargetFinal { .. } => "error final without forwarding",
        Evidence::AckRequireNotSubset { .. } => "ack requires more than its invite",
        Evidence::AckRouteDiverged { .. } => "ack route diverged",
        Evidence::StrictRouteNotRewritten { .. } => "strict route not rewritten on forward",
        Evidence::RegisterCarriesRoute { .. } => "register carries a route set",
        Evidence::ConcurrentRegister { .. } => "register raced an unanswered one",
        Evidence::ConcurrentReInvite { .. } => "re-invite raced a pending one",
        Evidence::ByeOffDialog { early_dialog: false, .. } => "bye names no dialog",
        Evidence::ByeOffDialog { early_dialog: true, .. } => "callee byed an early dialog",
        Evidence::OverlappingReInvite { .. } => "re-invite overlapped its own prior one",
        Evidence::TryingNotSentInGrace { .. } => "no 100 trying inside the grace",
        Evidence::UnackedReject { .. } => "non-2xx final never acked",
        Evidence::AbandonedReInvite { .. } => "re-invite drew a provisional and no final",
        Evidence::LateProvisional { .. } => "provisional after the final",
        Evidence::Unreliable1xx { .. } => "provisional not sent reliably",
        Evidence::UnsolicitedReliable1xx { .. } => "reliable provisional without opt-in",
        Evidence::InDialogReliable1xx { .. } => "reliable provisional on an in-dialog request",
        Evidence::PrackAbsorbed { .. } => "unmatched prack absorbed",
        Evidence::PrackAnsweredWrongly { .. } => "prack answered off its rseq state",
        Evidence::AnsweredOverUnackedOffer { .. } => "2xx over an unacked reliable offer",
        Evidence::LatePrackRejected { .. } => "late prack rejected",
        Evidence::Reliable1xxAfterFinal { .. } => "new reliable provisional after the final",
        Evidence::PrackedTrying { .. } => "prack of a 100 trying",
        Evidence::PrackWithoutAnswer { .. } => "prack of an offer carries no answer",
        Evidence::AckBodyOnClosedRound { .. } => "ack body on a completed round",
        Evidence::OfferLeftUnanswered { .. } => "offer left unanswered",
        Evidence::AnswerStreamRetyped { .. } => "answer re-typed an offered stream",
        Evidence::SdpOriginDiverged { same_session: false, .. } => "sdp origin names another session",
        Evidence::SdpOriginDiverged { same_session: true, .. } => "sdp sess-version off its changes",
        Evidence::OfferWhilePending { .. } => "offer over an unanswered one",
        Evidence::AnswerMLineCountDiffers { .. } => "answer stream count off the offer's",
        Evidence::AnswerTLineDiffers { .. } => "answer t= line off the offer's",
        Evidence::AnswerMediaTypeMismatched { .. } => "answer media type off the offer's",
        Evidence::DirectionPairInvalid { .. } => "answer direction the offer's does not admit",
        Evidence::RejectedStreamWithoutFormat { .. } => "rejected stream lists no format",
        Evidence::ReOfferStreamsDropped { .. } => "re-offer dropped a stream slot",
        Evidence::ZeroPortResurrected { .. } => "answer revived a disabled stream",
        Evidence::PayloadTypeRemapped { .. } => "payload type rebound mid-call",
        Evidence::RequiredHeaderAbsent { .. } => "a header the message owed is absent",
        Evidence::ForbiddenHeaderPresent { .. } => "a header the message may not state",
        Evidence::HeaderValueRejected { .. } => "a header value no reader accepts",
        Evidence::ResponseViaDiverged { .. } => "response via stack off its request's",
        Evidence::ResponseCseqPhantom { .. } => "response cseq names no sent request",
        Evidence::DialogTagForeign { .. } => "in-dialog tag the taker never minted",
        Evidence::PeerUriRewritten { .. } => "peer uri rewritten mid-dialog",
        Evidence::DialogCallIdChanged { .. } => "call-id changed mid-dialog",
        Evidence::CancelUriDiverged { .. } => "cancel uri off its invite's",
        Evidence::CancelBranchUnmatched { .. } => "cancel branch names no open invite",
        Evidence::UasTagFlipped { .. } => "uas to-tag flipped on its transaction",
        Evidence::Reliable1xx { .. } => "1xx reliability markers do not add up",
        Evidence::SdpBodyRejected { .. } => "sdp body no reader accepts",
        Evidence::HeldAndRejectedStreams { .. } => "stream held and rejected at once",
        // Which retransmitting class re-composed its rung — the measure a reader
        // weighs a divergence by, since each class is one emitter code path.
        Evidence::RungDiverged { class, .. } => {
            RungClass::from_label(class).map_or("rung diverged", RungClass::label)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;

    /// A census reports a rule with no hits as zero rather than omitting it,
    /// and merging two sweeps adds up.
    #[test]
    fn a_census_reports_every_rule_and_merges() {
        let clean = doc_of(vec![
            dg(1_000, A, B, request("INVITE", 1, "m1", "fa", None)),
            dg(4_000, B, A, response(200, "OK", 1, "INVITE", "m1", "fa", Some("tb"))),
            dg(4_500, A, B, request("ACK", 1, "m1", "fa", Some("tb"))),
        ]);
        let dirty = doc_of(vec![
            dg(1_000, A, B, request("INVITE", 1, "m2", "fa", None)),
            dg(4_000, A, B, request("CANCEL", 1, "m2", "fa", None)),
            dg(5_000, B, A, response(200, "OK", 1, "INVITE", "m2", "fa", Some("tb"))),
            dg(5_500, A, B, request("ACK", 1, "m2", "fa", Some("tb"))),
        ]);

        let mut first = Census::new();
        first.absorb("a.json", "cap-a", &clean);
        assert_eq!(first.rules["no-200-after-cancel"].hits, 0, "reported, not omitted");
        assert_eq!(
            first.rules["unacked-reliable-provisional"].hits, 0,
            "a rule this corpus never fires is still a reported result"
        );
        assert_eq!(first.rules["no-ack-to-dialog-creating-2xx"].hits, 0);

        let mut second = Census::new();
        second.absorb("b.json", "cap-b", &dirty);
        second.fail("c.json", "not JSON".to_string());

        first.merge(second);
        first.sort();
        assert_eq!(first.documents, 2);
        let tally = &first.rules["no-200-after-cancel"];
        assert_eq!((tally.hits, tally.documents), (1, 1));
        assert_eq!(tally.by_role["undetermined"], 1, "a single-leg call attributes neither side");
        assert_eq!(tally.buckets["gap <10ms"], 1);
        assert_eq!(first.hits[0].capture, "cap-b");
        assert_eq!(first.failures.len(), 1);
        assert!(first.summary().contains("no-200-after-cancel: 1 hit(s)"));
        assert!(first.summary().contains("unacked-reliable-provisional: 0 hit(s)"));
    }

    /// Three rules in one document are tallied apart and all reach the report.
    #[test]
    fn every_rule_in_one_document_is_tallied_separately() {
        let both = doc_of(vec![
            // The PRACK rule's shape: 100rel offered, a reliable 180 taken,
            // never PRACKed, and the dialog lives on for seconds.
            dg(1_000_000, A, B, request_hdr("INVITE", 1, "x1", "fa", None, "Supported: 100rel\r\n")),
            dg(
                1_200_000,
                B,
                A,
                response_hdr(
                    180,
                    "Ringing",
                    1,
                    "INVITE",
                    "x1",
                    "fa",
                    Some("tb"),
                    "Require: 100rel\r\nRSeq: 1\r\n",
                ),
            ),
            // The cancel rule's shape, on the same transaction.
            dg(9_000_000, A, B, request("CANCEL", 1, "x1", "fa", None)),
            dg(9_100_000, B, A, response(200, "OK", 1, "CANCEL", "x1", "fa", Some("tb"))),
            dg(9_200_000, B, A, response(200, "OK", 1, "INVITE", "x1", "fa", Some("tb"))),
            // The ACK rule's shape: nobody ever confirms that dialog, and the
            // far side gives up 32 s later exactly as RFC 3261 §13.3.1.4 says.
            dg(41_200_000, B, A, request("BYE", 9, "x1", "tb", Some("fa"))),
            dg(41_300_000, A, B, response(200, "OK", 9, "BYE", "x1", "tb", Some("fa"))),
        ]);
        let mut census = Census::new();
        census.absorb("both.json", "cap-both", &both);
        census.sort();
        assert_eq!(census.rules["no-200-after-cancel"].hits, 1);
        assert_eq!(census.rules["unacked-reliable-provisional"].hits, 1);
        assert_eq!(census.rules["unacked-reliable-provisional"].buckets["window <10s"], 1);
        assert_eq!(census.rules["no-ack-to-dialog-creating-2xx"].hits, 1);
        assert_eq!(census.rules["no-ack-to-dialog-creating-2xx"].buckets["torn down un-ACKed"], 1);
        assert_eq!(census.hits.len(), 3);
        let json = serde_json::to_value(&census.hits[1]).unwrap();
        assert_eq!(json["rule"], "unacked-reliable-provisional");
        assert_eq!(json["emitter"], A, "the UAC that owed the PRACK");
        assert_eq!(json["rseq"], 1);
        let ack = serde_json::to_value(&census.hits[2]).unwrap();
        assert_eq!(ack["rule"], "no-ack-to-dialog-creating-2xx");
        assert_eq!(ack["emitter"], A, "the UAC that owed the ACK");
        assert_eq!(ack["to_tag"], "tb");
        assert_eq!(ack["bye_by"], B);
    }

    /// A candidate rule is run only where the sweep names it: present at zero
    /// like a WIRE rule, tallied under its own token and bucketed by its own
    /// measure — and absent from a census that did not name it, so the WIRE
    /// vocabulary a report decodes through never grows by being counted.
    #[test]
    fn a_named_candidate_is_tallied_and_an_unnamed_one_is_not() {
        let ok = response(200, "OK", 1, "INVITE", "r1", "fa", Some("tb"));
        let recomposed = response_hdr(200, "OK", 1, "INVITE", "r1", "fa", Some("tb"), "Server: x\r\n");
        let doc = doc_of(vec![
            dg(1_000_000, A, B, request("INVITE", 1, "r1", "fa", None)),
            dg(1_100_000, B, A, ok),
            dg(1_600_000, B, A, recomposed),
            dg(1_700_000, A, B, request("ACK", 1, "r1", "fa", Some("tb"))),
        ]);
        let mut named = Census::with_candidates(&[RfcRule::RungByteIdentical]);
        named.absorb("r.json", "cap-r", &doc);
        let tally = &named.rules["rung-byte-identical"];
        assert_eq!((tally.occasions, tally.decided, tally.hits), (1, 1, 1));
        assert_eq!(tally.buckets["2xx final"], 1);
        let hit = serde_json::to_value(&named.hits[0]).unwrap();
        assert_eq!(hit["rule"], "rung-byte-identical");
        assert_eq!(hit["emitter"], B, "the UAS that re-composed its rung");
        assert_eq!(hit["region"], "head");
        assert!(named.summary().contains("rung-byte-identical: 1 hit(s)"));

        let mut plain = Census::new();
        plain.absorb("r.json", "cap-r", &doc);
        assert!(!plain.rules.contains_key("rung-byte-identical"), "{:?}", plain.rules.keys());
        assert!(plain.hits.is_empty());
        let mut zero = Census::with_candidates(&[RfcRule::RungByteIdentical]);
        zero.absorb("clean.json", "cap-c", &doc_of(vec![
            dg(1_000_000, A, B, request("INVITE", 1, "r2", "fa", None)),
            dg(1_100_000, B, A, response(200, "OK", 1, "INVITE", "r2", "fa", Some("tb"))),
            dg(1_200_000, A, B, request("ACK", 1, "r2", "fa", Some("tb"))),
        ]));
        assert_eq!(zero.rules["rung-byte-identical"].hits, 0, "reported, not omitted");
    }
}
