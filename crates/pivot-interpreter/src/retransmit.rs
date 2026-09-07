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
//!   the SUT rides (ADR-0029), so the oracle and the thing it confronts cannot
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
/// captured platform's ladder is not the RFC's — measured across the corpus it
/// runs from 409 ms to 17.5 s where T1 says 500 — and 92 ms of error is enough
/// to put a reliable provisional's rung on the far side of the PRACK that ends
/// it, which is a sequence the capture never held. A count longer than the
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
                Closer::Final => is_response && status.is_some_and(|s| s >= 200) && same_transaction,
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
            // An ACK rides no timer, so a count on one is never a pacing: the
            // core sends one per 2xx it received, and the paired final's OWN
            // repeats are how many that is. A count read off the capture would
            // assert the captured platform's ladder instead.
            // An ACK rides no timer on either side, so its count is the WIRE's
            // and never a pacing. Ours we produce, so the paired final's own
            // repeats say how many (RFC 3261 §13.2.2.4). The SUT's is the SUT's:
            // a copy arriving BEFORE its first ACK draws no second one — there
            // is no ACK to re-send yet — so the number turns on a timing this
            // module does not model, and it asserts one only where the DOCUMENT
            // states it. The ladder still COUNTS either way, so a re-ACK lands
            // in it instead of reaching the gate as a stray.
            _ if ladder.draws_its_count() => match ladder.side {
                LadderSide::Send => {
                    Some(self.paired_final(ladder).map_or(ladder.declared, |f| f.observed))
                }
                LadderSide::Expect if ladder.declared > 0 => Some(ladder.declared),
                LadderSide::Expect => None,
            },
            LadderSide::Send => Some(ladder.declared),
            LadderSide::Expect if !ladder.paced() => Some(ladder.declared),
            LadderSide::Expect => ladder
                .window(run_end_us)
                .map(|w| rungs_within(&ladder.pacing, w)),
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
mod tests {
    use super::*;

    /// A run window wide enough that a CLOSED ladder's dwell is what bounds it.
    const RUN_END: u64 = 60_000_000;

    const fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    const INVITE: &str = "INVITE sip:bob@10.0.0.2 SIP/2.0\r\n\
        Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-1\r\n\
        From: <sip:alice@10.0.0.1>;tag=a1\r\n\
        To: <sip:bob@10.0.0.2>\r\n\
        Call-ID: c1\r\n\
        CSeq: 1 INVITE\r\n\
        Content-Length: 0\r\n\r\n";

    fn ladder_of(raw: &str) -> Result<Schedule, String> {
        schedule_of(raw.as_bytes(), &[])
    }

    /// A captured platform's ladder is not the RFC's, and where the document
    /// measured it the document wins (§6.9, issue 90).
    #[test]
    fn a_measured_ladder_paces_by_the_documents_own_gaps() {
        let ladder = schedule_of(INVITE.as_bytes(), &[408, 17_480])
            .expect("a document that states its gaps paces by them");
        assert_eq!(ladder.interval(1), Some(ms(408)));
        assert_eq!(ladder.interval(2), Some(ms(17_480)));
        assert_eq!(
            ladder.interval(3),
            Some(ms(17_480)),
            "a count longer than its gaps repeats the last, never falls back onto a class ladder"
        );
    }

    /// A class is refused for having no interval anyone stated; a document that
    /// stated one has answered the objection.
    #[test]
    fn measured_gaps_pace_a_class_that_has_no_ladder_of_its_own() {
        let ack = INVITE
            .replace("INVITE sip:bob@10.0.0.2 SIP/2.0", "ACK sip:bob@10.0.0.2 SIP/2.0")
            .replace("CSeq: 1 INVITE", "CSeq: 1 ACK");
        assert!(ladder_of(&ack).is_err(), "an ACK rides no timer of its own");
        let ladder = schedule_of(ack.as_bytes(), &[203]).expect("stated gaps pace it");
        assert_eq!(ladder.interval(1), Some(ms(203)));
    }

    #[test]
    fn an_invite_doubles_without_a_ceiling() {
        let ladder = ladder_of(INVITE).expect("an INVITE has Timer A");
        assert_eq!(ladder, Schedule::rfc(Class::InviteClient));
        assert_eq!(ladder.interval(1), Some(ms(500)));
        assert_eq!(ladder.interval(2), Some(ms(1_000)));
        assert_eq!(ladder.interval(3), Some(ms(2_000)));
        assert_eq!(ladder.interval(6), Some(ms(16_000)), "Timer A never caps");
    }

    #[test]
    fn every_other_ladder_doubles_into_t2() {
        let bye = INVITE
            .replace("INVITE sip:bob@10.0.0.2 SIP/2.0", "BYE sip:bob@10.0.0.2 SIP/2.0")
            .replace("CSeq: 1 INVITE", "CSeq: 2 BYE");
        let ladder = ladder_of(&bye).expect("a non-INVITE request has Timer E");
        assert_eq!(ladder, Schedule::rfc(Class::NonInviteClient));
        assert_eq!(ladder.interval(1), Some(ms(500)));
        assert_eq!(ladder.interval(4), Some(ms(4_000)));
        assert_eq!(ladder.interval(9), Some(ms(4_000)), "T2 is the ceiling");
        let cancel = INVITE
            .replace("INVITE sip:bob@10.0.0.2 SIP/2.0", "CANCEL sip:bob@10.0.0.2 SIP/2.0")
            .replace("CSeq: 1 INVITE", "CSeq: 1 CANCEL");
        assert_eq!(
            ladder_of(&cancel).expect("ADR-0028 X4"),
            Schedule::rfc(Class::CancelClient),
            "a CANCEL is the class of its own the schedule names"
        );
    }

    #[test]
    fn a_final_response_rides_the_capped_ladder() {
        let ok = "SIP/2.0 200 OK\r\n\
            Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-1\r\n\
            From: <sip:alice@10.0.0.1>;tag=a1\r\n\
            To: <sip:bob@10.0.0.2>;tag=b1\r\n\
            Call-ID: c1\r\n\
            CSeq: 1 INVITE\r\n\
            Content-Length: 0\r\n\r\n";
        assert_eq!(ladder_of(ok).expect("§13.3.1.4"), Schedule::rfc(Class::Final2xx));
        let busy = ok.replace("SIP/2.0 200 OK", "SIP/2.0 486 Busy Here");
        assert_eq!(
            ladder_of(&busy).expect("§17.2.1 Timer G"),
            Schedule::rfc(Class::InviteServerFinal)
        );
        let to_bye = ok.replace("CSeq: 1 INVITE", "CSeq: 2 BYE");
        let why = ladder_of(&to_bye).expect_err("§17.2.2: re-sent on a repeated request only");
        assert!(why.contains("17.2.2"), "{why}");
        assert_eq!(
            schedule_of(to_bye.as_bytes(), &[500]).expect("stated gaps pace it").interval(1),
            Some(ms(500)),
            "a document that stated the gaps has answered the objection"
        );
    }

    #[test]
    fn an_ack_has_no_ladder_and_says_why() {
        let ack = INVITE
            .replace("INVITE sip:bob@10.0.0.2 SIP/2.0", "ACK sip:bob@10.0.0.2 SIP/2.0")
            .replace("CSeq: 1 INVITE", "CSeq: 1 ACK");
        let why = ladder_of(&ack).expect_err("an ACK retransmits on no timer");
        assert!(why.contains("one per 2xx"), "{why}");
    }

    #[test]
    fn an_unreliable_provisional_has_no_ladder_and_a_reliable_one_does() {
        let ringing = "SIP/2.0 180 Ringing\r\n\
            Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-1\r\n\
            From: <sip:alice@10.0.0.1>;tag=a1\r\n\
            To: <sip:bob@10.0.0.2>;tag=b1\r\n\
            Call-ID: c1\r\n\
            CSeq: 1 INVITE\r\n\
            Content-Length: 0\r\n\r\n";
        let why = ladder_of(ringing).expect_err("no RSeq, no ladder");
        assert!(why.contains("RSeq"), "{why}");
        let reliable = ringing.replace("Call-ID: c1", "RSeq: 1\r\nCall-ID: c1");
        assert_eq!(
            ladder_of(&reliable).expect("RFC 3262 §3"),
            Schedule::rfc(Class::ReliableProvisional)
        );
    }

    #[test]
    fn a_send_ladder_counts_only_what_the_document_declared() {
        let mut repeats = Repeats::new();
        repeats.claim("s1", "A", LadderSide::Send, Some(2), &[], b"INVITE\r\n", 0);
        repeats.claim("s3", "B", LadderSide::Send, None, &[], b"OTHER\r\n", 0);
        assert_eq!(repeats.note("A", b"INVITE\r\n", 0).as_deref(), Some("s1"));
        assert_eq!(repeats.note("A", b"INVITE\r\n", 0).as_deref(), Some("s1"));
        // The same bytes on ANOTHER leg are another leg's business.
        assert_eq!(repeats.note("B", b"INVITE\r\n", 0), None);
        // Our own peer emits what the document says and nothing else, so an
        // undeclared SEND counts nothing.
        assert_eq!(repeats.note("B", b"OTHER\r\n", 0), None);
        assert!(repeats.mismatches(0).is_empty(), "2 declared, 2 seen");
        assert_eq!(
            repeats.notes(0),
            vec![RetransmitNote {
                step: "s1".into(),
                leg: "A".into(),
                side: LadderSide::Send,
                declared: 2,
                observed: 2,
                intervals_ms: Vec::new(),
                dwell_us: None,
                rfc_rungs: None
            }],
            "the verdict lists the declared ladder and what it saw"
        );
    }

    /// The SUT's own rungs are owed whether or not the capture held them: a
    /// paced-class EXPECT claims its ladder with nothing declared, so a rung
    /// that arrives is counted here instead of reaching the gate as a datagram
    /// nobody expected.
    #[test]
    fn an_undeclared_paced_expect_counts_its_rungs_and_is_held_to_the_rfc() {
        let bye = b"BYE sip:b@h SIP/2.0\r\nCall-ID: c1\r\nCSeq: 7 BYE\r\n\r\n";
        let ok = b"SIP/2.0 200 OK\r\nCall-ID: c1\r\nCSeq: 7 BYE\r\n\r\n";
        let mut repeats = Repeats::new();
        repeats.claim("s4", "B", LadderSide::Expect, None, &[], bye, 0);
        assert_eq!(repeats.note("B", bye, 500_000).as_deref(), Some("s4"));
        assert_eq!(repeats.note("B", bye, 1_500_000).as_deref(), Some("s4"));
        repeats.answered("B", ok, 1_508_000);
        assert!(
            repeats.mismatches(2_000_000).is_empty(),
            "500 and 1500 are the two rungs a 1508 ms dwell owes"
        );
        let note = &repeats.notes(2_000_000)[0];
        assert_eq!((note.declared, note.observed, note.rfc_rungs), (0, 2, Some(2)));
        // A ladder the run ASSERTS states itself, so no failure names a note
        // the verdict does not carry: nothing declared, nothing seen, 5 owed.
        let mut silent = Repeats::new();
        silent.claim("s8", "A", LadderSide::Expect, None, &[], bye, 0);
        assert_eq!(silent.notes(12_400_000)[0].rfc_rungs, Some(5));
        assert!(matches!(
            silent.mismatches(12_400_000).as_slice(),
            [Failure::RetransmitCountMismatch { declared: 0, expected: 5, observed: 0, .. }]
        ));
    }

    /// The emitter stops on the closer it RECEIVED, which is one hop after we
    /// sent it, so a rung already on the wire arrives just behind our answer.
    #[test]
    fn one_rung_that_raced_the_closer_is_the_boundary_and_a_later_one_is_not() {
        let bye = b"BYE sip:b@h SIP/2.0\r\nCall-ID: c1\r\nCSeq: 7 BYE\r\n\r\n";
        let ok = b"SIP/2.0 200 OK\r\nCall-ID: c1\r\nCSeq: 7 BYE\r\n\r\n";
        let ladder = |arrival_us: u64| {
            let mut repeats = Repeats::new();
            repeats.claim("s4", "B", LadderSide::Expect, None, &[], bye, 0);
            // A 600 ms dwell owes exactly the 500 ms rung.
            repeats.note("B", bye, 500_000);
            repeats.answered("B", ok, 600_000);
            repeats.note("B", bye, arrival_us);
            repeats.mismatches(2_000_000)
        };
        assert!(ladder(601_000).is_empty(), "1 ms behind the closer is the same rung");
        assert!(
            matches!(
                ladder(650_000).as_slice(),
                [Failure::RetransmitCountMismatch { expected: 1, observed: 2, .. }]
            ),
            "50 ms behind it is a retransmission after the answer"
        );
    }

    /// An ACK rides no timer: the core owes one per 2xx it received, so the
    /// paired final's OWN repeats state the count, not the capture's.
    #[test]
    fn an_auto_ack_ladder_is_held_to_the_finals_own_repeats() {
        let ok = b"SIP/2.0 200 OK\r\nTo: <sip:b@h>;tag=b1\r\nCall-ID: c9\r\nCSeq: 4 INVITE\r\n\r\n";
        let ack = b"ACK sip:b@h SIP/2.0\r\nTo: <sip:b@h>;tag=b1\r\nCall-ID: c9\r\nCSeq: 4 ACK\r\n\r\n";
        let drawn = |copies: u32| {
            let mut repeats = Repeats::new();
            repeats.claim("s1", "A", LadderSide::Expect, None, &[], ok, 0);
            repeats.note("A", ok, 500_000);
            repeats.note("A", ok, 1_500_000);
            // The document read ONE repeat off the capture; the wire drew two.
            repeats.claim("s2", "A", LadderSide::Send, Some(1), &[], ack, 0);
            for _ in 0..copies {
                repeats.note("A", ack, 0);
            }
            repeats.mismatches(1_600_000)
        };
        assert!(drawn(2).is_empty(), "one ACK per repeat of the final it acknowledges");
        assert!(matches!(
            drawn(1).as_slice(),
            [Failure::RetransmitCountMismatch { step, expected: 2, observed: 1, .. }] if step == "s2"
        ));
    }

    #[test]
    fn a_ladder_that_did_not_arrive_is_a_failure_that_names_both_counts() {
        let mut repeats = Repeats::new();
        repeats.claim("s2", "A", LadderSide::Send, Some(2), &[], b"SIP/2.0 100 Trying\r\n", 0);
        repeats.note("A", b"SIP/2.0 100 Trying\r\n", 0);
        let mismatches = repeats.mismatches(0);
        assert_eq!(
            mismatches,
            vec![Failure::RetransmitCountMismatch {
                step: "s2".into(),
                leg: "A".into(),
                declared: 2,
                expected: 2,
                observed: 1
            }]
        );
        assert_eq!(repeats.declared_for("s2"), 2);
        assert_eq!(repeats.declared_for("s9"), 0, "a step with no ladder declares none");
    }

    fn drawn(step: &str) -> DrawnAck {
        DrawnAck {
            step: step.into(),
            leg: "A".into(),
            wire: b"ACK\r\n".to_vec(),
            dst: "127.0.0.1:5060".parse().unwrap(),
        }
    }

    #[test]
    fn a_repeat_sighted_before_the_ack_went_out_is_drawn_when_it_does() {
        let key = ("c1".to_string(), 1, "b1".to_string());
        let mut acks = DrawnAcks::new();
        // Two copies of the final arrive while the ACK is held.
        assert!(acks.repeat_sighted(key.clone()).is_none());
        assert!(acks.repeat_sighted(key.clone()).is_none());
        assert_eq!(acks.sent(key.clone(), drawn("s8")), 2, "both are owed at emission");
        // And a third repeat, after the hold, copies the datagram already sent.
        let copy = acks.repeat_sighted(key).expect("the ACK is out, so the repeat draws it");
        assert_eq!((copy.step.as_str(), copy.wire.as_slice()), ("s8", b"ACK\r\n".as_slice()));
    }

    #[test]
    fn another_dialog_s_final_draws_nothing() {
        let mut acks = DrawnAcks::new();
        acks.sent(("c1".into(), 1, "b1".into()), drawn("s8"));
        // A forked 2xx confirms its own dialog: same Call-ID and CSeq, other tag.
        assert!(acks.repeat_sighted(("c1".into(), 1, "b2".into())).is_none());
        // As does the next transaction on the same dialog.
        assert!(acks.repeat_sighted(("c1".into(), 2, "b1".into())).is_none());
    }

    #[test]
    fn one_repeat_too_many_is_counted_and_reported_not_dropped() {
        let mut repeats = Repeats::new();
        repeats.claim("s1", "A", LadderSide::Send, Some(1), &[], b"INVITE\r\n", 0);
        for _ in 0..3 {
            assert_eq!(repeats.note("A", b"INVITE\r\n", 0).as_deref(), Some("s1"));
        }
        assert!(matches!(
            repeats.mismatches(0).as_slice(),
            [Failure::RetransmitCountMismatch { declared: 1, expected: 1, observed: 3, .. }]
        ));
    }

    /// `capture_191724` case1 s15, to the millisecond: the SUT's BYE ladder on
    /// T1 = 500 puts a second rung on the wire 8 ms before the callee's 200,
    /// where the captured platform on a 638 ms T1 fired only one. The SUT paced
    /// this ladder, so the RFC states its length and the run HOLDS — the
    /// document's count stays beside it as the evidence it is.
    #[test]
    fn an_expect_ladder_is_held_to_the_rfcs_rungs_not_the_documents_count() {
        let bye = b"BYE sip:b@h SIP/2.0\r\nCSeq: 682945 BYE\r\n\r\n";
        let ok = b"SIP/2.0 200 OK\r\nCSeq: 682945 BYE\r\n\r\n";
        let mut repeats = Repeats::new();
        // s15 matched the original at 34788 ms; the two rungs are the repeats.
        repeats.claim("s15", "B", LadderSide::Expect, Some(1), &[638], bye, 34_788_000);
        repeats.note("B", bye, 35_288_000);
        repeats.note("B", bye, 36_288_000);
        repeats.answered("B", ok, 36_296_000);
        assert!(
            repeats.mismatches(40_000_000).is_empty(),
            "the SUT paced it, so the oracle is the RFC and 2 is what it owed"
        );
        assert_eq!(
            repeats.notes(40_000_000),
            vec![RetransmitNote {
                step: "s15".into(),
                leg: "B".into(),
                side: LadderSide::Expect,
                declared: 1,
                observed: 2,
                intervals_ms: vec![638],
                dwell_us: Some(1_508_000),
                rfc_rungs: Some(2)
            }],
            "T1 = 500 doubling into T2 puts 500 and 1500 inside a 1508 ms dwell"
        );
    }

    /// A ladder nothing closed ran until the run stopped. Judging it against the
    /// capture instead would make the same SUT behaviour pass or fail on how far
    /// the run got before it died.
    #[test]
    fn a_ladder_nothing_closed_is_owed_the_rungs_the_run_left_room_for() {
        let bye = b"BYE sip:b@h SIP/2.0\r\nCSeq: 7 BYE\r\n\r\n";
        let mut repeats = Repeats::new();
        repeats.claim("s9", "B", LadderSide::Expect, Some(1), &[638], bye, 0);
        repeats.note("B", bye, 500_000);
        repeats.note("B", bye, 1_500_000);
        let note = &repeats.notes(1_600_000)[0];
        assert_eq!((note.dwell_us, note.rfc_rungs), (None, Some(2)));
        assert!(repeats.mismatches(1_600_000).is_empty(), "a 1.6 s run holds 500 and 1500");
        assert!(
            matches!(
                repeats.mismatches(600_000).as_slice(),
                [Failure::RetransmitCountMismatch { expected: 1, observed: 2, .. }]
            ),
            "a run that stopped at 600 ms left room for one"
        );
    }

    #[test]
    fn a_class_that_rides_no_timer_of_its_own_states_no_rung_count() {
        let ack = b"ACK sip:b@h SIP/2.0\r\nCSeq: 7 ACK\r\n\r\n";
        let bye = b"BYE sip:b@h SIP/2.0\r\nCSeq: 7 BYE\r\n\r\n";
        let mut repeats = Repeats::new();
        repeats.claim("s9", "B", LadderSide::Expect, Some(1), &[], ack, 0);
        repeats.note("B", ack, 0);
        repeats.note("B", ack, 0);
        // The BYE closes nothing here; the dwell comes from the ACK's own 2xx.
        repeats.answered("B", bye, 1_000_000);
        let note = &repeats.notes(2_000_000)[0];
        assert_eq!(note.rfc_rungs, None, "an ACK is drawn by the wire, never paced");
    }

    #[test]
    fn a_reliable_provisionals_ladder_never_reaches_a_t2_ceiling() {
        // RFC 3262 §3: T1, doubling, and no T2 cap — the section names the
        // difference from a 2xx, whose ACK is drawn by the 2xx itself.
        let prack = Schedule::rfc(Class::ReliableProvisional);
        let ack = Schedule::rfc(Class::Final2xx);
        // Identical while doubling stays under T2.
        assert_eq!(rungs_within(&prack, ms(7_501)), 4);
        assert_eq!(rungs_within(&ack, ms(7_501)), 4);
        // The 5th rung is where the ceiling shows: 11.5 s capped, 15.5 s not.
        assert_eq!(rungs_within(&ack, ms(11_501)), 5);
        assert_eq!(rungs_within(&prack, ms(11_501)), 4);
    }

    #[test]
    fn the_rung_count_walks_the_class_ladder_and_stops_at_the_closer() {
        let bye = "BYE sip:b@h SIP/2.0\r\nCSeq: 7 BYE\r\n\r\n";
        let class = class_of(bye.as_bytes()).expect("a request has Timer E");
        assert_eq!(class, Class::NonInviteClient);
        let capped = Schedule::rfc(class);
        assert_eq!(rungs_within(&capped, ms(499)), 0);
        assert_eq!(rungs_within(&capped, ms(500)), 0, "a rung AT the closer is not inside it");
        assert_eq!(rungs_within(&capped, ms(501)), 1);
        assert_eq!(rungs_within(&capped, ms(1_508)), 2);
        assert_eq!(rungs_within(&capped, ms(3_508)), 3);
        // A measured ladder EXTRAPOLATES its last gap ([`Schedule::exact`]),
        // so it states how a peer PACES a count and never how many rungs the
        // capture held: that count is the document's `retransmits` itself.
        let measured = schedule_of(bye.as_bytes(), &[638]).expect("stated gaps pace it");
        assert_eq!(rungs_within(&measured, ms(1_508)), 2);
        // And a class that rides no timer counts none: `once`, not "by the RFC".
        let ack = b"ACK sip:b@h SIP/2.0\r\nCSeq: 7 ACK\r\n\r\n";
        assert_eq!(pacing_of(ack), Schedule::once());
        assert_eq!(rungs_within(&pacing_of(ack), ms(40_000)), 0);
    }

    /// RFC 3262 §3 — a reliable provisional's ladder ceases on its PRACK, not
    /// on the ACK that ends the INVITE it rides. The PRACK carries its own CSeq
    /// and names its target in `RAck` (§7.2), so a CSeq comparison never finds it.
    #[test]
    fn a_reliable_provisional_closes_on_its_prack_not_on_the_ack() {
        let p183 = b"SIP/2.0 183 Session Progress\r\nCall-ID: c1\r\nTo: <sip:b@h>;tag=t1\r\n\
            CSeq: 1 INVITE\r\nRSeq: 625707\r\n\r\n";
        let prack = b"PRACK sip:b@h SIP/2.0\r\nCall-ID: c1\r\nTo: <sip:b@h>;tag=t1\r\n\
            CSeq: 2 PRACK\r\nRAck: 625707 1 INVITE\r\n\r\n";
        let ack = b"ACK sip:b@h SIP/2.0\r\nCall-ID: c1\r\nTo: <sip:b@h>;tag=t1\r\n\
            CSeq: 1 ACK\r\n\r\n";
        let mut repeats = Repeats::new();
        repeats.claim("s6", "A", LadderSide::Expect, Some(1), &[500], p183, 2_314_000);
        repeats.answered("A", prack, 2_359_000);
        // The ACK arrives five seconds later and must not move the dwell.
        repeats.answered("A", ack, 7_557_000);
        assert_eq!(
            (repeats.notes(RUN_END)[0].dwell_us, repeats.notes(RUN_END)[0].rfc_rungs),
            (Some(45_000), Some(0)),
            "the PRACK closed it at 45 ms, so the T1 ladder owed no rung"
        );
    }

    /// A PRACK naming another INVITE's `RSeq` space closes nothing: §7.2's
    /// `RAck` names its target by response-num AND the INVITE's CSeq-num.
    #[test]
    fn a_prack_for_another_invite_closes_nothing() {
        let p183 = b"SIP/2.0 183 Session Progress\r\nCall-ID: c1\r\nTo: <sip:b@h>;tag=t1\r\n\
            CSeq: 1 INVITE\r\nRSeq: 7\r\n\r\n";
        let elsewhere = b"PRACK sip:b@h SIP/2.0\r\nCall-ID: c1\r\nTo: <sip:b@h>;tag=t1\r\n\
            CSeq: 9 PRACK\r\nRAck: 7 4 INVITE\r\n\r\n";
        let mut repeats = Repeats::new();
        repeats.claim("s6", "A", LadderSide::Expect, Some(1), &[], p183, 0);
        repeats.answered("A", elsewhere, 1_000_000);
        assert_eq!(repeats.notes(RUN_END)[0].dwell_us, None, "a coincidental RSeq is not an answer");
    }

    /// RFC 3261 §17.1.2.1 — a provisional moves a non-INVITE transaction to
    /// Proceeding and RESETS Timer E to T2. It slows the ladder; only a final
    /// stops it.
    #[test]
    fn a_provisional_does_not_close_a_non_invite_request() {
        let bye = b"BYE sip:b@h SIP/2.0\r\nCall-ID: c1\r\nTo: <sip:b@h>;tag=t1\r\n\
            CSeq: 77 BYE\r\n\r\n";
        let trying = b"SIP/2.0 100 Trying\r\nCall-ID: c1\r\nTo: <sip:b@h>;tag=t1\r\n\
            CSeq: 77 BYE\r\n\r\n";
        let gone = b"SIP/2.0 481 Unknown Dialog\r\nCall-ID: c1\r\nTo: <sip:b@h>;tag=t1\r\n\
            CSeq: 77 BYE\r\n\r\n";
        let mut repeats = Repeats::new();
        repeats.claim("s47", "B", LadderSide::Expect, Some(2), &[681, 3251], bye, 2_257_923_000);
        repeats.answered("B", trying, 2_261_466_000);
        repeats.answered("B", gone, 2_265_854_000);
        assert_eq!(
            (repeats.notes(RUN_END)[0].dwell_us, repeats.notes(RUN_END)[0].rfc_rungs),
            (Some(7_931_000), Some(4)),
            "the 481 closed it, and 500/1500/3500/7500 all fall inside 7931 ms"
        );
    }

    /// RFC 3261 §17.1.1.2 — Timer A runs only in Calling, which the FIRST
    /// response of any kind leaves. An INVITE's ladder therefore closes on a
    /// provisional where a BYE's does not.
    #[test]
    fn any_response_closes_an_invite_request() {
        let invite = b"INVITE sip:b@h SIP/2.0\r\nCall-ID: c1\r\nTo: <sip:b@h>\r\n\
            CSeq: 1 INVITE\r\n\r\n";
        let trying = b"SIP/2.0 100 Trying\r\nCall-ID: c1\r\nTo: <sip:b@h>;tag=t1\r\n\
            CSeq: 1 INVITE\r\n\r\n";
        let mut repeats = Repeats::new();
        repeats.claim("s3", "B", LadderSide::Expect, Some(1), &[], invite, 4_000);
        repeats.answered("B", trying, 7_915_000);
        assert_eq!(
            (repeats.notes(RUN_END)[0].dwell_us, repeats.notes(RUN_END)[0].rfc_rungs),
            (Some(7_911_000), Some(4)),
            "Timer A stops in Proceeding; 500/1500/3500/7500 fall inside 7911 ms"
        );
    }

    /// A CSeq number is unique only within a dialog, so a fork's other early
    /// dialog — same Call-ID, same CSeq, another To-tag — closes nothing (§6.5).
    #[test]
    fn another_early_dialogs_answer_closes_nothing() {
        let ok = b"SIP/2.0 200 OK\r\nCall-ID: c1\r\nTo: <sip:b@h>;tag=fork1\r\n\
            CSeq: 1 INVITE\r\n\r\n";
        let other_ack = b"ACK sip:b@h SIP/2.0\r\nCall-ID: c1\r\nTo: <sip:b@h>;tag=fork2\r\n\
            CSeq: 1 ACK\r\n\r\n";
        let own_ack = b"ACK sip:b@h SIP/2.0\r\nCall-ID: c1\r\nTo: <sip:b@h>;tag=fork1\r\n\
            CSeq: 1 ACK\r\n\r\n";
        let mut repeats = Repeats::new();
        repeats.claim("s20", "A", LadderSide::Expect, Some(1), &[], ok, 0);
        repeats.answered("A", other_ack, 500_000);
        assert_eq!(repeats.notes(RUN_END)[0].dwell_us, None, "the other fork's ACK is not ours");
        repeats.answered("A", own_ack, 900_000);
        assert_eq!(repeats.notes(RUN_END)[0].dwell_us, Some(900_000));
    }

    /// RFC 3261 §17.2.2 — a non-INVITE server transaction re-sends its final
    /// only when a retransmission of the REQUEST arrives, so that final rides no
    /// timer of its own and its repeats are drawn, not paced.
    #[test]
    fn a_non_invite_final_rides_no_ladder_of_its_own() {
        let to_bye = b"SIP/2.0 200 OK\r\nCSeq: 77 BYE\r\n\r\n";
        let to_invite = b"SIP/2.0 200 OK\r\nCSeq: 1 INVITE\r\n\r\n";
        assert_eq!(class_of(to_bye), None, "§17.2.2 draws it from the request");
        assert_eq!(class_of(to_invite), Some(Class::Final2xx), "§13.3.1.4 paces an INVITE's 2xx");
    }

    /// Past 64·T1 the transaction is gone (§17.2.1 Timer H), so a ladder nothing
    /// closed inside the envelope stops counting there.
    #[test]
    fn the_rung_count_stops_at_the_transaction_envelope() {
        let ladder = Schedule::rfc(Class::Final2xx);
        assert_eq!(
            rungs_within(&ladder, ms(31_500)),
            9,
            "500/1500/3500/7500/11500/15500/19500/23500/27500 fall inside 31.5 s"
        );
        assert_eq!(
            rungs_within(&ladder, Duration::from_secs(44)),
            10,
            "the envelope bounds it at 32 s, where a 44 s dwell would say 13"
        );
    }

    #[test]
    fn only_the_laddered_transactions_own_answer_closes_it() {
        let bye = b"BYE sip:b@h SIP/2.0\r\nCSeq: 4 BYE\r\n\r\n";
        let other = b"SIP/2.0 200 OK\r\nCSeq: 4 INVITE\r\n\r\n";
        let mut repeats = Repeats::new();
        repeats.claim("s9", "B", LadderSide::Expect, Some(1), &[], bye, 0);
        repeats.note("B", bye, 500_000);
        repeats.note("B", bye, 1_500_000);
        // Same leg, same CSeq NUMBER, another transaction: it closes nothing.
        repeats.answered("B", other, 1_508_000);
        assert_eq!(
            repeats.notes(1_600_000)[0].dwell_us,
            None,
            "a coincidental CSeq is not an answer, so nothing closed this ladder"
        );
    }
}
