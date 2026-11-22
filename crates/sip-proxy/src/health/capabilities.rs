//! [`WorkerCapabilities`]: the `Allow`, `Supported` and `Accept` the workers
//! last advertised, on a `200` to a probe inside its reply window. The proxy
//! states them on its own answer to a supervision OPTIONS (ADR-0069), a claim
//! it relays and never widens.

use std::sync::Arc;

use arc_swap::ArcSwapOption;
use sip_message::generators::CapabilitySet;
use sip_message::types::SipHeader;

/// The workers' last advertisement; none before the first answer read.
#[derive(Debug, Default)]
pub struct WorkerCapabilities {
    last: ArcSwapOption<CapabilitySet>,
}

impl WorkerCapabilities {
    pub fn unheard() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A worker's answer carried `headers`: what it states is the workers'
    /// last advertisement, a half it does not state stated by none.
    pub fn advertised(&self, headers: &[SipHeader]) {
        self.last.store(Some(Arc::new(CapabilitySet::relayed(headers))));
    }

    /// The header lines of the last advertisement, `Allow` then `Supported`
    /// then `Accept`; none before one was read.
    pub fn lines(&self) -> Vec<SipHeader> {
        self.last.load().as_ref().map_or_else(Vec::new, |set| {
            set.lines()
                .into_iter()
                .map(|(name, value)| SipHeader {
                    name: name.as_wire_str().into(),
                    value: value.into(),
                })
                .collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(name: &str, value: &str) -> SipHeader {
        SipHeader { name: name.into(), value: value.into() }
    }

    #[test]
    fn the_last_advertisement_is_relayed_as_it_was_stated() {
        let caps = WorkerCapabilities::unheard();
        assert!(caps.lines().is_empty());
        caps.advertised(&[header("Allow", "INVITE, ACK, BYE"), header("Supported", "replaces")]);
        caps.advertised(&[
            header("Allow", "INVITE, ACK, BYE, OPTIONS"),
            header("Max-Forwards", "70"),
        ]);
        let lines: Vec<(String, String)> =
            caps.lines().into_iter().map(|h| (h.name.to_string(), h.value.to_string())).collect();
        assert_eq!(lines, [("Allow".to_string(), "INVITE, ACK, BYE, OPTIONS".to_string())]);
    }
}
