//! pcap2test — a SIP capture's anonymiser and ingest.
//!
//! Consumes the upstream `sipflow --json` flow model and writes what the
//! corpus keeps of a capture ([`auto`]): the anonymised flows document, the
//! agent-readable callflow `source.txt` ([`text`], with the leg pairings
//! [`propose`] offers), and a summary of the anonymisation.
//!
//! The anonymiser ([`anonymize`]) rewrites every number under a
//! numbering-plan equivalence config ([`plan`]) and every other identity —
//! host names, addresses, charging ids, product text — under a seed keying the
//! fakes ([`fake`]), kept in the store's seed directory ([`seeds`]); its leak checks
//! are [`verify`], its quick check [`reproduce`]. A network profile
//! ([`profile`]) names the SUT addresses ([`cut`]) and texts it keeps and the
//! body types the network adds. The
//! plan, the profile and the reading the capture was extracted under
//! ([`reading`]) are the caller's: nothing here holds a deployment's data.
//!
//! SIP parsing happens upstream (`sip-pcap` via `sip-message`); this crate
//! reads a message's headers, its body parts and their framing, and the rows
//! of a named header through `sip-message`. Two readings are its own, for
//! want of an accessor there: the byte scan's positional classifiers
//! (`rewrite::fields`: the header or SDP line a byte stands on, a status
//! line's reason phrase in the head or a sipfrag), and the quoted strings of
//! a header value (`identities::quoted_all`), escaped quotes a vendor writes
//! outside one included.

pub mod anonymize;
pub mod auto;
pub mod cut;
pub mod encoded;
pub mod fake;
pub mod identities;
pub mod index;
pub mod loopback;
pub mod model;
pub mod passport;
pub mod plan;
pub mod profile;
pub mod propose;
pub mod reading;
pub mod reproduce;
pub mod rewrite;
pub mod seeds;
#[cfg(test)]
mod testflow;
pub mod text;
pub mod uui;
pub mod verify;
pub mod xml;
