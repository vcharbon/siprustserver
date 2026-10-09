//! The verdicts the dialog table passes on a received in-dialog request, each
//! one a named rule a test opts out of with a `WaiverScope` on the UA that
//! holds the table.

use std::fmt;

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
    /// The CSeq is below the last one the peer used on this dialog.
    CseqBackwards { cseq: u32, last: u32 },
    /// The CSeq is more than one above the last one the peer used.
    CseqSkip { cseq: u32, last: u32 },
}

impl VerdictKind {
    /// The rule name a `WaiverScope` opts out of.
    pub fn rule(&self) -> &'static str {
        match self {
            VerdictKind::UnknownDialog { .. } => DIALOG_UNKNOWN,
            VerdictKind::Ended { .. } => DIALOG_ENDED,
            VerdictKind::CseqBackwards { .. } => DIALOG_CSEQ_BACKWARDS,
            VerdictKind::CseqSkip { .. } => DIALOG_CSEQ_SKIP,
        }
    }

    /// Whether the receiving UA refuses the request on the wire: `481` for a
    /// request on no dialog or an ended one, `500` for a CSeq going backwards
    /// (RFC 3261 §12.2.2). A skip is accepted.
    pub fn refusal(&self) -> Option<(u16, &'static str)> {
        match self {
            VerdictKind::UnknownDialog { .. } | VerdictKind::Ended { .. } => {
                Some((481, "Call/Transaction Does Not Exist"))
            }
            VerdictKind::CseqBackwards { .. } => Some((500, "Server Internal Error")),
            VerdictKind::CseqSkip { .. } => None,
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

/// One verdict: the request it judges (method, Call-ID, tags, CSeq) and what
/// is wrong with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogVerdict {
    pub method: String,
    pub call_id: String,
    pub from_tag: String,
    pub to_tag: String,
    pub cseq: u32,
    pub kind: VerdictKind,
}

impl DialogVerdict {
    /// The rule name a `WaiverScope` opts out of.
    pub fn rule(&self) -> &'static str {
        self.kind.rule()
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
        }
    }
}
