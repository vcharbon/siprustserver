//! The names pass of the anonymiser: host names and realms,
//! charging ids, product text and addresses, mapped after the number pass
//! over the same datagram bytes and model strings, so model and bytes keep
//! agreeing. A name is matched as the number pass left it — its digits
//! already mapped — and as written, where that pass does not reach (a dialog
//! identifier's host), and replaced by a same-length fake ([`crate::fake::names`],
//! [`crate::fake::address`]), so a body keeps its Content-Length. A value
//! redacted whole (a PASSporT) is replaced by its same-length redaction. The
//! fakes the pass writes are reported ([`NamePass::fakes`]) for the leak
//! check to judge the output by.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, Ipv6Addr};

use crate::fake::address::{fake_ipv4, fake_ipv6, ipv4_octets, Scope};
use crate::fake::names::{
    fake_host_among, fake_icid_among, fake_text_among, keeps_host, keeps_icid, keeps_text, redact,
};
use crate::fake::Seed;
use crate::identities::names::Names;
use crate::rewrite::addresses;
use crate::rewrite::bounded::BoundedSubs;

/// What the names pass mapped: the distinct values of each class.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct NameCounts {
    pub hosts: usize,
    pub icids: usize,
    pub texts: usize,
    pub addresses: usize,
    /// Values redacted whole (a PASSporT).
    pub redacted: usize,
}

/// The distinct names discovery met, each class in first-occurrence order.
#[derive(Debug, Default)]
pub(super) struct Found {
    hosts: Vec<String>,
    icids: Vec<String>,
    texts: Vec<String>,
    redact: Vec<String>,
    seen: HashSet<(u8, String)>,
}

impl Found {
    /// Take one datagram's names.
    pub(super) fn absorb(&mut self, names: &Names) {
        let classes =
            [(0u8, &names.hosts), (1, &names.icids), (2, &names.texts), (3, &names.redact)];
        for (class, values) in classes {
            for value in values {
                if !value.is_empty() && self.seen.insert((class, value.clone())) {
                    match class {
                        0 => self.hosts.push(value.clone()),
                        1 => self.icids.push(value.clone()),
                        2 => self.texts.push(value.clone()),
                        _ => self.redact.push(value.clone()),
                    }
                }
            }
        }
    }
}

/// One name the pass maps: its class (`host`, `icid`, `text`, `address`) and
/// the name as the number pass left it.
pub type NameEntry = (&'static str, String);

/// One class of names: its label, the names found, what it keeps as found,
/// and its fake.
type Class<'a> =
    (&'static str, &'a Vec<String>, fn(&str) -> bool, fn(&Seed, &str, &HashSet<Vec<u8>>) -> String);

/// The compiled pass: the names to replace and the addresses the scope maps.
pub(super) struct NamePass<'a> {
    subs: BoundedSubs,
    /// The names `subs` replaces, in discovery order, with their fakes.
    named: Vec<(NameEntry, Vec<u8>)>,
    /// The fakes of the names `subs` also replaces as written, where the
    /// number pass would have spelt them otherwise.
    as_written: Vec<Vec<u8>>,
    seed: &'a Seed,
    scope: &'a Scope,
    /// Every address literal met, with its fake or `None` where it stays.
    addresses: RefCell<HashMap<String, Option<String>>>,
    /// The mapped address literals, in the order the pass met them.
    mapped_addresses: RefCell<Vec<String>>,
}

impl<'a> NamePass<'a> {
    /// The pass over `found`, each name taken as `digits` (the number pass)
    /// leaves it; no fake writes one of `numbers`, the capture's number
    /// keys, as a whole digit run.
    pub(super) fn new(
        found: Found,
        seed: &'a Seed,
        scope: &'a Scope,
        numbers: &HashSet<Vec<u8>>,
        digits: impl Fn(&[u8]) -> Vec<u8>,
    ) -> NamePass<'a> {
        let mut taken: HashSet<Vec<u8>> = HashSet::new();
        let mut pairs: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let mut named = Vec::new();
        let classes: [Class<'_>; 4] = [
            ("redact", &found.redact, |_| false, |_, value, _| redact(value)),
            ("host", &found.hosts, keeps_host, fake_host_among),
            ("icid", &found.icids, keeps_icid, fake_icid_among),
            ("text", &found.texts, keeps_text, fake_text_among),
        ];
        let mut as_written = Vec::new();
        for (class, values, keeps, fake) in classes {
            // A text the SUT writes is vocabulary, kept as written.
            for value in values.iter().filter(|v| class != "text" || !scope.keeps_text(v)) {
                // The name as the number pass left it, and as written where
                // that pass does not reach (a dialog identifier's host).
                let left = digits(value.as_bytes());
                let written = (left != value.as_bytes()).then(|| value.as_bytes().to_vec());
                for (name, alternate) in [(Some(left), false), (written, true)] {
                    let Some(name) = name else { continue };
                    let Ok(text) = std::str::from_utf8(&name) else { continue };
                    if keeps(text) || !taken.insert(name.to_ascii_lowercase()) {
                        continue;
                    }
                    let fake = fake(seed, text, numbers).into_bytes();
                    if alternate {
                        as_written.push(fake.to_ascii_lowercase());
                    } else {
                        named.push(((class, text.to_string()), fake.to_ascii_lowercase()));
                    }
                    pairs.push((name, fake));
                }
            }
        }
        NamePass {
            subs: BoundedSubs::new(pairs),
            named,
            as_written,
            seed,
            scope,
            addresses: RefCell::default(),
            mapped_addresses: RefCell::default(),
        }
    }

    /// `bytes` with every name and every mapped address replaced.
    pub(super) fn apply(&self, bytes: &[u8]) -> Vec<u8> {
        addresses::rewrite(&self.subs.apply(bytes), &|literal| self.address(literal))
    }

    /// The fake of an address literal, or `None` where the scope keeps it.
    fn address(&self, literal: &str) -> Option<String> {
        if let Some(known) = self.addresses.borrow().get(literal) {
            return known.clone();
        }
        let fake = match ipv4_octets(literal) {
            Some((octets, _)) => self
                .scope
                .maps_v4(Ipv4Addr::from(octets))
                .then(|| fake_ipv4(self.seed, literal))
                .flatten(),
            None => literal
                .parse::<Ipv6Addr>()
                .ok()
                .filter(|addr| self.scope.maps_v6(*addr))
                .and_then(|_| fake_ipv6(self.seed, literal)),
        };
        self.addresses.borrow_mut().insert(literal.to_string(), fake.clone());
        if fake.is_some() {
            self.mapped_addresses.borrow_mut().push(literal.to_string());
        }
        fake
    }

    /// Every name the pass mapped so far, the addresses last.
    pub(super) fn entries(&self) -> Vec<NameEntry> {
        let addresses = self.mapped_addresses.borrow();
        self.named
            .iter()
            .map(|(entry, _)| entry.clone())
            .chain(addresses.iter().map(|a| ("address", a.clone())))
            .collect()
    }

    /// Every fake the pass wrote so far, names and addresses, lowercase.
    pub(super) fn fakes(&self) -> Vec<Vec<u8>> {
        let addresses = self.addresses.borrow();
        self.named
            .iter()
            .map(|(_, fake)| fake.clone())
            .chain(self.as_written.iter().cloned())
            .chain(addresses.values().flatten().map(|f| f.to_ascii_lowercase().into_bytes()))
            .collect()
    }

    /// The names and addresses mapped so far that share their fake with
    /// another: distinct identities the document no longer tells apart.
    pub(super) fn collisions(&self) -> usize {
        let addresses = self.addresses.borrow();
        let fakes = self
            .named
            .iter()
            .map(|(_, fake)| fake.clone())
            .chain(addresses.values().flatten().map(|f| f.to_ascii_lowercase().into_bytes()));
        let mut seen: HashMap<Vec<u8>, usize> = HashMap::new();
        for fake in fakes {
            *seen.entry(fake).or_default() += 1;
        }
        seen.values().filter(|&&n| n > 1).sum()
    }
}

impl NameCounts {
    /// The counts of `entries`, class by class.
    pub fn of(entries: &[NameEntry]) -> NameCounts {
        let count = |class: &str| entries.iter().filter(|(c, _)| *c == class).count();
        NameCounts {
            hosts: count("host"),
            icids: count("icid"),
            texts: count("text"),
            addresses: count("address"),
            redacted: count("redact"),
        }
    }
}

#[cfg(test)]
mod number_probe_tests;
#[cfg(test)]
mod probe_tests;
#[cfg(test)]
mod tests;
