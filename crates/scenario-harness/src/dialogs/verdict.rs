//! The verdicts the dialog table passes on a received in-dialog request (or a
//! 2xx answering one the UA sent), each
//! one a named rule a test opts out of with a `WaiverScope` on the UA that
//! holds the table.

use std::fmt;

use crate::realcall::LostMessage;

/// What is wrong with a received in-dialog request (RFC 3261 §12.2.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerdictKind {
    /// The request names no dialog the UA holds: an unknown remote or local
    /// tag, or an early dialog another branch's 2xx abandoned past the
    /// §13.2.2.4 window. `unknown` is the tag the UA does not hold (`From`
    /// or `To`, with its value), `held` the tags it holds in its place.
    UnknownDialog { header: &'static str, unknown: String, held: Vec<String> },
    /// The request names a dialog a non-2xx final (`by: Some(status)`) or a
    /// BYE (`by: None`) ended.
    Ended { by: Option<u16> },
    /// The request names a dialog a non-2xx final (`by: Some(status)`) or a
    /// BYE (`by: None`) ended, and the peer may have sent it before it knew
    /// of that end: an early dialog a final the UA sent ended, the peer not
    /// yet shown to have seen it (RFC 3261 §12.3); or, on a run that
    /// declares a loss model, a dialog a final or a BYE of either side ended,
    /// the request a retransmission whose lost first copy left before the
    /// peer sent or read that end (§12.2.2,
    /// §15.1.2, §17.1.2.2). The UA refuses it as it refuses
    /// [`VerdictKind::Ended`]. Judged as on the dialog before it ended, it is
    /// a finding only of the rule it `broke` there.
    Crossed { by: Option<u16>, broke: Option<Box<VerdictKind>> },
    /// The request `broke` the remote sequence (a CSeq gone backwards, or
    /// one skipping), and on a run that declares a loss model the wire record
    /// shows a loss that explains it: the network lost a first copy of the
    /// request's own transaction, overtaken since by a later request, or one
    /// request numbered each number of the gap (RFC 3261 §12.2.2,
    /// §17.1.2.2). The UA answers it as it answers
    /// `broke`; it is no finding.
    LossExplained { broke: Box<VerdictKind> },
    /// The CSeq is below the last one the peer used on this dialog.
    CseqBackwards { cseq: u32, last: u32 },
    /// The CSeq is more than one above the last one the peer used.
    CseqSkip { cseq: u32, last: u32 },
    /// A re-INVITE on an early dialog (RFC 3261 §14.1: its INVITE is still in
    /// progress), or a BYE from the callee on one (§15: the callee never BYEs
    /// an early dialog). `reinvite` tells the two apart.
    OnEarlyDialog { reinvite: bool },
    /// A `status` 2xx answered a request the UA sent in dialog, on a dialog a
    /// `by` non-2xx final had ended when the 2xx arrived (RFC 3261 §12.1.2),
    /// the request having reached the peer after that final took effect
    /// there: one the peer took before met a live dialog (§12.3).
    EndedAnswered { status: u16, by: u16 },
}

impl VerdictKind {
    /// The rule name a `WaiverScope` opts out of.
    pub fn rule(&self) -> &'static str {
        match self {
            VerdictKind::UnknownDialog { .. } => DIALOG_UNKNOWN,
            VerdictKind::Crossed { broke: Some(broke), .. }
            | VerdictKind::LossExplained { broke } => broke.rule(),
            VerdictKind::Ended { .. } | VerdictKind::Crossed { broke: None, .. } => DIALOG_ENDED,
            VerdictKind::CseqBackwards { .. } => DIALOG_CSEQ_BACKWARDS,
            VerdictKind::CseqSkip { .. } => DIALOG_CSEQ_SKIP,
            VerdictKind::OnEarlyDialog { .. } => DIALOG_EARLY_REQUEST,
            VerdictKind::EndedAnswered { .. } => DIALOG_ENDED_ANSWERED,
        }
    }

    /// Whether the verdict judges a response the UA received rather than a
    /// request.
    pub fn on_response(&self) -> bool {
        matches!(self, VerdictKind::EndedAnswered { .. })
    }

    /// Whether the verdict is a finding against the peer, gated by the
    /// harness or failed by a driver: every kind but a
    /// [`VerdictKind::Crossed`] whose broken rule, if any, is no finding, and
    /// a [`VerdictKind::LossExplained`], which only tell the UA how to answer.
    pub fn is_finding(&self) -> bool {
        match self {
            VerdictKind::Crossed { broke, .. } => broke.as_ref().is_some_and(|b| b.is_finding()),
            VerdictKind::LossExplained { .. } => false,
            _ => true,
        }
    }

    /// Whether the receiving UA refuses the request on the wire: `481` for a
    /// request on no dialog or an ended one (a crossing one whatever rule it
    /// broke), `500` for a CSeq going backwards
    /// (RFC 3261 §12.2.2), `491` for a re-INVITE while its INVITE is in
    /// progress (§14.2). A skip, a callee's early BYE and a response are
    /// taken.
    pub fn refusal(&self) -> Option<(u16, &'static str)> {
        match self {
            VerdictKind::LossExplained { broke } => broke.refusal(),
            VerdictKind::UnknownDialog { .. }
            | VerdictKind::Ended { .. }
            | VerdictKind::Crossed { .. } => Some((481, "Call/Transaction Does Not Exist")),
            VerdictKind::CseqBackwards { .. } => Some((500, "Server Internal Error")),
            VerdictKind::CseqSkip { .. } | VerdictKind::EndedAnswered { .. } => None,
            VerdictKind::OnEarlyDialog { reinvite: true } => Some((491, "Request Pending")),
            VerdictKind::OnEarlyDialog { reinvite: false } => None,
        }
    }
}

/// The rule of a request that names no dialog.
pub const DIALOG_UNKNOWN: &str = "dialog-unknown";
/// The rule of a request on a dialog a non-2xx final or a BYE ended.
pub const DIALOG_ENDED: &str = "dialog-ended";
/// The rule of a CSeq below the last remote CSeq.
pub const DIALOG_CSEQ_BACKWARDS: &str = "dialog-cseq-backwards";
/// The rule of a CSeq more than one above the last remote CSeq.
pub const DIALOG_CSEQ_SKIP: &str = "dialog-cseq-skip";
/// The rule of a re-INVITE, or a callee's BYE, on an early dialog.
pub const DIALOG_EARLY_REQUEST: &str = "dialog-early-request";
/// The rule of a 2xx to an in-dialog request on a dialog a non-2xx final
/// ended.
pub const DIALOG_ENDED_ANSWERED: &str = "dialog-ended-answered";

/// One verdict: the request it judges (method, Call-ID, tags, CSeq) and what
/// is wrong with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogVerdict {
    pub method: String,
    pub call_id: String,
    pub from_tag: String,
    pub to_tag: String,
    pub cseq: u32,
    /// Whether the request ends its subscription: a NOTIFY stating
    /// `Subscription-State: terminated` (RFC 6665 §4.1.3).
    pub terminating: bool,
    pub kind: VerdictKind,
}

impl DialogVerdict {
    /// The rule name a `WaiverScope` opts out of.
    pub fn rule(&self) -> &'static str {
        self.kind.rule()
    }

    /// Whether the loss model's drops of requests on their way to the UA,
    /// `lost`, explain this verdict: a skip over a CSeq a dropped request of
    /// the dialog carried; a CSeq going backwards, or a request on a dialog a
    /// BYE ended, that is the retransmission of a dropped copy (RFC 3261
    /// §17.1.2: it was sent before the later request or the BYE arrived). A
    /// request crossing the UA's final or BYE is explained as the rule it
    /// broke is.
    pub fn explained_by(&self, lost: &[&LostMessage]) -> bool {
        let of_dialog =
            |m: &&&LostMessage| m.call_id == self.call_id && m.from_tag == self.from_tag;
        let same =
            |m: &LostMessage| m.cseq == self.cseq && m.method.as_deref() == Some(&self.method);
        let kind = match &self.kind {
            VerdictKind::Crossed { broke: Some(broke), .. }
            | VerdictKind::LossExplained { broke } => broke.as_ref(),
            kind => kind,
        };
        match *kind {
            VerdictKind::CseqSkip { cseq, last } => {
                lost.iter().filter(of_dialog).any(|m| (last + 1..cseq).contains(&m.cseq))
            }
            VerdictKind::CseqBackwards { .. } | VerdictKind::Ended { by: None } => {
                lost.iter().filter(of_dialog).any(|m| same(m))
            }
            _ => false,
        }
    }
}

/// One line naming the request and the tags the UA holds, e.g.
/// `OPTIONS From-tag A1 names no dialog (holds A2)`.
impl fmt::Display for DialogVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let m = &self.method;
        match &self.kind {
            VerdictKind::UnknownDialog { header, unknown, held } => {
                let held = if held.is_empty() { "none".to_string() } else { held.join(", ") };
                write!(f, "{m} {header}-tag {unknown} names no dialog (holds {held})")
            }
            VerdictKind::Ended { by } => {
                let by = match by {
                    Some(status) => format!("a {status} final"),
                    None => "a BYE".to_string(),
                };
                write!(
                    f,
                    "{m} From-tag {} To-tag {} names a dialog {by} ended",
                    self.from_tag, self.to_tag
                )
            }
            VerdictKind::Crossed { by: Some(by), broke: None } => write!(
                f,
                "{m} From-tag {} To-tag {} names a dialog a {by} final ended, crossing that \
                 final",
                self.from_tag, self.to_tag
            ),
            VerdictKind::Crossed { by: None, broke: None } => write!(
                f,
                "{m} From-tag {} To-tag {} names a dialog a BYE ended, crossing that BYE",
                self.from_tag, self.to_tag
            ),
            VerdictKind::Crossed { by, broke: Some(broke) } => {
                let before = DialogVerdict { kind: broke.as_ref().clone(), ..self.clone() };
                match by {
                    Some(by) => write!(f, "{before}, crossing the {by} final that ended it"),
                    None => write!(f, "{before}, crossing the BYE that ended it"),
                }
            }
            VerdictKind::LossExplained { broke } => {
                let broken = DialogVerdict { kind: broke.as_ref().clone(), ..self.clone() };
                write!(f, "{broken}, as the network's loss of a first copy explains")
            }
            VerdictKind::CseqBackwards { cseq, last } => write!(
                f,
                "{m} CSeq {cseq} goes backwards on From-tag {} (last remote CSeq {last})",
                self.from_tag
            ),
            VerdictKind::CseqSkip { cseq, last } => write!(
                f,
                "{m} CSeq {cseq} skips on From-tag {} (last remote CSeq {last})",
                self.from_tag
            ),
            VerdictKind::OnEarlyDialog { .. } => write!(
                f,
                "{m} on the early dialog From-tag {} To-tag {} while its INVITE is in progress",
                self.from_tag, self.to_tag
            ),
            VerdictKind::EndedAnswered { status, by } => write!(
                f,
                "{status} to {m} on a dialog a {by} ended (Call-ID {}, tags {}/{})",
                self.call_id, self.from_tag, self.to_tag
            ),
        }
    }
}
