//! The `retransmits` COUNT (`PCAP2TEST_PIVOT_V3.md` §6.9): the repeats of one
//! message, beyond the first, that a `send` step emits and an `expect` step
//! tolerates and counts.
//!
//! A captured retransmission is not a step — it collapses onto the step it
//! repeats, as a count. Four things follow, and this module is all of them:
//!
//! - **Pacing.** The document states HOW MANY repeats a step carries and,
//!   where the capture measured them, the gaps between them (§6.9). Where it
//!   states none, a scripted peer paces its ladder the way RFC 3261 does —
//!   Timer A for an INVITE (§17.1.1.2), Timer E for another request
//!   (§17.1.2.2), §13.3.1.4 for a 2xx, §17.2.1 for another INVITE final, RFC
//!   3262 §3 for a reliable provisional — on the one [`sip_retransmit::Schedule`]
//!   the SUT rides (ADR-0032), so the oracle and the thing it confronts cannot
//!   disagree about a schedule. A message with no ladder of its own is REFUSED
//!   by name rather than paced by a default nobody wrote down (§14).
//! - **Drawing.** One ladder is neither paced nor refused: an auto ACK step's
//!   count is DRAWN by the wire (§6.3). The UAC core sends one ACK per 2xx it
//!   receives (RFC 3261 §13.2.2.4), so the count is measured against the
//!   repeats of the final that transaction drew, and [`DrawnAcks`] holds the
//!   pairing on [`scenario_harness::absorption`]'s own key.
//! - **Counting.** A repeat's UNIT is the datagram byte-identical to the one the
//!   step already claimed, on the same leg. Whether a repeat ever reaches the
//!   transaction user is [`scenario_harness::absorption`]'s call and never
//!   re-derived here; this module only asks whether a step claimed those bytes
//!   and how many times they came back.
//! - **Asserting.** A count states how long a ladder is, and WHO says so is the
//!   pacer's, not the document's. Where our own peer paced it the document is
//!   the instruction and holds by construction; where the SUT paced it the
//!   document holds a foreign platform's T1, so the oracle is the RFC —
//!   [`Repeats::owed`] states the four regimes and the assertion is the same on
//!   every lane. A lane whose SUT paces differently is a FINDING; where the
//!   difference is one the project accepts, that is a tolerance and it lives in
//!   the rule registry, never in here.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use pivot_schema::bundle::{Failure, LadderSide, RetransmitNote};
use scenario_harness::absorption::AckKey;
use sip_message::sniff;
use sip_retransmit::{Class, Schedule};

/// 64·T1 (§17.2.1 Timer H): the transaction envelope, past which no rung is
/// owed. What the audit COUNTS to — a bound on the walk, distinct from the
/// give-up a class's schedule carries, which is what a SUT SENDS to.
const ENVELOPE: Duration = Duration::from_millis(32_000);
/// The two hops between the ladder the emitter RAN and the one this side
/// measured: `claimed_us` is when our side received the head and `closed_us`
/// when it sent the closer, so the emitter's own window is one hop wider at each
/// end. A rung that ARRIVED inside this much of the closer was on the wire
/// before the closer reached the emitter, and is owed however the model rounds.
/// One hop measures 1 ms on the fake transport and at most 13.5 ms on a routed
/// one, so 30 ms covers both and nothing else — it forgives ONE rung, and only
/// one the wire already timestamped.
const TWO_HOPS: Duration = Duration::from_millis(30);
/// The rung [`rungs_within`] stops counting at. The envelope is 64·T1 = 32 s
/// (§6.9) and the slowest ladder inside it caps at T2, so no ladder anyone
/// counts reaches here; it bounds the walk, it does not shape it.
const MAX_RUNGS: u32 = 64;

/// The schedule a scripted `send` step paces its ladder by, or why it has
/// none.
///
/// The document's own gaps where it states them (§6.9), one per rung: a
/// captured platform's ladder need not be the RFC's — an implementation may
/// configure any T1 (RFC 3261 §17.1.1.1) — and a fraction of T1 of error is
/// enough to put a reliable provisional's rung on the far side of the PRACK
/// that ends it, which is a sequence the capture never held. A count longer than the
/// stated gaps repeats the last one ([`Schedule::exact`]), so a malformed
/// document paces steadily rather than falling back onto a class it never
/// chose.
///
/// Otherwise the RFC's own schedule for the class the datagram rides
/// ([`class_of`]). A class that rides no timer of its own is REFUSED by name
/// rather than paced by a default nobody wrote down (§14): an ACK is re-sent
/// once per 2xx received, an unreliable provisional at the transaction user's
/// discretion, a non-INVITE final only on a repeat of its request — except on
/// an auto ACK step, whose count [`DrawnAcks`] draws from the wire instead of
/// pacing (§6.3). A document that stated the gaps has answered the objection,
/// whatever the class.
pub fn schedule_of(wire: &[u8], measured: &[u64]) -> Result<Schedule, String> {
    if !measured.is_empty() {
        let gaps: Vec<Duration> = measured.iter().map(|ms| Duration::from_millis(*ms)).collect();
        return Ok(Schedule::exact(&gaps, None));
    }
    match class_of(wire) {
        Some(class) => Ok(Schedule::rfc(class)),
        None => Err(no_timer_of_its_own(wire)),
    }
}

/// The retransmission class the RFC puts on whoever emitted `raw`, read off the
/// datagram alone — the obligation a ladder the SUT paced and this side only
/// counted is held to. `None` where the class rides no timer of its own and its
/// repeats are DRAWN by the wire instead: an ACK, an unreliable provisional,
/// and a non-INVITE final — §17.2.2 re-sends that one only when a
/// retransmission of the request arrives, so its count is the request's, not a
/// timer's.
pub fn class_of(raw: &[u8]) -> Option<Class> {
    if sniff::is_response(raw) {
        let status = sniff::resp_status(raw)?;
        if status < 200 {
            return sniff::rseq_of(raw).map(|_| Class::ReliableProvisional);
        }
        if sniff::cseq_method_label(raw) != "INVITE" {
            return None;
        }
        return Some(if status < 300 { Class::Final2xx } else { Class::InviteServerFinal });
    }
    match sniff::req_method(raw)?.as_str() {
        "ACK" => None,
        "INVITE" => Some(Class::InviteClient),
        "CANCEL" => Some(Class::CancelClient),
        _ => Some(Class::NonInviteClient),
    }
}

/// The schedule an EXPECT-side ladder is counted against: the RFC's for the
/// class the SUT owes, or [`Schedule::once`] where the class rides no timer of
/// its own — an emitter that owes no rung is not one paced by a default.
fn pacing_of(raw: &[u8]) -> Schedule {
    class_of(raw).map_or_else(Schedule::once, Schedule::rfc)
}

/// Why `raw` has no ladder of its own, in the RFC's words.
fn no_timer_of_its_own(raw: &[u8]) -> String {
    if sniff::is_response(raw) {
        return match sniff::resp_status(raw) {
            Some(status) if status >= 200 => format!(
                "a {status} final to a {} retransmits on no timer of its own — RFC 3261 \
                 §17.2.2 re-sends it only when a retransmission of the request arrives, so \
                 its count is the request's, never paced; state `retransmit_intervals_ms` \
                 to pace it as the capture did, or drop `retransmits`",
                sniff::cseq_method_label(raw)
            ),
            Some(status) => format!(
                "an unreliable {status} provisional retransmits on no timer of its own; \
                 RFC 3262 §3 paces a RELIABLE one, and this response carries no RSeq"
            ),
            None => "a response with no status line rides no ladder".into(),
        };
    }
    match sniff::req_method(raw).as_deref() {
        Some("ACK") => "an ACK retransmits on no timer of its own — the core sends one per 2xx \
                        received (RFC 3261 §13.2.2.4), so its count is drawn by an `auto` step \
                        marking its transaction with `cseq`, never paced"
            .into(),
        _ => "a request with no request line rides no ladder".into(),
    }
}

/// How many rungs `schedule` puts on the wire inside `dwell`, rung 1 being the
/// first repeat: every rung whose cumulative wait falls strictly before the
/// closer that ends the ladder.
///
/// Bounded by the transaction envelope: past 64·T1 the transaction is gone
/// (§17.2.1 Timer H), so a ladder nothing closed inside it stops there rather
/// than counting rungs nobody owes. A schedule with no such rung — [`Schedule::once`]
/// — counts none.
pub fn rungs_within(schedule: &Schedule, dwell: Duration) -> u32 {
    let bound = dwell.min(ENVELOPE);
    let mut cumulative = Duration::ZERO;
    for n in 1..=MAX_RUNGS {
        let Some(wait) = schedule.interval(n) else {
            return n - 1;
        };
        if wait.is_zero() {
            return n - 1;
        }
        cumulative += wait;
        if cumulative >= bound {
            return n - 1;
        }
    }
    MAX_RUNGS
}

/// What ends a ladder, by the class of the datagram that opened it.
///
/// Each variant is one RFC sentence about when retransmissions CEASE, and they
/// do not agree with one another — which is why reading "is it a response?" off
/// the claimed datagram is not enough to pick one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Closer {
    /// An INVITE final: the ACK the core owes it (§13.3.1.4 for a 2xx, Timer G
    /// / §17.2.1 for a non-2xx).
    Ack,
    /// A reliable provisional: the PRACK that names it — RFC 3262 §3,
    /// "retransmissions cease when a matching PRACK is received by the UA
    /// core". A PRACK carries its OWN CSeq and names its target in `RAck`
    /// (§7.2), so this is the one closer a CSeq comparison cannot find.
    Prack { rseq: u64 },
    /// An INVITE request: ANY response. Timer A runs only in Calling, and
    /// §17.1.1.2 leaves Calling on the first response of any kind.
    AnyResponse,
    /// A non-INVITE request: only a FINAL. §17.1.2.1 has a provisional move the
    /// transaction to Proceeding and RESET Timer E to T2 — it slows the ladder,
    /// it does not stop it.
    Final,
    /// A CANCEL: its own final, or the final to the INVITE it names. Once that
    /// INVITE has its final the CANCEL is moot (§9.1) and its ladder stops
    /// (ADR-0043 X4), whether or not the CANCEL itself was ever answered.
    Cancel,
    /// A class whose repeats are drawn by the wire, so nothing this vantage
    /// sees ends a ladder it never paced.
    Nothing,
}

impl Closer {
    /// The closer the datagram `raw` waits for, read off its class alone.
    fn of(raw: &[u8]) -> Closer {
        if sip_message::sniff::is_response(raw) {
            return match sip_message::sniff::resp_status(raw) {
                Some(status) if status >= 200 => Closer::Ack,
                Some(_) => match sip_message::sniff::rseq_of(raw) {
                    Some(rseq) => Closer::Prack { rseq },
                    None => Closer::Nothing,
                },
                None => Closer::Nothing,
            };
        }
        match sip_message::sniff::req_method(raw).as_deref() {
            Some("ACK") => Closer::Nothing,
            Some("INVITE") => Closer::AnyResponse,
            Some("CANCEL") => Closer::Cancel,
            Some(_) => Closer::Final,
            None => Closer::Nothing,
        }
    }
}

/// One step's ladder as the run holds it: what the document declared, what the
/// run saw, and the bytes a repeat has to match.
#[derive(Debug, Clone)]
struct StepLadder {
    step: String,
    leg: String,
    /// Which side of the vantage paced this ladder: `Send` is the scripted
    /// peer's own and holds by construction, `Expect` is the SUT's.
    side: LadderSide,
    declared: u32,
    /// The document's own gaps for this step (§6.9); empty where it stated none.
    intervals_ms: Vec<u64>,
    observed: u32,
    claimed: Vec<u8>,
    /// `claimed`'s CSeq, and what ends this ladder — [`Closer`], read off the
    /// claimed datagram's own class.
    cseq: Option<u32>,
    cseq_method: &'static str,
    closer: Closer,
    /// The dialog `claimed` belongs to. A CSeq number is unique only WITHIN a
    /// dialog, so a leg carrying two calls — or a fork's two early dialogs
    /// (§6.5) — would otherwise let one closer end every ladder that shares its
    /// number. The tag is `None` where the claimed datagram states none: a
    /// dialog-forming request precedes every tag its responses carry.
    call_id: Option<String>,
    to_tag: Option<String>,
    /// When the step claimed its datagram — where the ladder starts counting.
    claimed_us: u64,
    /// When our side put the closer on the wire, where it did.
    closed_us: Option<u64>,
    /// The schedule the RFC puts on `claimed`'s emitter ([`pacing_of`]):
    /// its class's, or [`Schedule::once`] where the class rides no timer of its
    /// own and its repeats are drawn.
    pacing: Schedule,
    /// When each counted repeat crossed this vantage, in order.
    repeats_us: Vec<u64>,
}

impl StepLadder {
    /// Whether `raw` sits in the dialog this ladder's claimed datagram did.
    ///
    /// The Call-ID always; the To-tag only where the CLAIMED datagram carried
    /// one. A dialog-forming request precedes every tag its responses answer
    /// with, so requiring equality there would refuse the very response that
    /// ends its ladder. Where the claim DOES carry a tag, the tag separates a
    /// fork's two early dialogs, which share a Call-ID and a CSeq.
    fn same_dialog(&self, raw: &[u8]) -> bool {
        if self.call_id != sip_message::sniff::call_id(raw) {
            return false;
        }
        match &self.to_tag {
            None => true,
            Some(tag) => sip_message::sniff::to_tag(raw) == *tag,
        }
    }

    /// Whether `bye`, sent by the SUT, names this ladder's dialog: an
    /// expect-side 2xx to an INVITE whose dialog the BYE names from the 2xx's
    /// own side — its From tag is the 2xx's To tag, the tag the SUT holds the
    /// dialog under.
    fn ended_by_emitters_bye(&self, bye: &[u8]) -> bool {
        let two_xx =
            sip_message::sniff::resp_status(&self.claimed).is_some_and(|s| (200..300).contains(&s));
        self.side == LadderSide::Expect
            && self.closer == Closer::Ack
            && two_xx
            && self.call_id == sip_message::sniff::call_id(bye)
            && self.to_tag.as_deref() == Some(sip_message::sniff::from_tag(bye).as_str())
    }

    /// How long the ladder ran: the claimed datagram to the closer that ended
    /// it. `None` while nothing closed it, which is a ladder still running.
    fn dwell(&self) -> Option<Duration> {
        Some(Duration::from_micros(self.closed_us?.checked_sub(self.claimed_us)?))
    }

    /// The window the emitter had to put rungs in: the dwell where a closer
    /// ended the ladder, otherwise the run itself — a ladder nothing closed ran
    /// until the run stopped, and judging it against the capture instead would
    /// make the same emitter behaviour pass or fail on whether we got that far.
    fn window(&self, run_end_us: u64) -> Option<Duration> {
        match self.dwell() {
            Some(dwell) => Some(dwell),
            None => Some(Duration::from_micros(run_end_us.checked_sub(self.claimed_us)?)),
        }
    }

    /// Whether this ladder counts its repeats at all.
    ///
    /// A declared count always does. So does every EXPECT of a paced class,
    /// declared or not: the RFC owes those rungs whatever the capture held, and
    /// a rung nobody counts does not become legal — it reaches the gate as an
    /// unexpected datagram, which is a harsher verdict than the count it is.
    ///
    /// And so does every EXPECT of an ACK, which no timer paces and the WIRE
    /// draws: RFC 3261 §13.2.2.4 owes one ACK per 2xx received, so the count is
    /// whatever the final's own ladder turned out to be — a number the document
    /// cannot state, because it is our side's emission and not the capture's.
    fn counts(&self) -> bool {
        self.declared > 0
            || (self.side == LadderSide::Expect && (self.paced() || self.draws_its_count()))
    }

    /// Whether this ladder's count is DRAWN by the wire rather than paced: an
    /// ACK, whose repeats are one per repeat of the final it acknowledges.
    fn draws_its_count(&self) -> bool {
        sip_message::sniff::req_method(&self.claimed).as_deref() == Some("ACK")
    }

    /// Whether the RFC paces the claimed datagram's class at all: a schedule
    /// with a first rung. [`Schedule::once`] has none.
    fn paced(&self) -> bool {
        self.pacing.interval(1).is_some()
    }

    /// Whether one repeat too many is the closer's own boundary.
    ///
    /// The emitter stops on the closer it RECEIVES, and [`TWO_HOPS`] is how far
    /// that is from the closer we SENT. A single rung that arrived inside it was
    /// already on the wire; a later one is a rung after the answer, and stays a
    /// mismatch.
    fn raced_the_closer(&self, owed: u32) -> bool {
        if self.side != LadderSide::Expect || self.observed != owed + 1 {
            return false;
        }
        let Some(closed) = self.closed_us else { return false };
        let Some(&last) = self.repeats_us.last() else { return false };
        last <= closed.saturating_add(TWO_HOPS.as_micros() as u64)
    }
}

/// Every step's retransmission ladder, declared against observed (§6.9).
///
/// A step CLAIMS the datagram it emitted or matched; every later datagram with
/// those bytes on that leg is a repeat of it. Only a step that DECLARES a count
/// counts them: an undeclared repeat stays what it was — recorded evidence the
/// transaction layer absorbed or the gate refused — because a count nobody
/// stated is not a count this run may invent.
#[derive(Debug, Default)]
pub struct Repeats {
    ladders: Vec<StepLadder>,
}

impl Repeats {
    pub fn new() -> Repeats {
        Repeats::default()
    }

    /// Register the datagram `step` put on the wire or matched, with the count
    /// the document declares for it.
    ///
    /// A BYE an expect claims is the SUT ending a dialog (RFC 3261 §15): every
    /// ladder of a 2xx the SUT ran to a re-INVITE of ours on that dialog stops
    /// there, so the claim closes them ([`StepLadder::ended_by_emitters_bye`]).
    /// The 2xx that created the dialog keeps its ladder to the ACK or the
    /// §13.3.1.4 give-up.
    pub fn claim(
        &mut self,
        step: &str,
        leg: &str,
        side: LadderSide,
        declared: Option<u32>,
        intervals_ms: &[u64],
        raw: &[u8],
        at_us: u64,
    ) {
        if side == LadderSide::Expect
            && sip_message::sniff::req_method(raw).as_deref() == Some("BYE")
        {
            let reinvites: Vec<(Option<String>, Option<u32>)> = self
                .ladders
                .iter()
                .filter(|l| l.leg == leg && l.side == LadderSide::Send && l.to_tag.is_some())
                .filter(|l| sip_message::sniff::req_method(&l.claimed).as_deref() == Some("INVITE"))
                .map(|l| (l.call_id.clone(), l.cseq))
                .collect();
            for ladder in self.ladders.iter_mut().filter(|l| l.leg == leg) {
                let answers_a_reinvite = reinvites
                    .iter()
                    .any(|(call, cseq)| *call == ladder.call_id && *cseq == ladder.cseq);
                if ladder.closed_us.is_none()
                    && answers_a_reinvite
                    && ladder.ended_by_emitters_bye(raw)
                {
                    ladder.closed_us = Some(at_us);
                }
            }
        }
        self.ladders.retain(|l| l.step != step);
        self.ladders.push(StepLadder {
            step: step.to_string(),
            leg: leg.to_string(),
            side,
            declared: declared.unwrap_or(0),
            intervals_ms: intervals_ms.to_vec(),
            observed: 0,
            claimed: raw.to_vec(),
            cseq: sip_message::sniff::cseq_number(raw),
            cseq_method: sip_message::sniff::cseq_method_label(raw),
            closer: Closer::of(raw),
            call_id: sip_message::sniff::call_id(raw),
            to_tag: Some(sip_message::sniff::to_tag(raw)).filter(|t| !t.is_empty()),
            claimed_us: at_us,
            closed_us: None,
            pacing: pacing_of(raw),
            repeats_us: Vec::new(),
        });
    }

    /// The step whose declared ladder `raw` belongs to, counting it.
    ///
    /// The most recently claimed match wins, so a leg that carries the same
    /// bytes twice credits the ladder still running. `None` when no step
    /// declared a ladder for these bytes — the datagram is then whatever it
    /// already was, and this module says nothing about it.
    pub fn note(&mut self, leg: &str, raw: &[u8], at_us: u64) -> Option<String> {
        let ladder = self
            .ladders
            .iter_mut()
            .rev()
            .find(|l| l.leg == leg && l.counts() && l.claimed == raw)?;
        ladder.observed += 1;
        ladder.repeats_us.push(at_us);
        Some(ladder.step.clone())
    }

    /// The datagram OUR side put on the wire that ends a ladder — [`Closer`] per
    /// class, inside the claimed datagram's own dialog, whichever comes first.
    ///
    /// Only an expect-side ladder needs it. A send-side ladder is ours to pace
    /// and its count holds by construction, so nothing is closed there.
    pub fn answered(&mut self, leg: &str, raw: &[u8], at_us: u64) {
        let cseq = sip_message::sniff::cseq_number(raw);
        let is_response = sip_message::sniff::is_response(raw);
        let status = sip_message::sniff::resp_status(raw);
        let method = sip_message::sniff::cseq_method_label(raw);
        let req_method = sip_message::sniff::req_method(raw);
        for ladder in self.ladders.iter_mut() {
            if ladder.leg != leg || !ladder.counts() || ladder.closed_us.is_some() {
                continue;
            }
            if !ladder.same_dialog(raw) {
                continue;
            }
            let same_transaction = ladder.cseq == cseq && ladder.cseq_method == method;
            let closes = match ladder.closer {
                Closer::Ack => {
                    !is_response && req_method.as_deref() == Some("ACK") && ladder.cseq == cseq
                }
                // RFC 3262 §7.2: `RAck` names its target by response-num AND the
                // INVITE's CSeq-num, because two INVITEs on one dialog number
                // their `RSeq` spaces independently.
                Closer::Prack { rseq } => {
                    !is_response
                        && req_method.as_deref() == Some("PRACK")
                        && sip_message::sniff::rack_rseq(raw) == Some(rseq)
                        && sip_message::sniff::rack_cseq(raw) == ladder.cseq
                }
                Closer::AnyResponse => is_response && same_transaction,
                Closer::Final => {
                    is_response && status.is_some_and(|s| s >= 200) && same_transaction
                }
                Closer::Cancel => {
                    is_response
                        && status.is_some_and(|s| s >= 200)
                        && ladder.cseq == cseq
                        && (method == "CANCEL" || method == "INVITE")
                }
                Closer::Nothing => false,
            };
            if closes {
                ladder.closed_us = Some(at_us);
            }
        }
    }

    /// Whether a step's declared ladder has room for another repeat — what the
    /// note a recording carries says, not a tolerance rule: an over-count is
    /// counted and reported, never silently dropped.
    #[cfg(test)]
    pub fn declared_for(&self, step: &str) -> u32 {
        self.ladders.iter().find(|l| l.step == step).map_or(0, |l| l.declared)
    }

    /// How many repeats ladder `i`'s emitter OWED, or `None` where nothing
    /// states a number — §6.9's four regimes, which differ in WHO paced the
    /// ladder and therefore in who says how long it is:
    ///
    /// | ladder | paced by | owes |
    /// |---|---|---|
    /// | a scripted send | the document | `declared` — it is an instruction |
    /// | an auto ACK send | the wire (§13.2.2.4) | one per repeat of its final |
    /// | an expect of an ACK | the SUT | `declared`, or nothing where none is stated |
    /// | an expect of a paced class | the SUT, on the RFC's ladder | the rungs inside its window |
    /// | an expect of another drawn class | our own emissions, 1:1 | `declared` |
    fn owed(&self, i: usize, run_end_us: u64) -> Option<u32> {
        let ladder = &self.ladders[i];
        match ladder.side {
            // An ACK rides no timer on either side, so its count is the WIRE's
            // and never a pacing. Ours we produce, so the paired final's own
            // repeats say how many (RFC 3261 §13.2.2.4). A count read off the
            // capture would assert the captured platform's ladder instead. The
            // ladder still COUNTS either way, so a re-ACK lands in it instead of
            // reaching the gate as a stray.
            _ if ladder.draws_its_count() => match ladder.side {
                LadderSide::Send => {
                    Some(self.paired_final(ladder).map_or(ladder.declared, |f| f.observed))
                }
                LadderSide::Expect if ladder.declared > 0 => Some(ladder.declared),
                LadderSide::Expect => None,
            },
            LadderSide::Send => Some(ladder.declared),
            LadderSide::Expect if !ladder.paced() => Some(ladder.declared),
            LadderSide::Expect => {
                ladder.window(run_end_us).map(|w| rungs_within(&ladder.pacing, w))
            }
        }
    }

    /// The INVITE final an ACK ladder acknowledges — ours or the SUT's: same
    /// dialog, same transaction, on the same leg. `None` where no step claimed
    /// it, in which case the wire drew nothing this side can count.
    fn paired_final(&self, ack: &StepLadder) -> Option<&StepLadder> {
        self.ladders.iter().find(|l| {
            l.leg == ack.leg
                && l.call_id == ack.call_id
                && l.to_tag == ack.to_tag
                && l.cseq == ack.cseq
                && sip_message::sniff::resp_status(&l.claimed).is_some_and(|s| s >= 200)
        })
    }

    /// Every ladder the run counted, for the verdict (§6.9's counts, exposed) —
    /// with the ladder's facts beside them: the document's own gaps, the dwell
    /// the closer ended it in, and, for the EXPECT side alone, the rungs an
    /// RFC-paced ladder of this message's class puts inside the window it had.
    /// A send ladder paced itself, so the RFC's count says nothing about it.
    ///
    /// A ladder that declared nothing, saw nothing and owed nothing states
    /// nothing, so it is left out. Every ladder a failure can name is in.
    pub fn notes(&self, run_end_us: u64) -> Vec<RetransmitNote> {
        self.ladders
            .iter()
            .filter(|l| l.counts())
            .map(|l| RetransmitNote {
                step: l.step.clone(),
                leg: l.leg.clone(),
                side: l.side,
                declared: l.declared,
                observed: l.observed,
                intervals_ms: l.intervals_ms.clone(),
                dwell_us: l.dwell().map(|d| d.as_micros() as u64),
                rfc_rungs: (l.side == LadderSide::Expect && l.paced())
                    .then(|| l.window(run_end_us))
                    .flatten()
                    .map(|window| rungs_within(&l.pacing, window)),
            })
            .filter(|n| n.declared > 0 || n.observed > 0 || n.rfc_rungs.is_some_and(|r| r > 0))
            .collect()
    }

    /// A ladder that is not the one its emitter owed, as the failure it is. A
    /// count is a protocol fact, not a lane's vocabulary, so it gates on every
    /// lane and this module excuses none of them.
    ///
    /// What each ladder owes is [`Repeats::owed`]'s call and never a margin:
    /// where the SUT paced it, the oracle is the RFC on every lane, because an
    /// interpreter that knew which stack was behind the socket could no longer
    /// report that the stack differs. The one thing forgiven is the closer's own
    /// boundary ([`StepLadder::raced_the_closer`]), and that is read off the
    /// arrival the wire timestamped.
    pub fn mismatches(&self, run_end_us: u64) -> Vec<Failure> {
        (0..self.ladders.len())
            .filter_map(|i| {
                let ladder = &self.ladders[i];
                if !ladder.counts() {
                    return None;
                }
                let expected = self.owed(i, run_end_us)?;
                if ladder.observed == expected || ladder.raced_the_closer(expected) {
                    return None;
                }
                Some(Failure::RetransmitCountMismatch {
                    step: ladder.step.clone(),
                    leg: ladder.leg.clone(),
                    declared: ladder.declared,
                    expected,
                    observed: ladder.observed,
                })
            })
            .collect()
    }
}

/// One auto ACK step's emission, held so a later repeat of the final can be
/// answered with the SAME datagram (RFC 3261 §13.2.2.4: one client transaction,
/// one ACK, re-sent).
#[derive(Debug, Clone)]
pub struct DrawnAck {
    pub step: String,
    pub leg: String,
    pub wire: Vec<u8>,
    pub dst: SocketAddr,
}

/// The ACK ladders the WIRE draws (§6.3): one ACK per repeat of the 2xx the
/// step's transaction drew, keyed on the dialog
/// [`scenario_harness::absorption`]'s own core keys its ACK cache by.
///
/// A repeat that arrives while the ACK is still HELD is not lost — the core
/// owes an ACK for every 2xx it received, so the copies pile up and go out with
/// the ACK when the hold ends. A repeat that arrives after it copies the
/// datagram already sent.
#[derive(Debug, Default)]
pub struct DrawnAcks {
    /// Repeats of a final sighted before its ACK went out.
    owed: BTreeMap<AckKey, u32>,
    /// The ACK already sent for a confirmed dialog.
    sent: BTreeMap<AckKey, DrawnAck>,
}

impl DrawnAcks {
    pub fn new() -> DrawnAcks {
        DrawnAcks::default()
    }

    /// Note the ACK `step` put on the wire for `key`, and answer with the
    /// number of CATCH-UP copies it owes: one per repeat of the final that
    /// arrived while the step was held.
    pub fn sent(&mut self, key: AckKey, ack: DrawnAck) -> u32 {
        let owed = self.owed.remove(&key).unwrap_or(0);
        self.sent.insert(key, ack);
        owed
    }

    /// A repeat of a 2xx was sighted: the ACK to copy NOW, or `None` when the
    /// step that owes it has not run yet — in which case the repeat is
    /// remembered and drawn when it does.
    pub fn repeat_sighted(&mut self, key: AckKey) -> Option<DrawnAck> {
        match self.sent.get(&key) {
            Some(ack) => Some(ack.clone()),
            None => {
                *self.owed.entry(key).or_insert(0) += 1;
                None
            }
        }
    }
}

#[cfg(test)]
mod tests;
