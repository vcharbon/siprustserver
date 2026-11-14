//! The leak check of the names pass: every name of the RAW document the
//! pass maps — a host, a charging id, a product text, an address the scope
//! maps — is searched as a whole word in the anonymised datagrams and model
//! strings, and the count of distinct survivors is the verdict, never a name.
//! A datagram holding base64 text ([`crate::encoded`]) is harvested as the
//! anonymiser harvests it — as written, and with the text that is beyond
//! accident decoded — and searched as written and in every such text
//! decoded where it stands.

use std::net::{Ipv4Addr, Ipv6Addr};

use crate::encoded::{self, Context};
use crate::fake::address::{ipv4_octets, Scope};
use crate::fake::names::{keeps_host, keeps_icid, keeps_text};
use crate::identities;
use crate::model::Flows;
use crate::rewrite::addresses;
use crate::rewrite::bounded::BoundedSubs;
use crate::rewrite::fields::status_code_end;

/// The distinct names of `raw` the pass maps that survive in `anonymised`.
pub fn name_survivors(raw: &Flows, anonymised: &Flows, scope: &Scope) -> usize {
    let found: std::collections::HashSet<Vec<u8>> =
        name_survivor_sites(raw, anonymised, scope).into_iter().map(|s| s.name).collect();
    found.len()
}

/// One surviving name: the leg and message of the anonymised document it
/// stands in, its byte offset in that datagram (`None` where it stands in a
/// model address), and the name — for a check that locates it, never for a
/// report that prints it.
pub struct NameSite {
    pub leg: usize,
    pub msg: usize,
    pub at: Option<usize>,
    pub name: Vec<u8>,
}

/// Every occurrence of a surviving name ([`name_survivors`]).
pub fn name_survivor_sites(raw: &Flows, anonymised: &Flows, scope: &Scope) -> Vec<NameSite> {
    let needles = needles(raw, scope);
    let search = BoundedSubs::new(needles.iter().map(|(n, _)| (n.clone(), n.clone())).collect());
    let cx = Context::of(anonymised, scope.bodies());
    let mut found = Vec::new();
    for (li, leg) in anonymised.legs.iter().enumerate() {
        for (mi, msg) in leg.msgs.iter().enumerate() {
            let before = raw.legs.get(li).and_then(|l| l.msgs.get(mi));
            let pieces = |m: &crate::model::Msg| {
                [crate::model::wire(m), m.src.clone().into_bytes(), m.dst.clone().into_bytes()]
            };
            let before = before.map(pieces);
            // Protocol text is never anonymised, so never read.
            let [datagram, src, dst] = pieces(msg);
            let out = [super::without_protocol_text(&datagram, scope.bodies()), src, dst];
            for (k, bytes) in out.iter().enumerate() {
                let origins =
                    if k == 0 { crate::rewrite::origin_numbers(bytes) } else { Vec::new() };
                for (start, id) in search.found_at(bytes) {
                    // A response's status code is protocol, which no pass
                    // rewrites.
                    let (name, _) = &needles[id];
                    if k == 0 && start < status_code_end(bytes) {
                        continue;
                    }
                    // An SDP origin's session numbers are protocol, no name.
                    if origins.iter().any(|o| o.start <= start && start + name.len() <= o.end) {
                        continue;
                    }
                    // A fake spelling another name of the raw document where the
                    // raw bytes held something else is a coincidence, not a survivor.
                    let end = start + name.len();
                    let elsewhere = before.as_ref().is_some_and(|b| {
                        b[k].len() == bytes.len()
                            && !b[k][start..end].eq_ignore_ascii_case(&bytes[start..end])
                    });
                    if elsewhere {
                        continue;
                    }
                    let at = (k == 0).then_some(start);
                    found.push(NameSite { leg: li, msg: mi, at, name: name.clone() });
                }
            }
            let Some(reading) = encoded::read(&datagram, &cx) else { continue };
            let before = before.as_ref().and_then(|b| reading.alike(&b[0]));
            let bytes = super::without_protocol_text(&reading.bytes, scope.bodies());
            for (start, id) in search.found_at(&bytes) {
                let (name, end) = (&needles[id].0, start + needles[id].0.len());
                let Some(at) = reading.run_at(start).filter(|_| reading.run_at(end - 1).is_some())
                else {
                    continue;
                };
                let elsewhere = before
                    .as_ref()
                    .is_some_and(|b| !b[start..end].eq_ignore_ascii_case(&bytes[start..end]));
                if !elsewhere {
                    found.push(NameSite { leg: li, msg: mi, at: Some(at), name: name.clone() });
                }
            }
        }
    }
    found
}

/// Every distinct name of `raw` the names pass maps, each with whether it is
/// a free text only (no host or charging id spells it), as the pass takes it.
fn needles(raw: &Flows, scope: &Scope) -> Vec<(Vec<u8>, bool)> {
    let cx = Context::of(raw, scope.bodies());
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let mut texts = Vec::new();
    for msg in raw.legs.iter().flat_map(|leg| &leg.msgs) {
        let written = crate::model::wire(msg);
        let decoded = encoded::read_sure(&written, &cx).map(|reading| reading.bytes);
        for datagram in [Some(written), decoded].into_iter().flatten() {
            let names = identities::harvest(&datagram, scope.bodies()).names;
            texts
                .extend(names.texts.into_iter().filter(|t| !keeps_text(t) && !scope.keeps_text(t)));
            let words = names
                .hosts
                .into_iter()
                .filter(|h| !keeps_host(h))
                .chain(names.icids.into_iter().filter(|i| !keeps_icid(i)));
            let literals = [datagram.as_slice(), msg.src.as_bytes()]
                .into_iter()
                .flat_map(|bytes| {
                    addresses::literals(bytes)
                        .into_iter()
                        .map(|(s, e)| String::from_utf8_lossy(&bytes[s..e]).into_owned())
                        .collect::<Vec<_>>()
                })
                .filter(|literal| maps(scope, literal));
            for needle in words.chain(literals) {
                if !needle.is_empty() && seen.insert(needle.to_ascii_lowercase()) {
                    out.push((needle.into_bytes(), false));
                }
            }
        }
    }
    for text in texts {
        if !text.is_empty() && seen.insert(text.to_ascii_lowercase()) {
            out.push((text.into_bytes(), true));
        }
    }
    out
}

/// Whether the scope maps the address `literal` writes.
fn maps(scope: &Scope, literal: &str) -> bool {
    match ipv4_octets(literal) {
        Some((octets, _)) => scope.maps_v4(Ipv4Addr::from(octets)),
        None => literal.parse::<Ipv6Addr>().is_ok_and(|addr| scope.maps_v6(addr)),
    }
}
