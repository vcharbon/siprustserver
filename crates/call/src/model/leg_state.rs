//! A leg's state, read off what its INVITE and its dialogs record: the
//! [`LegState`], how the leg ends ([`ByeDisposition`]) and its INVITE's
//! outcome ([`LegDisposition`]). None of the three is stored: each is derived
//! on demand from the INVITE transaction facts (the final the leg's INVITE
//! took, a CANCEL this stack sent) and the states of the leg's dialogs.

use serde::{Deserialize, Serialize};

use super::dialog::{DialogEnd, DialogState, EndingBy};
use super::leg::Leg;

/// Where a leg stands in its INVITE usage (RFC 3261 §12, §13, §15).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LegState {
    /// The INVITE has no final, no dialog of the leg is early, nothing was
    /// cancelled.
    Trying,
    /// One of the leg's dialogs is early and the INVITE awaits its final.
    Early,
    /// One of the leg's dialogs is confirmed and no BYE ends it yet.
    Confirmed,
    /// The leg is over or being ended: its INVITE took a non-2xx final or
    /// was cancelled, or a BYE ended or is ending its dialog.
    Terminated,
}

/// The outcome of the leg's INVITE.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LegDisposition {
    /// No final yet and no CANCEL.
    Pending,
    /// A 2xx answered it.
    Bridged,
    /// This stack's CANCEL of it awaits the INVITE's final (§9.1).
    Cancelling,
    /// A non-2xx final ended it, a given-up transaction's 408 included.
    Rejected,
}

/// How a leg ended, or is ending.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ByeDisposition {
    /// This stack's BYE awaits its final (§15.1.1).
    ByeSent,
    /// The peer sent a BYE (§15.1.2).
    ByeReceived,
    /// This stack's BYE took its final.
    ByeConfirmed,
    /// This stack's BYE ended without a final: its transaction timed out,
    /// or the stack gave the dialog up.
    ByeTimeout,
    /// This stack CANCELled the leg's INVITE (§9.1).
    Cancelled,
    /// The leg's INVITE took a non-2xx final.
    Rejected,
}

impl ByeDisposition {
    /// No more SIP traffic is expected for the leg: anything but a BYE in
    /// flight.
    pub fn is_terminal(self) -> bool {
        self != ByeDisposition::ByeSent
    }
}

impl Leg {
    /// The final the leg's dialog-creating INVITE took: the one received on
    /// a leg this stack dialled, the one sent on a leg whose INVITE it
    /// received.
    pub fn invite_final(&self) -> Option<u16> {
        match &self.outbound_invite {
            Some(invite) => invite.final_received,
            None => self.invite_final_sent,
        }
    }

    /// Whether this stack CANCELled the leg's INVITE.
    pub fn cancel_sent(&self) -> bool {
        self.outbound_invite.as_ref().is_some_and(|i| i.cancel_sent)
    }

    /// Whether the INVITE this stack dialled the leg with still rings: no
    /// final and no CANCEL of it yet (RFC 3261 §9.1), whatever its dialogs.
    pub fn invite_ringing(&self) -> bool {
        self.outbound_invite.as_ref().is_some_and(|i| i.final_received.is_none() && !i.cancel_sent)
    }

    /// The leg's state: `Confirmed` while one of its dialogs is; else
    /// `Terminated` once a BYE ended or is ending one of them, its INVITE was
    /// cancelled or took a final; else `Early` while one of its dialogs is;
    /// else `Trying`.
    pub fn state(&self) -> LegState {
        let mut early = false;
        let mut ended = false;
        for d in self.dialogs.iter() {
            match d.state {
                DialogState::Confirmed => return LegState::Confirmed,
                DialogState::Early => early = true,
                DialogState::Ending(EndingBy::Bye)
                | DialogState::Ended(
                    DialogEnd::PeerBye | DialogEnd::EarlyPeerBye | DialogEnd::OurBye { .. },
                ) => ended = true,
                _ => {}
            }
        }
        if ended || self.cancel_sent() || self.invite_final().is_some() {
            LegState::Terminated
        } else if early {
            LegState::Early
        } else {
            LegState::Trying
        }
    }

    /// How the leg ended or is ending: by the BYE its dialog records, else by
    /// this stack's CANCEL, else by its INVITE's non-2xx final. `None` while
    /// nothing ends it.
    pub fn end(&self) -> Option<ByeDisposition> {
        let mut by_bye = None;
        for d in self.dialogs.iter() {
            by_bye = match d.state {
                DialogState::Ending(EndingBy::Bye) => return Some(ByeDisposition::ByeSent),
                DialogState::Ended(DialogEnd::PeerBye | DialogEnd::EarlyPeerBye) => {
                    Some(ByeDisposition::ByeReceived)
                }
                DialogState::Ended(DialogEnd::OurBye { final_status: Some(_) }) => {
                    Some(ByeDisposition::ByeConfirmed)
                }
                DialogState::Ended(DialogEnd::OurBye { final_status: None }) => {
                    Some(ByeDisposition::ByeTimeout)
                }
                _ => by_bye,
            };
        }
        by_bye
            .or_else(|| self.cancel_sent().then_some(ByeDisposition::Cancelled))
            .or_else(|| self.invite_final().filter(|s| *s >= 300).map(|_| ByeDisposition::Rejected))
    }

    /// The outcome of the leg's INVITE.
    pub fn disposition(&self) -> LegDisposition {
        match self.invite_final() {
            Some(status) if (200..300).contains(&status) => LegDisposition::Bridged,
            Some(_) => LegDisposition::Rejected,
            None if self.cancel_sent() => LegDisposition::Cancelling,
            None => LegDisposition::Pending,
        }
    }

    /// Whether the call waits on nothing from the leg before its record is
    /// released: no confirmed dialog left to BYE, no BYE or CANCEL in
    /// flight, no dialled INVITE still ringing. A caller's INVITE without a
    /// final waits on nothing: the call answers it as it ends.
    pub fn is_resolved(&self) -> bool {
        match self.state() {
            LegState::Confirmed => false,
            LegState::Early => self.outbound_invite.is_none(),
            LegState::Trying => true,
            LegState::Terminated => {
                self.end() != Some(ByeDisposition::ByeSent)
                    && self.disposition() != LegDisposition::Cancelling
            }
        }
    }
}
