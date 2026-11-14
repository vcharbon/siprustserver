//! The [`Scope`] of an anonymisation: which addresses it maps, which texts
//! it keeps as the vocabulary of the network that captured them, and which
//! of that network's body types it reads.

use std::net::{Ipv4Addr, Ipv6Addr};

use crate::cut::Cut;
use crate::fake::address::{is_public_v4, is_reserved_v4};
use crate::identities::BodyTypes;

/// Which addresses an anonymisation maps, and which texts it keeps. The
/// default maps the public addresses and keeps every non-public range, whose
/// addresses name no one outside the network that captured them; `keeping`
/// names addresses kept whatever they are (the SUT ranges a network profile
/// states, which the cut matches the document against). `all_addresses` maps
/// every address but the reserved ranges, the kept ones included, for a
/// document that leaves the network; `keep_addresses` maps none, for a
/// document that stays on the network's own tooling with every peer as
/// captured. `keeping_texts` names the strings the
/// SUT itself writes (a network profile's `kept_texts`): vocabulary, never an
/// identity, which the anonymiser keeps as written and verify accepts.
/// `reading_bodies` names the body types the network adds (a profile's
/// `bodies`), which the anonymiser and verify read where they would
/// otherwise redact the part whole.
#[derive(Debug, Clone, Default)]
pub struct Scope {
    kept: Option<Cut>,
    all_addresses: bool,
    keep_addresses: bool,
    texts: Vec<String>,
    bodies: BodyTypes,
}

impl Scope {
    /// Read the body types `bodies` names.
    pub fn reading_bodies(mut self, bodies: &BodyTypes) -> Scope {
        self.bodies = bodies.clone();
        self
    }

    /// The body types the network adds.
    pub fn bodies(&self) -> &BodyTypes {
        &self.bodies
    }

    /// Keep every address `cut` names, unless `all_addresses` says otherwise.
    pub fn keeping(mut self, cut: &Cut) -> Scope {
        self.kept = Some(cut.clone());
        self
    }

    /// Map every address a real peer can hold, the private and kept ones too.
    pub fn all_addresses(mut self) -> Scope {
        self.all_addresses = true;
        self
    }

    /// Map no address at all: every peer stays as captured.
    pub fn keep_addresses(mut self) -> Scope {
        self.keep_addresses = true;
        self
    }

    /// Keep every text of `texts` as written.
    pub fn keeping_texts(mut self, texts: &[String]) -> Scope {
        self.texts = texts.iter().map(|t| t.trim().to_ascii_lowercase()).collect();
        self
    }

    /// Whether `text` is a kept text, compared trimmed and case-blind.
    pub fn keeps_text(&self, text: &str) -> bool {
        let text = text.trim();
        self.texts.iter().any(|kept| kept.eq_ignore_ascii_case(text))
    }

    /// Whether `addr` is mapped to a fake.
    pub fn maps_v4(&self, addr: Ipv4Addr) -> bool {
        if is_reserved_v4(addr) || self.keep_addresses {
            return false;
        }
        if self.all_addresses {
            return true;
        }
        !self.kept.as_ref().is_some_and(|cut| cut.matches(&addr.to_string())) && is_public_v4(addr)
    }

    /// Whether `addr` is mapped to a fake; an IPv4-mapped address as its
    /// IPv4 address is. Global unicast is mapped but `2001:db8::/32` (RFC
    /// 3849), whose text is kept. The documentation range `3fff::/20` (RFC
    /// 9637) is mapped like any global address, so a test written on it
    /// exercises the mapping a real global address gets.
    pub fn maps_v6(&self, addr: Ipv6Addr) -> bool {
        if let Some(v4) = addr.to_ipv4_mapped() {
            return self.maps_v4(v4);
        }
        let first = addr.segments()[0];
        if self.keep_addresses || addr.is_unspecified() || addr.is_loopback() || first >> 8 == 0xfd
        {
            return false;
        }
        self.all_addresses
            || (first & 0xe000 == 0x2000 && !(first == 0x2001 && addr.segments()[1] == 0x0db8))
    }
}
