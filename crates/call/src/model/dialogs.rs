//! A leg's dialogs ([`Dialogs`]) and the one way to reach them: by the two
//! tags that name a dialog, read off a received message by its direction
//! (RFC 3261 §12.2), or by the callee dialog a caller-facing one relays. No
//! accessor names "the first" dialog: a leg may hold several (forking,
//! §12.1.2), and each message names the one it belongs to.

use serde::{Deserialize, Serialize};

use super::dialog::Dialog;

/// The dialogs of one leg, in every state.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Dialogs(Vec<Dialog>);

impl Dialogs {
    pub fn new() -> Dialogs {
        Dialogs(Vec::new())
    }

    pub fn iter(&self) -> impl Iterator<Item = &Dialog> {
        self.0.iter()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Dialog> {
        self.0.iter_mut()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Add `dialog`. A dialog already named by the same two tags is replaced:
    /// two records never share an identity.
    pub fn insert(&mut self, dialog: Dialog) {
        match self
            .0
            .iter_mut()
            .find(|d| d.our_tag == dialog.our_tag && d.peer_tag == dialog.peer_tag)
        {
            Some(d) => *d = dialog,
            None => self.0.push(dialog),
        }
    }

    /// Keep only the dialogs `keep` accepts.
    pub fn retain(&mut self, keep: impl FnMut(&Dialog) -> bool) {
        self.0.retain(keep)
    }

    /// The dialog named by `our_tag` and `peer_tag`.
    pub fn by_tags(&self, our_tag: &str, peer_tag: &str) -> Option<&Dialog> {
        self.0.iter().find(|d| d.our_tag == our_tag && d.peer_tag == peer_tag)
    }

    /// The dialog named by `our_tag` and `peer_tag`, to update.
    pub fn by_tags_mut(&mut self, our_tag: &str, peer_tag: &str) -> Option<&mut Dialog> {
        self.0.iter_mut().find(|d| d.our_tag == our_tag && d.peer_tag == peer_tag)
    }

    /// The dialog a received request rides: its To-tag is ours, its From-tag
    /// the peer's. `None` when either tag is missing.
    pub fn of_request(&self, to: Option<&str>, from: Option<&str>) -> Option<&Dialog> {
        self.by_tags(to?, from?)
    }

    /// The dialog a received response belongs to: its From-tag is ours, its
    /// To-tag the peer's. `None` when either tag is missing.
    pub fn of_response(&self, to: Option<&str>, from: Option<&str>) -> Option<&Dialog> {
        self.by_tags(from?, to?)
    }

    /// The dialog a 2xx established and no BYE has completed on
    /// ([`Dialog::is_confirmed`]): at most one per leg.
    pub fn confirmed(&self) -> Option<&Dialog> {
        self.0.iter().find(|d| d.is_confirmed())
    }

    /// The confirmed dialog, to update.
    pub fn confirmed_mut(&mut self) -> Option<&mut Dialog> {
        self.0.iter_mut().find(|d| d.is_confirmed())
    }

    /// The dialog a request toward the leg rides when no dialog of the leg is
    /// named: the confirmed one, else the leg's only dialog that carries
    /// requests ([`Dialog::carries_requests`]). `None` when several early
    /// dialogs leave the choice open.
    pub fn carrying(&self) -> Option<&Dialog> {
        self.confirmed().or_else(|| {
            let mut live = self.0.iter().filter(|d| d.carries_requests());
            let only = live.next()?;
            live.next().is_none().then_some(only)
        })
    }

    /// The dialog a 2xx established, whether or not a BYE has ended it since
    /// ([`Dialog::was_established`]): at most one per leg.
    pub fn established(&self) -> Option<&Dialog> {
        self.0.iter().find(|d| d.was_established())
    }

    /// The established dialog, to update.
    pub fn established_mut(&mut self) -> Option<&mut Dialog> {
        self.0.iter_mut().find(|d| d.was_established())
    }

    /// The caller-facing dialog that relays `leg_id`'s dialog with peer tag
    /// `peer_tag` ([`Dialog::fronts`]), in any state.
    pub fn fronting(&self, leg_id: &str, peer_tag: &str) -> Option<&Dialog> {
        self.0
            .iter()
            .find(|d| d.fronts.iter().any(|f| f.leg_id == leg_id && f.peer_tag == peer_tag))
    }
}

impl From<Vec<Dialog>> for Dialogs {
    fn from(dialogs: Vec<Dialog>) -> Dialogs {
        dialogs.into_iter().collect()
    }
}

impl FromIterator<Dialog> for Dialogs {
    fn from_iter<I: IntoIterator<Item = Dialog>>(iter: I) -> Dialogs {
        let mut dialogs = Dialogs::new();
        for d in iter {
            dialogs.insert(d);
        }
        dialogs
    }
}

impl<'a> IntoIterator for &'a Dialogs {
    type Item = &'a Dialog;
    type IntoIter = std::slice::Iter<'a, Dialog>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<'a> IntoIterator for &'a mut Dialogs {
    type Item = &'a mut Dialog;
    type IntoIter = std::slice::IterMut<'a, Dialog>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter_mut()
    }
}
