//! The query AST: what a query IS, independent of how it is written down or
//! how it is evaluated. JSON loading lives in [`super::load`], evaluation in
//! [`super::eval`].
//!
//! JSON is the canonical form deliberately — a text query language can be
//! added later as a parser that produces this tree, and the evaluator never
//! learns that a parser exists.
//!
//! A predicate tree is evaluated against a BINDING (a call group, one of its
//! legs, one transaction, one message). Quantifier nodes rebind to a narrower
//! scope; leaves read whatever the current binding offers. Leaves that read
//! "the same thing" at several scopes — an R-URI, a final status — are ONE
//! variant resolved per binding, so a query says what it means rather than
//! naming the level it means it at.

use crate::flow::FlowConfig;
use crate::txn::TxnKind;

/// A whole query document.
#[derive(Debug, Clone)]
pub struct Query {
    pub name: Option<String>,
    /// Search-space narrowing applied before any predicate runs.
    pub scope: Scope,
    /// Correlation pipeline override; `None` keeps the caller's default.
    pub correlate: Option<FlowConfig>,
    pub select: Node,
    pub project: Projection,
    /// Expand each hit into the calls that resemble it.
    pub neighbours: Option<Neighbours>,
}

/// Bounds evaluated before predicates, so a corpus query can skip whole
/// captures instead of walking their groups.
#[derive(Debug, Clone, Default)]
pub struct Scope {
    /// Capture-time bounds, microseconds since the epoch, inclusive.
    pub from_us: Option<u64>,
    pub to_us: Option<u64>,
}

impl Scope {
    /// Whether a group's first activity falls inside the bounds.
    pub fn admits(&self, t0_us: u64) -> bool {
        self.from_us.is_none_or(|f| t0_us >= f) && self.to_us.is_none_or(|t| t0_us <= t)
    }
}

/// "Similar" is a key the caller names, not a notion the engine owns: hits are
/// expanded to the calls sharing that key within `window_us`.
#[derive(Debug, Clone)]
pub struct Neighbours {
    /// Fields whose combined value defines sameness.
    pub key: Vec<KeyField>,
    /// How far either side of a hit to look.
    pub window_us: u64,
    /// Max neighbours per hit; 0 is unlimited.
    pub max: usize,
}

/// A projectable field of a matched call group. Also the vocabulary
/// [`Neighbours::key`] draws on, so "group by what I selected" needs no
/// second field language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyField {
    /// Index of the group within its capture.
    Group,
    /// First activity, microseconds since the epoch.
    T0Us,
    /// First activity to last, milliseconds.
    DurMs,
    LegCount,
    CallId,
    Ruri,
    /// The R-URI's user part — the dialed number, host-insensitive.
    RuriUser,
    FromUri,
    ToUri,
    ToUser,
    FromUser,
    /// Source socket of each leg's first message.
    Src,
    Dst,
    FinalStatus,
    Saw180,
    TerminatedBy,
    MsgCount,
    Hops,
    /// Correlation evidence kinds that joined the group.
    Evidence,
    /// Application-server socket named by derived-Call-ID evidence.
    AsSocket,
    PeerSocket,
}

impl KeyField {
    /// The wire spelling used in queries.
    pub fn as_str(&self) -> &'static str {
        match self {
            KeyField::Group => "group",
            KeyField::T0Us => "t0_us",
            KeyField::DurMs => "dur_ms",
            KeyField::LegCount => "leg_count",
            KeyField::CallId => "call_id",
            KeyField::Ruri => "ruri",
            KeyField::RuriUser => "ruri_user",
            KeyField::FromUri => "from",
            KeyField::ToUri => "to",
            KeyField::ToUser => "to_user",
            KeyField::FromUser => "from_user",
            KeyField::Src => "src",
            KeyField::Dst => "dst",
            KeyField::FinalStatus => "final",
            KeyField::Saw180 => "ring",
            KeyField::TerminatedBy => "term",
            KeyField::MsgCount => "msgs",
            KeyField::Hops => "hops",
            KeyField::Evidence => "evidence",
            KeyField::AsSocket => "as_socket",
            KeyField::PeerSocket => "peer_socket",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        const ALL: [KeyField; 21] = [
            KeyField::Group,
            KeyField::T0Us,
            KeyField::DurMs,
            KeyField::LegCount,
            KeyField::CallId,
            KeyField::Ruri,
            KeyField::RuriUser,
            KeyField::FromUri,
            KeyField::ToUri,
            KeyField::ToUser,
            KeyField::FromUser,
            KeyField::Src,
            KeyField::Dst,
            KeyField::FinalStatus,
            KeyField::Saw180,
            KeyField::TerminatedBy,
            KeyField::MsgCount,
            KeyField::Hops,
            KeyField::Evidence,
            KeyField::AsSocket,
            KeyField::PeerSocket,
        ];
        ALL.into_iter().find(|f| f.as_str() == s)
    }

    /// Every spelling, for an error message that lists the alternatives.
    pub fn names() -> String {
        const ALL: [KeyField; 21] = [
            KeyField::Group,
            KeyField::T0Us,
            KeyField::DurMs,
            KeyField::LegCount,
            KeyField::CallId,
            KeyField::Ruri,
            KeyField::RuriUser,
            KeyField::FromUri,
            KeyField::ToUri,
            KeyField::ToUser,
            KeyField::FromUser,
            KeyField::Src,
            KeyField::Dst,
            KeyField::FinalStatus,
            KeyField::Saw180,
            KeyField::TerminatedBy,
            KeyField::MsgCount,
            KeyField::Hops,
            KeyField::Evidence,
            KeyField::AsSocket,
            KeyField::PeerSocket,
        ];
        ALL.iter().map(|f| f.as_str()).collect::<Vec<_>>().join(", ")
    }
}

/// What a match is turned into. The three modes are the three phases a corpus
/// sweep needs: how many, which ones (cheap), and the whole thing (expensive).
#[derive(Debug, Clone)]
pub enum Projection {
    /// Matched group count only.
    Count,
    /// The named fields per matched group — the screening pass.
    Summary { fields: Vec<KeyField> },
    /// The full flow model restricted to matched groups, raw payloads
    /// included — the extraction pass.
    Full,
}

impl Default for Projection {
    fn default() -> Self {
        Projection::Summary {
            fields: vec![
                KeyField::T0Us,
                KeyField::CallId,
                KeyField::Ruri,
                KeyField::FromUri,
                KeyField::ToUri,
                KeyField::FinalStatus,
            ],
        }
    }
}

/// A predicate node. Quantifiers rebind the scope their child evaluates in;
/// everything else reads the current binding.
#[derive(Debug, Clone)]
pub enum Node {
    /// Constant — `{"all": []}` and `{"any": []}` reduce to these.
    Always(bool),
    All(Vec<Node>),
    Any(Vec<Node>),
    Not(Box<Node>),

    /// Group → each leg.
    AnyLeg(Box<Node>),
    /// Group or leg → each transaction.
    AnyTxn(Box<Node>),
    /// Group, leg or transaction → each message.
    AnyMsg(Box<Node>),
    /// Transaction → its request.
    Request(Box<Node>),
    /// Transaction → each of its responses.
    AnyResponse(Box<Node>),

    /// Group → how many legs.
    CountLeg(NumCmp),
    /// Group or leg → how many transactions satisfy `filter` (all, if absent).
    CountTxn { filter: Option<Box<Node>>, count: NumCmp },

    // --- leaves, resolved against whatever the binding offers ---
    /// Correlation evidence kind on the group.
    EvidenceKind(String),
    /// Application-server socket from derived-Call-ID evidence.
    AsSocket(StrMatch),
    CallId(StrMatch),
    /// Request-URI: the leg's INVITE, a transaction's request, or a message.
    Ruri(StrMatch),
    FromUri(StrMatch),
    ToUri(StrMatch),
    /// Leg or transaction final response.
    FinalStatus(StatusMatch),
    /// Response status of one message.
    Status(StatusMatch),
    Saw180(bool),
    TerminatedBy(StrMatch),
    /// Leg first-to-last activity.
    DurationUs(NumCmp),
    /// Transaction request → first final response.
    LatencyUs(NumCmp),
    TxnKindIs(TxnKind),
    /// Request method (of a transaction or a message).
    MethodIs(String),
    IsRequest(bool),
    Retx(bool),
    /// A header of the bound message, or of any message of a wider binding.
    Header { name: String, value: StrMatch },
    /// The bound message's body — the SDP predicate.
    Body(StrMatch),
    Src(StrMatch),
    Dst(StrMatch),
    /// Q.850 cause carried in a `Reason` header.
    ReasonCause(NumCmp),
}

/// How a string leaf compares.
#[derive(Debug, Clone)]
pub enum StrMatch {
    Equals(String),
    Contains(String),
    Prefix(String),
    Suffix(String),
    /// Matches when the field is absent or empty.
    Absent,
}

impl StrMatch {
    /// Test an existing value. `Absent` is false for any present non-empty
    /// value; a missing field never reaches here (the caller handles it).
    pub fn test(&self, value: &str) -> bool {
        match self {
            StrMatch::Equals(s) => value == s,
            StrMatch::Contains(s) => value.contains(s.as_str()),
            StrMatch::Prefix(s) => value.starts_with(s.as_str()),
            StrMatch::Suffix(s) => value.ends_with(s.as_str()),
            StrMatch::Absent => value.is_empty(),
        }
    }

    /// Test a possibly-absent value — the only form that can satisfy `Absent`.
    pub fn test_opt(&self, value: Option<&str>) -> bool {
        match (self, value) {
            (StrMatch::Absent, None) => true,
            (_, None) => false,
            (_, Some(v)) => self.test(v),
        }
    }
}

/// A numeric comparison; every present bound must hold.
#[derive(Debug, Clone, Default)]
pub struct NumCmp {
    pub eq: Option<u64>,
    pub ne: Option<u64>,
    pub ge: Option<u64>,
    pub le: Option<u64>,
    pub gt: Option<u64>,
    pub lt: Option<u64>,
}

impl NumCmp {
    pub fn test(&self, v: u64) -> bool {
        self.eq.is_none_or(|b| v == b)
            && self.ne.is_none_or(|b| v != b)
            && self.ge.is_none_or(|b| v >= b)
            && self.le.is_none_or(|b| v <= b)
            && self.gt.is_none_or(|b| v > b)
            && self.lt.is_none_or(|b| v < b)
    }

    /// True when no bound was given — an empty comparison constrains nothing.
    pub fn is_empty(&self) -> bool {
        self.eq.is_none()
            && self.ne.is_none()
            && self.ge.is_none()
            && self.le.is_none()
            && self.gt.is_none()
            && self.lt.is_none()
    }
}

/// A response-status leaf: a code, a class, a comparison, or the absence of
/// any final response at all (the timeout query).
#[derive(Debug, Clone)]
pub enum StatusMatch {
    /// No final response was ever seen.
    NoneSeen,
    /// A class such as `4xx`.
    Class(u16),
    Cmp(NumCmp),
}

impl StatusMatch {
    pub fn test(&self, status: Option<u16>) -> bool {
        match (self, status) {
            (StatusMatch::NoneSeen, None) => true,
            (StatusMatch::NoneSeen, Some(_)) => false,
            (_, None) => false,
            (StatusMatch::Class(c), Some(s)) => s / 100 == *c,
            (StatusMatch::Cmp(n), Some(s)) => n.test(s as u64),
        }
    }
}
