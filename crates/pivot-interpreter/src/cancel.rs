//! The answer to a CANCEL sent after its INVITE's final
//! (`PCAP2TEST_PIVOT_V3.md` §6.7d).
//!
//! A UAS keeps its INVITE server transaction in Completed after a non-2xx final
//! until the ACK or Timer H (RFC 3261 §17.2.1). A CANCEL matching a transaction
//! draws 200 whatever that transaction's state; one matching none draws 481
//! (§9.2). So a CANCEL a leg sends AFTER a final to its INVITE reached it draws
//! 200 or 481 by how long the UAS holds the transaction, both conformant, and
//! an expect naming either is satisfied by the other. The fact is read off the
//! leg's recording — this run's wire — never off the document's list order.

use pivot_schema::bundle::{Dir, RecordedMessage};
use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipMessage, SipParser};

use crate::gate::Inbound;
use crate::plan::{CompiledStep, Discriminator};

/// The two finals a late CANCEL draws, one per side of the transaction's life.
const EITHER: [u16; 2] = [200, 481];

/// Whether `inbound` answers `step` as the other final a late CANCEL draws: the
/// step is an expect naming 200 or 481 to CANCEL, the arrival is the other of
/// the two on the CANCEL transaction the step waits on, and `leg` records a
/// final to the INVITE of the same CSeq number arriving BEFORE that CANCEL was
/// sent. The CANCEL is the one sent under `opener` (the step's opening send,
/// `Cursor::opening_send`), else the leg's last CANCEL sent — the transaction
/// `progress` charges the step on.
pub fn answers_late_cancel(
    step: &CompiledStep,
    inbound: &Inbound,
    leg: &[RecordedMessage],
    opener: Option<&str>,
) -> bool {
    let Discriminator::Response { status: named, cseq_method: Some(method) } = &step.discriminator
    else {
        return false;
    };
    let Some(arrived) = inbound.status else { return false };
    if !step.is_expect()
        || Method::from_wire(method) != Method::Cancel
        || Method::from_wire(&inbound.cseq_method) != Method::Cancel
        || named == &arrived
        || !EITHER.contains(named)
        || !EITHER.contains(&arrived)
    {
        return false;
    }
    let Some((at, cseq)) = cancel_sent(leg, opener) else { return false };
    cseq == inbound.cseq
        && leg[..at].iter().any(|recorded| {
            recorded.dir == Dir::In
                && matches!(parse(recorded), Some(SipMessage::Response(r))
                    if r.status() >= 200
                        && *r.cseq().method() == Method::Invite
                        && r.cseq().seq() == cseq)
        })
}

/// The recording note a tolerated late-CANCEL final carries.
pub fn note(named: u16, arrived: u16) -> String {
    format!(
        "tolerated: a final to a CANCEL sent after the INVITE's final arrived on this leg draws \
         200 while the server transaction lives and 481 once it is gone (RFC 3261 §9.2, \
         §17.2.1); {arrived} arrived where the step names {named}"
    )
}

/// Where the CANCEL the step waits on stands on the leg, and its CSeq number:
/// the first CANCEL recorded under `opener`, else the leg's last CANCEL sent.
fn cancel_sent(leg: &[RecordedMessage], opener: Option<&str>) -> Option<(usize, u32)> {
    let mut cancels =
        leg.iter().enumerate().filter(|(_, recorded)| recorded.dir == Dir::Out).filter_map(
            |(at, recorded)| match parse(recorded)? {
                SipMessage::Request(r) if *r.method() == Method::Cancel => {
                    Some((at, r.cseq().seq(), recorded.step.as_deref()))
                }
                _ => None,
            },
        );
    let found = match opener {
        Some(opener) => cancels.find(|(_, _, step)| *step == Some(opener)),
        None => cancels.next_back(),
    };
    found.map(|(at, cseq, _)| (at, cseq))
}

fn parse(recorded: &RecordedMessage) -> Option<SipMessage> {
    CustomParser::new().parse(recorded.wire()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::StepKind;
    use crate::program::StepLoc;
    use crate::recording::Recording;
    use pivot_schema::flow::{Anchor, CheckMode, Delay};
    use pivot_schema::msg::MsgSpec;

    fn request(method: &str, cseq: u32) -> String {
        format!(
            "{method} sip:bob@127.0.0.1:5080 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-a\r\n\
             From: <sip:alice@example.test>;tag=a1\r\n\
             To: <sip:bob@example.test>\r\n\
             Call-ID: call-a\r\n\
             CSeq: {cseq} {method}\r\n\
             Content-Length: 0\r\n\r\n"
        )
    }

    fn response(status: u16, cseq: u32, method: &str) -> String {
        format!(
            "SIP/2.0 {status} Whatever\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-a\r\n\
             From: <sip:alice@example.test>;tag=a1\r\n\
             To: <sip:bob@example.test>;tag=b1\r\n\
             Call-ID: call-a\r\n\
             CSeq: {cseq} {method}\r\n\
             Content-Length: 0\r\n\r\n"
        )
    }

    /// Leg A's recording, each datagram under the step that claimed it.
    fn ladder(entries: &[(Dir, String, Option<&str>)]) -> Vec<RecordedMessage> {
        let recording = Recording::new();
        recording.declare("A");
        for (at, (dir, raw, step)) in entries.iter().enumerate() {
            recording.push("A", *dir, at as u64, raw.clone(), *step, None);
        }
        recording.legs().remove("A").expect("the leg was recorded")
    }

    fn arrival(raw: &str) -> Inbound {
        Inbound::of(&CustomParser::new().parse(raw.as_bytes()).expect("parses"))
    }

    fn expects(status: u16, cseq_method: Option<&str>) -> CompiledStep {
        CompiledStep {
            id: "s8".into(),
            leg: "A".into(),
            kind: StepKind::Expect { check: CheckMode::Record, optional: false },
            auto: false,
            after: Vec::new(),
            early: None,
            overlap: None,
            discriminator: Discriminator::Response {
                status,
                cseq_method: cseq_method.map(str::to_string),
            },
            msg: MsgSpec::default(),
            checks: Vec::new(),
            delay: Delay { ms: 0, from: Anchor::Trigger, compressible: false, timer_linked: false },
            within_ms: 1_000,
            retransmits: None,
            retransmit_intervals_ms: Vec::new(),
            loc: StepLoc { item: 0, branch: None, within: 0 },
            order: 8,
            deviations: Vec::new(),
        }
    }

    /// INVITE, its non-2xx final, then the CANCEL under s7.
    fn late() -> Vec<RecordedMessage> {
        ladder(&[
            (Dir::Out, request("INVITE", 1), Some("s1")),
            (Dir::In, response(100, 1, "INVITE"), Some("s2")),
            (Dir::In, response(487, 1, "INVITE"), Some("s6")),
            (Dir::Out, request("CANCEL", 1), Some("s7")),
        ])
    }

    #[test]
    fn either_final_answers_a_cancel_sent_after_its_invite_s_final() {
        let leg = late();
        let ok = arrival(&response(200, 1, "CANCEL"));
        let gone = arrival(&response(481, 1, "CANCEL"));
        assert!(answers_late_cancel(&expects(481, Some("CANCEL")), &ok, &leg, Some("s7")));
        assert!(answers_late_cancel(&expects(200, Some("CANCEL")), &gone, &leg, Some("s7")));
        assert!(answers_late_cancel(&expects(481, Some("CANCEL")), &ok, &leg, None));
    }

    #[test]
    fn only_the_pair_200_481_is_tolerated() {
        let leg = late();
        let terminated = arrival(&response(487, 1, "CANCEL"));
        let busy = arrival(&response(486, 1, "CANCEL"));
        assert!(!answers_late_cancel(&expects(481, Some("CANCEL")), &terminated, &leg, Some("s7")));
        assert!(!answers_late_cancel(&expects(200, Some("CANCEL")), &busy, &leg, Some("s7")));
        let ok = arrival(&response(200, 1, "CANCEL"));
        assert!(!answers_late_cancel(&expects(487, Some("CANCEL")), &ok, &leg, Some("s7")));
        assert!(
            !answers_late_cancel(&expects(200, Some("CANCEL")), &ok, &leg, Some("s7")),
            "the named status is the discriminator's own match"
        );
    }

    #[test]
    fn a_step_naming_no_cancel_transaction_gets_nothing() {
        let leg = late();
        let ok = arrival(&response(200, 1, "CANCEL"));
        assert!(!answers_late_cancel(&expects(481, None), &ok, &leg, None));
        assert!(!answers_late_cancel(&expects(481, Some("BYE")), &ok, &leg, None));
        let bye = arrival(&response(200, 1, "BYE"));
        assert!(!answers_late_cancel(&expects(481, Some("CANCEL")), &bye, &leg, Some("s7")));
    }

    /// A CANCEL sent while the INVITE was still unanswered is the ordinary
    /// race of RFC 3261 §9.1: a 481 there is a finding.
    #[test]
    fn a_cancel_sent_before_the_invite_s_final_gets_nothing() {
        let leg = ladder(&[
            (Dir::Out, request("INVITE", 1), Some("s1")),
            (Dir::In, response(100, 1, "INVITE"), Some("s2")),
            (Dir::Out, request("CANCEL", 1), Some("s7")),
            (Dir::In, response(487, 1, "INVITE"), Some("s9")),
        ]);
        let ok = arrival(&response(200, 1, "CANCEL"));
        assert!(!answers_late_cancel(&expects(481, Some("CANCEL")), &ok, &leg, Some("s7")));
    }

    #[test]
    fn a_provisional_before_the_cancel_does_not_count() {
        let leg = ladder(&[
            (Dir::Out, request("INVITE", 1), Some("s1")),
            (Dir::In, response(180, 1, "INVITE"), Some("s5")),
            (Dir::Out, request("CANCEL", 1), Some("s7")),
        ]);
        let gone = arrival(&response(481, 1, "CANCEL"));
        assert!(!answers_late_cancel(&expects(200, Some("CANCEL")), &gone, &leg, Some("s7")));
    }

    /// The INVITE final must be for the CSeq number the CANCEL carries, and the
    /// arrival must ride that CANCEL's transaction.
    #[test]
    fn the_final_and_the_arrival_are_on_the_cancel_s_own_cseq() {
        let leg = ladder(&[
            (Dir::Out, request("INVITE", 1), Some("s1")),
            (Dir::In, response(486, 1, "INVITE"), Some("s3")),
            (Dir::Out, request("INVITE", 2), Some("s5")),
            (Dir::Out, request("CANCEL", 2), Some("s7")),
        ]);
        let ok = arrival(&response(200, 2, "CANCEL"));
        assert!(!answers_late_cancel(&expects(481, Some("CANCEL")), &ok, &leg, Some("s7")));
        let other = arrival(&response(200, 2, "CANCEL"));
        assert!(!answers_late_cancel(&expects(481, Some("CANCEL")), &other, &late(), Some("s7")));
    }

    #[test]
    fn the_note_names_both_statuses() {
        assert!(note(481, 200).ends_with("; 200 arrived where the step names 481"));
    }
}
