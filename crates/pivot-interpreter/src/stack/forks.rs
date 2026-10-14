//! The early dialogs a leg rings (RFC 3261 §12.1), keyed by the To-tag each answers under.

use super::{EarlyDialog, LegStack, StackError};
use sip_message::generators::StackDialog;
use sip_message::Method;

impl LegStack {
    /// Whether the leg rings an early dialog: no 2xx has opened a dialog yet,
    /// and a provisional under a To-tag opened one on either side that no
    /// non-2xx final has ended (RFC 3261 §12.1, §12.3).
    #[cfg(test)]
    pub fn rings_early(&self) -> bool {
        !self.has_dialog && self.early.iter().any(|(_, early)| !early.ended)
    }

    /// The early dialog a request rides: the one `early` names, else the
    /// leg's only one still ringing, else, where none rings, the leg's only
    /// ended one: a peer that sends into a dialog a final already ended is
    /// answered 481 by the far side (RFC 3261 §12.2.2), and the request is
    /// composed on what that dialog held. None and several are refusals:
    /// which dialog a request rides decides which endpoint it reaches, and §14
    /// has the tool infer nothing.
    pub(super) fn early_tag_for(
        &self,
        early: Option<&str>,
        method: &Method,
    ) -> Result<String, StackError> {
        if let Some(named) = early {
            return Ok(named.to_string());
        }
        let tags = |ended: bool| -> Vec<&String> {
            self.early.iter().filter(|(_, e)| e.ended == ended).map(|(tag, _)| tag).collect()
        };
        let ringing = tags(false);
        let candidates = if ringing.is_empty() { tags(true) } else { ringing };
        match candidates.as_slice() {
            [] => Err(StackError::NoDialog { leg: self.leg.clone(), method: method.to_string() }),
            [only] => Ok((*only).clone()),
            several => Err(StackError::EarlyDialogAmbiguous {
                leg: self.leg.clone(),
                method: method.to_string(),
                dialogs: several.len(),
            }),
        }
    }

    /// Open or refresh the early dialog `tag` names, as a provisional to the
    /// INVITE of CSeq `invite` rang it. A later provisional on the same fork
    /// refreshes the target and route set it carried; the dialog's own sequence
    /// number stands. A provisional under the tag of an ENDED dialog opens a
    /// new one.
    pub(super) fn ring_early(
        &mut self,
        tag: &str,
        invite: u32,
        dialog: StackDialog,
        rseq: Option<u32>,
    ) {
        let fresh = EarlyDialog { dialog, rseq, invite, ended: false };
        match self.early.iter_mut().find(|(known, _)| known == tag) {
            Some((_, early)) if early.ended => *early = fresh,
            Some((_, early)) => {
                early.dialog = StackDialog { local_cseq: early.dialog.local_cseq, ..fresh.dialog };
                if rseq.is_some() {
                    early.rseq = rseq;
                }
            }
            None => self.early.push((tag.to_string(), fresh)),
        }
    }

    /// End the early dialogs the provisionals to the INVITE of CSeq `invite`
    /// opened: a non-2xx final to that INVITE terminates them (RFC 3261 §12.3).
    pub(super) fn end_early(&mut self, invite: u32) {
        for (_, early) in self.early.iter_mut().filter(|(_, early)| early.invite == invite) {
            early.ended = true;
        }
    }

    pub(super) fn early_dialog(&self, tag: &str) -> Option<&EarlyDialog> {
        self.early.iter().find(|(known, _)| known == tag).map(|(_, early)| early)
    }

    /// The RSeq of the last reliable provisional that rode ONE fork — what
    /// `${early:<id>.rseq}` publishes, where `${leg:<id>.rseq}` has room for
    /// only the newest of them.
    pub fn early_rseq(&self, tag: &str) -> Option<u32> {
        self.early_dialog(tag).and_then(|early| early.rseq)
    }

    /// The forks this leg is ringing, by the To-tag each answers under.
    #[cfg(test)]
    pub fn early_tags(&self) -> impl Iterator<Item = &str> {
        self.early.iter().map(|(tag, _)| tag.as_str())
    }
}

#[cfg(test)]
mod tests;
