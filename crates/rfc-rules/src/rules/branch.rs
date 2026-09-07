//! The per-branch reading: what ONE endpoint did on ONE top-Via branch.
//!
//! RFC 3261 §17 names a transaction by its top-Via `branch`, and every rule
//! that pairs two messages of one transaction keys on it. Six families do: the
//! §9.1 CANCEL pair ([`super::cancel`]), the §13.2.2.4 / §17.1.1.3 ACK-carries
//! pair ([`super::ack`]), the §16.4 strict-route rewrite ([`super::proxy`]), the
//! §10.2 REGISTER serialisation ([`super::register`]), the §14.1 abandoned
//! re-INVITE ([`super::reinvite`]) and the §20.32 Require-on-ACK read
//! ([`super::capability`]), which needs the branch's final status to know which
//! ACK it is looking at. They share this ONE walk.
//!
//! **It lives with the rules, not in [`crate::wire`].** The wire model is what
//! the two adapters FILL; this is a reading DERIVED from it, identical for
//! every consumer and influenced by none of them. Promoting it to the model
//! would put a rule-shaped answer inside the thing rules are written against.
//!
//! **The key is (emitter, call, branch)** — three parts, never two. A view
//! carries many calls (a live bind serves every dialog on its socket), and both
//! ends of a hop see the one branch, so the endpoint judged is part of the
//! identity: an endpoint is measured against what IT sent, never against a
//! peer's copy of the same transaction.
//!
//! **Repeats are not fresh events.** A message the adapter marked
//! [`Msg::repeat`] is absorbed by nothing here — retransmitting until answered
//! is required behaviour, never a second act — so each consumer's occasions are
//! one per distinct send.
//!
//! **A message with no top-Via branch keys nothing.** The branch IS the
//! pairing; without one no pair can be formed, and the reading states nothing
//! rather than guessing at one.

use std::collections::BTreeMap;

use crate::wire::Msg;

/// One branch as ONE endpoint drove it: the call it rides and the top-Via
/// branch its transaction is named by. Spelled out as a struct so the three
/// parts can never swap places.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct BranchKey<'a> {
    /// The endpoint judged: the one that SENT the requests on the branch.
    pub emitter: &'a str,
    pub call_id: &'a str,
    pub branch: &'a str,
}

impl<'a> BranchKey<'a> {
    /// The branch `msg`'s SENDER drives — a request going out.
    pub fn sent_by(msg: &'a Msg, branch: &'a str) -> Self {
        BranchKey { emitter: msg.src.as_str(), call_id: msg.call_id.as_str(), branch }
    }

    /// The branch `msg`'s RECIPIENT drives — a response coming back.
    pub fn taken_by(msg: &'a Msg, branch: &'a str) -> Self {
        BranchKey { emitter: msg.dst.as_str(), call_id: msg.call_id.as_str(), branch }
    }
}

/// One request an endpoint sent on a branch.
#[derive(Debug, Clone, Copy)]
pub(super) struct Sent<'a> {
    /// Index into the view's `msgs`.
    pub msg: usize,
    pub hop: usize,
    pub ts_us: u64,
    pub cseq: u32,
    /// The REQUEST-LINE method, as the wire spelled it.
    pub method: &'a str,
    /// Where it went — the other side of any obligation it opens.
    pub taker: &'a str,
}

/// What one endpoint did on one branch: the requests it sent, in view order,
/// and where its first provisional and first final answer landed.
#[derive(Debug, Default)]
pub(super) struct Branch<'a> {
    sent: Vec<Sent<'a>>,
    /// View index of the first provisional it took on the branch.
    pub first_1xx: Option<usize>,
    /// View index of the first final (≥ 200) it took on the branch — where the
    /// transaction stopped being in flight.
    pub first_final: Option<usize>,
}

impl<'a> Branch<'a> {
    /// The FIRST request of `method` the endpoint sent on the branch: what a
    /// later message of the same transaction is compared against.
    pub fn first_sent(&self, method: &str) -> Option<&Sent<'a>> {
        self.sent.iter().find(|s| s.method.eq_ignore_ascii_case(method))
    }

    /// Every request of `method` it sent, in view order.
    pub fn sent_of<'s>(&'s self, method: &'s str) -> impl Iterator<Item = &'s Sent<'a>> {
        self.sent.iter().filter(move |s| s.method.eq_ignore_ascii_case(method))
    }
}

/// What one view's messages say about the branches each endpoint drove.
/// Collected in one pass, so every rule that pairs on a branch reads the same
/// walk.
#[derive(Debug, Default)]
pub(super) struct BranchReading<'a> {
    pub branches: BTreeMap<BranchKey<'a>, Branch<'a>>,
}

impl<'a> BranchReading<'a> {
    pub fn of(msgs: &'a [Msg]) -> Self {
        let mut seen = BranchReading::default();
        for (mi, msg) in msgs.iter().enumerate() {
            seen.absorb(mi, msg);
        }
        seen
    }

    /// The branch one endpoint drove, or `None` where this vantage carried none
    /// of that endpoint's traffic on it.
    pub fn at(&self, key: &BranchKey<'a>) -> Option<&Branch<'a>> {
        self.branches.get(key)
    }

    /// Absorb one message onto the branch it names.
    fn absorb(&mut self, mi: usize, msg: &'a Msg) {
        if msg.repeat {
            return;
        }
        let Some(branch) = msg.via_branch.as_deref().filter(|b| !b.is_empty()) else { return };
        if let Some(status) = msg.status() {
            // A response belongs to the endpoint that TOOK it — the UAC side of
            // the branch. Its own arrival order in the view is what a "before"
            // test reads.
            let taken = self.branches.entry(BranchKey::taken_by(msg, branch)).or_default();
            if (100..200).contains(&status) {
                taken.first_1xx.get_or_insert(mi);
            } else {
                taken.first_final.get_or_insert(mi);
            }
            return;
        }
        let Some(method) = request_method(msg) else { return };
        self.branches.entry(BranchKey::sent_by(msg, branch)).or_default().sent.push(Sent {
            msg: mi,
            hop: msg.hop,
            ts_us: msg.at_us,
            cseq: msg.cseq,
            method,
            taker: msg.dst.as_str(),
        });
    }
}

/// The request-line method, or `None` for a response.
fn request_method(msg: &Msg) -> Option<&str> {
    match &msg.kind {
        crate::wire::Kind::Request { method } => Some(method.as_str()),
        crate::wire::Kind::Response { .. } => None,
    }
}
