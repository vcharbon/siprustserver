//! An agent's session streams: the descriptions it sent on a call, one stream
//! per dialog (RFC 3264 §8 constrains the successive descriptions of ONE
//! session, and each dialog an INVITE creates carries its own, RFC 3261 §12).

use std::collections::BTreeMap;

use crate::wire::Msg;

/// Whether the description a message carries is a step of a session: it rides
/// an `INVITE`, `ACK`, `PRACK` or `UPDATE` transaction (RFC 3261 §13.2.1, RFC
/// 3262 §5, RFC 3311 §5.1) on a request, a provisional or a 2xx. Any other
/// method's body describes no session — an `OPTIONS` body is a capability set
/// (RFC 3261 §11.2) — and neither does a redirect or failure final's (a `488`
/// lists media capabilities, §21.4.26): neither is a step of any stream.
pub(super) fn steps_the_session(msg: &Msg) -> bool {
    let method = msg.cseq_method.as_str();
    ["INVITE", "ACK", "PRACK", "UPDATE"].iter().any(|m| method.eq_ignore_ascii_case(m))
        && msg.status().is_none_or(|status| status < 300)
}

/// The dialog a message rides: its tag pair taken unordered, since a request
/// the callee sends inside the dialog swaps the From and To tags (RFC 3261
/// §12.2.1.1). `None` on a message with no To tag — an initial INVITE — which
/// rides no dialog yet.
fn dialog_of(msg: &Msg) -> Option<(&str, &str)> {
    let to = msg.to_tag.as_deref()?;
    let from = msg.from_tag.as_deref().unwrap_or_default();
    Some(if from <= to { (from, to) } else { (to, from) })
}

/// The tag the sender of `msg` holds in the dialog: the From tag of a request,
/// the To tag of a response (RFC 3261 §12.1).
fn local_tag(msg: &Msg) -> Option<&str> {
    match msg.status() {
        None => msg.from_tag.as_deref(),
        Some(_) => msg.to_tag.as_deref(),
    }
}

/// One session stream of one agent: every description it sent there, readable
/// or not, in send order.
pub(super) struct SessionStream {
    /// Indexes into the view's `msgs`: the stream's prior, then from
    /// [`Self::own_from`] its own steps.
    pub(super) msgs: Vec<usize>,
    /// The first of `msgs` that is a step of THIS stream. A dialog's prior is
    /// the agent's tagless descriptions, each judged once, on the tagless
    /// stream; the tagless stream has no prior.
    pub(super) own_from: usize,
}

/// One agent's session streams on one call.
///
/// A description on a message with no To tag (an initial INVITE, a retry after
/// a challenge) continues the TAGLESS stream. The first description the agent
/// sends on a dialog opens that dialog's stream; where the agent holds the
/// tagless request's own tag there — it is that dialog's caller — the tagless
/// descriptions sent so far are its prior: an INVITE's description is the prior
/// of every dialog its forks create. The dialog's later descriptions continue
/// it alone. A tagless description sent after a dialog's stream opened is no
/// step of that dialog, and a response an agent forwards on a dialog it is not
/// the caller of continues no request of its own.
#[derive(Default)]
pub(super) struct SessionStreams<'a> {
    tagless: Vec<usize>,
    /// The From tag of the agent's tagless requests.
    tagless_tag: Option<&'a str>,
    dialogs: BTreeMap<(&'a str, &'a str), SessionStream>,
}

impl<'a> SessionStreams<'a> {
    /// Record a description the agent sent that steps the session.
    pub(super) fn push(&mut self, msg: &'a Msg, mi: usize) {
        let Some(dialog) = dialog_of(msg) else {
            self.tagless.push(mi);
            self.tagless_tag = self.tagless_tag.or(msg.from_tag.as_deref());
            return;
        };
        let prior = if local_tag(msg) == self.tagless_tag { self.tagless.as_slice() } else { &[] };
        self.dialogs
            .entry(dialog)
            .or_insert_with(|| SessionStream { msgs: prior.to_vec(), own_from: prior.len() })
            .msgs
            .push(mi);
    }

    /// Every stream that holds a step of its own: the tagless one, then one per
    /// dialog.
    pub(super) fn into_streams(self) -> Vec<SessionStream> {
        let tagless = SessionStream { msgs: self.tagless, own_from: 0 };
        std::iter::once(tagless)
            .chain(self.dialogs.into_values())
            .filter(|s| s.msgs.len() > s.own_from)
            .collect()
    }
}
