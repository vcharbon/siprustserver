//! The early dialogs a leg rings (RFC 3261 §12.1), keyed by the To-tag each answers under.

use super::{EarlyDialog, LegStack, StackError};
use sip_message::generators::StackDialog;
use sip_message::Method;

impl LegStack {
    /// Whether the leg rings an early dialog it can address a request on: no
    /// final has opened a dialog yet, and a provisional under a To-tag opened
    /// one on either side (RFC 3261 §12.1).
    pub fn rings_early(&self) -> bool {
        !self.has_dialog && !self.early.is_empty()
    }

    /// The early dialog a request rides: the one `early` names, else the
    /// leg's only one. None and several are refusals: which dialog a request
    /// rides decides which endpoint it reaches, and §14 has the tool infer
    /// nothing.
    pub(super) fn early_tag_for(
        &self,
        early: Option<&str>,
        method: &Method,
    ) -> Result<String, StackError> {
        if let Some(named) = early {
            return Ok(named.to_string());
        }
        match self.early.len() {
            0 => Err(StackError::NoDialog { leg: self.leg.clone(), method: method.to_string() }),
            1 => Ok(self.early[0].0.clone()),
            dialogs => Err(StackError::EarlyDialogAmbiguous {
                leg: self.leg.clone(),
                method: method.to_string(),
                dialogs,
            }),
        }
    }

    /// Open or refresh the early dialog `tag` names. A later provisional on the
    /// same fork refreshes the target and route set it carried; the dialog's own
    /// sequence number stands.
    pub(super) fn ring_early(&mut self, tag: &str, dialog: StackDialog, rseq: Option<u32>) {
        match self.early.iter_mut().find(|(known, _)| known == tag) {
            Some((_, early)) => {
                early.dialog = StackDialog { local_cseq: early.dialog.local_cseq, ..dialog };
                if rseq.is_some() {
                    early.rseq = rseq;
                }
            }
            None => self.early.push((tag.to_string(), EarlyDialog { dialog, rseq })),
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
