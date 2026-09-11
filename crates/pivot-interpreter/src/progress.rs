//! Whether a run can **go on** past a failure it has just recorded
//! (`PCAP2TEST_PIVOT_V3.md` §11.2).
//!
//! An error is not by itself the end of a script. What ends a script is an error
//! that leaves the run unable to compose its next message: a message the flow
//! waits for that can never arrive. Everything else — a datagram nothing
//! expected, one no armed expect matched while the expected one is still coming
//! — is recorded as the failure it is and the flow carries on, so the verdict
//! sees the whole divergence instead of only its first packet.
//!
//! **Only a FINAL response blocks, and only its own transaction's expects.**
//! RFC 3261 §17.1 ends a client transaction at its final response: after it, no
//! response of a different status rides that transaction again. So a final makes
//! an armed expect UNSATISFIABLE exactly when the expect waits for a different
//! status on the very transaction the final answered. A provisional ends
//! nothing, a request ends nothing, and a final for another transaction ends
//! nothing.
//!
//! **All the alternatives, or none.** `alt` branches and `unordered` members arm
//! several expects on one leg at once, and the run can still go on while any one
//! of them can still be satisfied — so a leg blocks only when the arrival
//! contradicts EVERY required expect armed on it. An `optional` expect is never
//! what blocks: a tolerated absence is RELEASED at its budget (§6.5) and the
//! flow moves on without it.
//!
//! Read off the leg's own RECORDING, like [`close`](crate::close): the
//! transaction an expect waits on is the request the leg last SENT with that
//! method, which is a fact about the wire rather than about the document.

use pivot_schema::bundle::{Dir, RecordedMessage};
use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipMessage, SipParser};

use crate::gate::Inbound;
use crate::plan::{CompiledStep, Discriminator};

/// Whether `inbound` — an arrival every armed expect on its leg refused —
/// leaves the run unable to GO ON.
///
/// `armed` is every expect the cursor has armed on that leg; `leg` is that leg's
/// recording so far, in wire order.
pub fn blocks(inbound: &Inbound, armed: &[CompiledStep], leg: &[RecordedMessage]) -> bool {
    // Only a final response ends a transaction (RFC 3261 §17.1). A request and a
    // provisional leave every armed expect exactly as satisfiable as it was.
    let Some(status) = inbound.status.filter(|status| *status >= 200) else {
        return false;
    };
    // A tolerated absence is released, not failed, so it can never be what wedges
    // a leg: only the expects the document REQUIRES can block.
    let mut required = armed.iter().filter(|step| !step.optional_expect()).peekable();
    if required.peek().is_none() {
        return false;
    }
    let sent = last_sent(leg, &inbound.cseq_method);
    required.all(|step| contradicts(step, status, inbound, sent))
}

/// Whether a final response leaves one armed expect unsatisfiable.
///
/// Three things must hold: the expect waits for a RESPONSE naming the method of
/// the transaction this final answered, that transaction is the one the leg has
/// outstanding for the method, and the status it waits for is not the status
/// that arrived. A same-status arrival contradicts nothing — a forking INVITE
/// draws a 2xx per answered fork (§13.2.2.4) and a final is retransmitted — so
/// what refused it was content or an early dialog, and another copy can still
/// satisfy the step.
fn contradicts(step: &CompiledStep, status: u16, inbound: &Inbound, sent: Option<u32>) -> bool {
    // An expect gated on a status alone names no transaction, so nothing about
    // one transaction ending makes it unsatisfiable.
    let Discriminator::Response { status: want, cseq_method: Some(method) } = &step.discriminator
    else {
        return false;
    };
    if Method::from_wire(method) != Method::from_wire(&inbound.cseq_method) {
        return false;
    }
    sent == Some(inbound.cseq) && *want != status
}

/// The CSeq of the last request with `method` this leg SENT, where it sent one.
///
/// A leg waiting for a response is the client of that transaction, so its own
/// emission is what identifies the transaction the expect behind it waits on —
/// and a re-INVITE's CSeq is how a final for the original is told from a final
/// for the renegotiation.
fn last_sent(leg: &[RecordedMessage], method: &str) -> Option<u32> {
    let want = Method::from_wire(method);
    leg.iter()
        .filter(|recorded| recorded.dir == Dir::Out)
        .filter_map(|recorded| match CustomParser::new().parse(recorded.raw.as_bytes()).ok()? {
            SipMessage::Request(request) if *request.method() == want => Some(request.cseq().seq()),
            _ => None,
        })
        .next_back()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{CompiledStep, StepKind};
    use crate::program::StepLoc;
    use crate::recording::Recording;
    use pivot_schema::flow::{Anchor, CheckMode, Delay};
    use pivot_schema::msg::MsgSpec;

    const INVITE: &str = "INVITE sip:bob@127.0.0.1:5080 SIP/2.0\r\n\
        Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-a\r\n\
        From: <sip:alice@example.test>;tag=a1\r\n\
        To: <sip:bob@example.test>\r\n\
        Call-ID: call-a\r\n\
        CSeq: 1 INVITE\r\n\
        Content-Length: 0\r\n\r\n";

    /// A leg's recorded ladder, from `(dir, raw)` pairs in wire order.
    fn ladder(entries: &[(Dir, String)]) -> Vec<RecordedMessage> {
        let recording = Recording::new();
        recording.declare("A");
        for (at, (dir, raw)) in entries.iter().enumerate() {
            recording.push("A", *dir, at as u64, raw.clone(), None, None);
        }
        recording.legs().remove("A").expect("the leg was recorded")
    }

    /// One arriving datagram, projected as the gate projects it.
    fn arrival(raw: &str) -> Inbound {
        let message = CustomParser::new().parse(raw.as_bytes()).expect("parses");
        Inbound::of(&message)
    }

    fn response(status: u16, cseq: &str) -> String {
        format!(
            "SIP/2.0 {status} Whatever\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-a\r\n\
             From: <sip:alice@example.test>;tag=a1\r\n\
             To: <sip:bob@example.test>;tag=b1\r\n\
             Call-ID: call-a\r\n\
             CSeq: {cseq}\r\n\
             Content-Length: 0\r\n\r\n"
        )
    }

    fn request(method: &str, cseq: &str) -> String {
        format!(
            "{method} sip:alice@127.0.0.1:5060 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-x\r\n\
             From: <sip:bob@example.test>;tag=b1\r\n\
             To: <sip:alice@example.test>;tag=a1\r\n\
             Call-ID: call-a\r\n\
             CSeq: {cseq}\r\n\
             Content-Length: 0\r\n\r\n"
        )
    }

    /// An expect on leg A, armed on `discriminator`.
    fn armed(id: &str, discriminator: Discriminator, optional: bool) -> CompiledStep {
        CompiledStep {
            id: id.into(),
            leg: "A".into(),
            kind: StepKind::Expect { check: CheckMode::Record, optional },
            auto: false,
            after: Vec::new(),
            early: None,
            overlap: None,
            discriminator,
            msg: MsgSpec::default(),
            checks: Vec::new(),
            delay: Delay { ms: 0, from: Anchor::Trigger, compressible: false, timer_linked: false },
            within_ms: 1_000,
            retransmits: None,
            retransmit_intervals_ms: Vec::new(),
            loc: StepLoc { item: 0, branch: None, within: 0 },
            order: 0,
            deviations: Vec::new(),
        }
    }

    /// An expect gated on a response.
    fn expects(id: &str, status: u16, cseq_method: Option<&str>, optional: bool) -> CompiledStep {
        let discriminator =
            Discriminator::Response { status, cseq_method: cseq_method.map(str::to_string) };
        armed(id, discriminator, optional)
    }

    /// An expect gated on a request method.
    fn expects_request(id: &str, method: &str) -> CompiledStep {
        armed(id, Discriminator::Request { method: method.into() }, false)
    }

    /// The caller sent one INVITE, and a reject answered it: the 200 the script
    /// waits for can never come, so the run cannot go on.
    #[test]
    fn a_reject_blocks_the_expect_of_the_answer_on_the_same_invite() {
        let leg = ladder(&[(Dir::Out, INVITE.to_string())]);
        let busy = arrival(&response(486, "1 INVITE"));
        assert!(blocks(&busy, &[expects("s7", 200, Some("INVITE"), false)], &leg));
    }

    /// A PROVISIONAL ends no transaction: the 200 is still coming, so the run
    /// records the refusal and carries on.
    #[test]
    fn a_provisional_that_no_expect_matched_leaves_the_run_able_to_go_on() {
        let leg = ladder(&[(Dir::Out, INVITE.to_string())]);
        let ringing = arrival(&response(183, "1 INVITE"));
        assert!(!blocks(&ringing, &[expects("s7", 200, Some("INVITE"), false)], &leg));
    }

    /// A REQUEST nothing matched ends no transaction either — the platform's own
    /// ACK to a 2xx is the case this program meets most.
    #[test]
    fn an_arriving_request_never_blocks_an_expect() {
        let leg = ladder(&[(Dir::Out, INVITE.to_string())]);
        let ack = arrival(&request("ACK", "1 ACK"));
        assert!(!blocks(&ack, &[expects_request("s10", "BYE")], &leg));
        assert!(!blocks(&ack, &[expects("s7", 200, Some("INVITE"), false)], &leg));
    }

    /// A REQUEST expect armed BESIDE the contradicted response expect keeps the
    /// run going: no final ends the transaction a request expect waits on, so
    /// one satisfiable alternative remains and the leg is not wedged — its own
    /// budget ends the script if the request truly never comes.
    #[test]
    fn a_request_expect_beside_the_contradicted_one_keeps_the_run_going() {
        let leg = ladder(&[(Dir::Out, INVITE.to_string())]);
        let busy = arrival(&response(486, "1 INVITE"));
        let answer = expects("s7", 200, Some("INVITE"), false);
        assert!(blocks(&busy, std::slice::from_ref(&answer), &leg), "alone, the reject wedges it");
        assert!(
            !blocks(&busy, &[answer, expects_request("s10", "BYE")], &leg),
            "with a request expect still armed, the run goes on"
        );
    }

    /// The SAME status contradicts nothing: what refused it was content or an
    /// early dialog, and a fork's own 2xx or a retransmission can still satisfy
    /// the step.
    #[test]
    fn a_final_of_the_status_the_step_waits_for_never_blocks_it() {
        let leg = ladder(&[(Dir::Out, INVITE.to_string())]);
        let ok = arrival(&response(200, "1 INVITE"));
        assert!(!blocks(&ok, &[expects("s7", 200, Some("INVITE"), false)], &leg));
    }

    /// A final for ANOTHER transaction leaves this one alone: the re-INVITE's
    /// reject says nothing about the original INVITE's answer.
    #[test]
    fn a_final_for_another_transaction_of_the_same_method_blocks_nothing() {
        let leg = ladder(&[
            (Dir::Out, INVITE.to_string()),
            (Dir::In, response(200, "1 INVITE")),
            (Dir::Out, request("ACK", "1 ACK")),
            (Dir::Out, INVITE.replace("CSeq: 1 INVITE", "CSeq: 2 INVITE")),
        ]);
        // The leg's outstanding INVITE is CSeq 2, so a final for CSeq 1 — a late
        // copy of the original's — contradicts nothing armed now.
        let stale = arrival(&response(486, "1 INVITE"));
        assert!(!blocks(&stale, &[expects("s9", 200, Some("INVITE"), false)], &leg));
        // The re-INVITE's own reject does block it.
        let refused = arrival(&response(488, "2 INVITE"));
        assert!(blocks(&refused, &[expects("s9", 200, Some("INVITE"), false)], &leg));
    }

    /// A final for a transaction this leg never opened blocks nothing: there is
    /// no expect of ours behind it.
    #[test]
    fn a_final_for_a_request_this_leg_never_sent_blocks_nothing() {
        let leg = ladder(&[(Dir::Out, INVITE.to_string())]);
        let bye_ok = arrival(&response(200, "2 BYE"));
        assert!(!blocks(&bye_ok, &[expects("s7", 481, Some("BYE"), false)], &leg));
    }

    /// `alt` arms one expect per branch, and the run can go on while ANY of them
    /// can still be satisfied. A 486 contradicts the branch waiting for 200 and
    /// not the branch waiting for a BYE, so it does not block; a 603 contradicts
    /// both response branches and does.
    #[test]
    fn an_alt_blocks_only_when_every_armed_branch_is_contradicted() {
        let leg = ladder(&[(Dir::Out, INVITE.to_string())]);
        let busy = arrival(&response(486, "1 INVITE"));
        let mixed = [expects("s7", 200, Some("INVITE"), false), expects_request("s8", "BYE")];
        assert!(!blocks(&busy, &mixed, &leg));

        let both =
            [expects("s7", 200, Some("INVITE"), false), expects("s8", 486, Some("INVITE"), false)];
        assert!(!blocks(&busy, &both, &leg), "one branch is the very status that arrived");
        let declined = arrival(&response(603, "1 INVITE"));
        assert!(blocks(&declined, &both, &leg), "neither branch can be satisfied now");
    }

    /// An `optional` expect is released at its budget, so a leg holding only
    /// tolerated absences is never blocked — the flow moves on without them.
    #[test]
    fn an_optional_expect_is_never_what_blocks_a_leg() {
        let leg = ladder(&[(Dir::Out, INVITE.to_string())]);
        let busy = arrival(&response(486, "1 INVITE"));
        assert!(!blocks(&busy, &[expects("s5", 180, Some("INVITE"), true)], &leg));
        // Beside a REQUIRED expect the same final contradicts, the required one
        // decides.
        let pair =
            [expects("s5", 180, Some("INVITE"), true), expects("s7", 200, Some("INVITE"), false)];
        assert!(blocks(&busy, &pair, &leg));
    }

    /// An expect gated on a status ALONE names no transaction, so no transaction
    /// ending can make it unsatisfiable.
    #[test]
    fn an_expect_naming_no_cseq_method_is_never_contradicted() {
        let leg = ladder(&[(Dir::Out, INVITE.to_string())]);
        let busy = arrival(&response(486, "1 INVITE"));
        assert!(!blocks(&busy, &[expects("s7", 200, None, false)], &leg));
    }

    /// No armed expect at all is a datagram nothing was waiting for: it fails the
    /// run and stops nothing.
    #[test]
    fn an_arrival_with_nothing_armed_blocks_nothing() {
        let leg = ladder(&[(Dir::Out, INVITE.to_string())]);
        let busy = arrival(&response(486, "1 INVITE"));
        assert!(!blocks(&busy, &[], &leg));
    }
}
