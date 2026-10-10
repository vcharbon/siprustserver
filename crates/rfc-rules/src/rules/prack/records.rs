//! The rows the family's reading collects, one per wire fact.

/// One 100rel obligation: the endpoint that owes the PRACK, the transaction
/// and the early dialog it rides, and the `RSeq` the PRACK's RAck must name.
/// Spelled out as a struct so the four parts can never swap places.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct ObligationKey<'a> {
    pub(super) uac: &'a str,
    /// The Call-ID: one endpoint may carry several calls, each numbering its
    /// CSeqs on its own.
    pub(super) call: &'a str,
    pub(super) cseq: u32,
    /// The early dialog's To tag: two forks answering one INVITE number their
    /// RSeqs independently, so the tag is what keeps their obligations apart.
    pub(super) dialog: &'a str,
    pub(super) rseq: u64,
}

/// One server transaction as ONE endpoint served it: RFC 3261 §17 names a
/// transaction by its top-Via branch, and the endpoint judged is part of the
/// identity because both ends of a hop see the one branch. Spelled out as a
/// struct so the three parts can never swap places.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct TxnKey<'a> {
    /// The endpoint judged: the UAS side, which takes the request and answers.
    pub(super) uas: &'a str,
    pub(super) call_id: &'a str,
    pub(super) branch: &'a str,
}

/// One request an endpoint TOOK on a server transaction.
#[derive(Debug, Clone, Copy)]
pub(super) struct Served<'a> {
    pub(super) msg: usize,
    pub(super) ts_us: u64,
    /// The request-line method, as the wire spelled it.
    pub(super) method: &'a str,
    /// It listed `100rel` in `Require`: §3 then obliges reliable provisionals
    /// or a 420.
    pub(super) requires_100rel: bool,
    /// It listed `100rel` in `Require` OR in `Supported` — the client opt-in a
    /// reliable provisional needs at all.
    pub(super) offers_100rel: bool,
    /// The early dialog it names — its To tag, empty on a dialog-creating
    /// request. A PRACK acknowledges within the dialog the provisional opened
    /// (RFC 3262 §5), and two forks number their `RSeq`s independently.
    pub(super) dialog: &'a str,
    /// What its `RAck` names, where the request is a PRACK carrying a readable
    /// one.
    pub(super) rack: Option<Rack>,
}

/// The three tokens of a PRACK's `RAck` (RFC 3262 §7.2): the response-num, the
/// CSeq-num, and whether the method token is `INVITE` — the only method a
/// reliable provisional answers, so any other names nothing a UAS sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Rack {
    pub(super) rseq: u64,
    pub(super) cseq: u32,
    pub(super) on_invite: bool,
}

/// One response an endpoint SENT on a server transaction.
#[derive(Debug, Clone, Copy)]
pub(super) struct Answer<'a> {
    pub(super) msg: usize,
    pub(super) hop: usize,
    pub(super) ts_us: u64,
    pub(super) status: u16,
    pub(super) cseq: u32,
    /// Where it went — the other side of any obligation it settles.
    pub(super) taker: &'a str,
    /// The CSeq method, as the wire spelled it: a PRACK's response and its
    /// INVITE's ride different branches, and a rule states which it judges.
    pub(super) method: &'a str,
    /// It carried `Require: 100rel` — the reliable-provisional marker.
    pub(super) requires_100rel: bool,
    /// It named `100rel` in `Unsupported` — what the 420 rejecting the
    /// extension carries (§3's other half of the disjunction).
    pub(super) rejects_100rel: bool,
    pub(super) rseq: Option<u64>,
    /// The early dialog it presents — the To tag that keeps two forks
    /// answering one INVITE apart.
    pub(super) to_tag: &'a str,
    /// Its head declared a body: `Content-Length` above zero, or a
    /// `Content-Type` where no length reads.
    pub(super) has_body: bool,
    /// The vantage recorded its headers at all — without them nothing here says
    /// which option tags it carried.
    pub(super) readable: bool,
}

impl Answer<'_> {
    /// A reliable provisional (RFC 3262 §3): a non-100 provisional on an INVITE
    /// transaction carrying `Require: 100rel`.
    pub(super) fn reliable_1xx(&self) -> bool {
        self.answers("INVITE") && (101..200).contains(&self.status) && self.requires_100rel
    }

    pub(super) fn answers(&self, method: &str) -> bool {
        self.method.eq_ignore_ascii_case(method)
    }
}

/// One `100 Trying` an endpoint took carrying the reliable-provisional markers
/// a 100 may never carry.
#[derive(Debug, Clone, Copy)]
pub(super) struct Trying<'a> {
    pub(super) msg: usize,
    pub(super) ts_us: u64,
    pub(super) cseq: u32,
    pub(super) rseq: u64,
    /// Who sent it — the other side of the occasion.
    pub(super) peer: &'a str,
}

/// One INVITE as this view carried it.
#[derive(Debug, Clone, Copy)]
pub(super) struct Invite {
    /// It named `100rel` in `Require` or in `Supported`.
    pub(super) offers_100rel: bool,
}

/// One reliable provisional as this view carried it.
#[derive(Debug, Clone, Copy)]
pub(super) struct Reliable<'a> {
    pub(super) msg: usize,
    pub(super) hop: usize,
    pub(super) ts_us: u64,
    pub(super) status: u16,
    /// The INVITE CSeq number whose `RSeq` space this provisional indexes.
    pub(super) cseq: u32,
    pub(super) rseq: u64,
    /// The endpoint on the other side of the obligation: for one the UAC took,
    /// the UAS that owes the retransmissions; for one the UAS sent, the UAC
    /// that owes the PRACK.
    pub(super) peer: &'a str,
}

/// One PRACK as this view carried it.
#[derive(Debug, Clone)]
pub(super) struct Prack<'a> {
    pub(super) msg: usize,
    pub(super) hop: usize,
    pub(super) ts_us: u64,
    pub(super) src: &'a str,
    pub(super) dst: &'a str,
    /// The call it rides — a view carries many.
    pub(super) call_id: &'a str,
    /// The early dialog's To tag.
    pub(super) dialog: &'a str,
    /// The `(RSeq, INVITE CSeq number)` its `RAck` names, or `None` where no
    /// reader accepts the header.
    pub(super) rack: Option<(u64, u32)>,
    /// The FULL `RAck` triple `(response-num, CSeq-num, method)`, the method
    /// ASCII-uppercased — what identifies one PRACK as the copy of another,
    /// whatever method the header names.
    pub(super) rack_triple: Option<(u64, u32, String)>,
}

impl Prack<'_> {
    /// The `RSeq` its `RAck` acknowledges, whatever method the header names.
    pub(super) fn racked_rseq(&self) -> Option<u64> {
        self.rack_triple.as_ref().map(|(rseq, _, _)| *rseq)
    }
}
