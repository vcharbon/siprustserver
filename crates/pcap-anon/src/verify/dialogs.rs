//! The dialog check: the anonymised document must tell apart every dialog
//! and transaction the raw one did. Per leg and over the whole document, the
//! distinct Call-IDs, Via branches, From tags and To tags are counted in the
//! raw document and in the anonymised one, read through the parser; every
//! count that dropped — two identifiers merged into one — is a defect,
//! counted, never shown.

use std::collections::HashSet;

use crate::model::Flows;
use crate::reading::Reading;

/// The distinct identifiers of a set of datagrams, by kind.
#[derive(Default)]
struct Identifiers {
    call_ids: HashSet<String>,
    branches: HashSet<String>,
    from_tags: HashSet<String>,
    to_tags: HashSet<String>,
}

impl Identifiers {
    fn read(&mut self, raw: &[u8], reading: &Reading) {
        let Ok(msg) = reading.parse(raw) else { return };
        self.call_ids.insert(msg.call_id().as_str().to_string());
        self.branches.extend(msg.via().iter().filter_map(|v| v.branch().map(str::to_string)));
        self.from_tags.extend(msg.from().tag().map(str::to_string));
        self.to_tags.extend(msg.to().tag().map(str::to_string));
    }

    fn counts(&self) -> [usize; 4] {
        [self.call_ids.len(), self.branches.len(), self.from_tags.len(), self.to_tags.len()]
    }
}

/// How many identifier counts — per leg and over the document, of each kind
/// — the anonymisation lowered, every datagram read under `reading`.
pub fn merged(raw: &Flows, anonymised: &Flows, reading: &Reading) -> usize {
    let (mut all_raw, mut all_out) = (Identifiers::default(), Identifiers::default());
    let mut dropped = 0;
    for (before, after) in raw.legs.iter().zip(&anonymised.legs) {
        let (mut leg_raw, mut leg_out) = (Identifiers::default(), Identifiers::default());
        for msg in &before.msgs {
            let raw = crate::model::wire(msg);
            leg_raw.read(&raw, reading);
            all_raw.read(&raw, reading);
        }
        for msg in &after.msgs {
            let raw = crate::model::wire(msg);
            leg_out.read(&raw, reading);
            all_out.read(&raw, reading);
        }
        dropped += lowered(&leg_raw, &leg_out);
    }
    dropped + lowered(&all_raw, &all_out)
}

fn lowered(before: &Identifiers, after: &Identifiers) -> usize {
    before.counts().iter().zip(after.counts()).filter(|(b, a)| a < *b).count()
}
