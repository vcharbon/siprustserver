//! **TEST-ONLY.** One parse of a capture's wire entries, for the predicates
//! that read what was captured around a message: every message's
//! transaction and dialog, and the finals each transaction drew.
//!
//! Every read delegates to [`msg_reads`] over the lenient parse the audit
//! itself makes, so a predicate reads a message as the rule that flagged it.

use std::collections::HashMap;

use sip_message::{Method, SipMessage, SipParser};

use super::msg_reads;
use crate::report::RecordedSipEntry;

/// A transaction as one hop names it: Call-ID, CSeq number, method and top
/// Via branch (RFC 3261 §17.1.3, §17.2.3). A response names the transaction
/// of its CSeq method.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TxnKey {
    pub call_id: String,
    pub cseq: u32,
    pub method: Method,
    pub branch: String,
}

/// A dialog as the wire names it (ADR-0041): Call-ID, From-tag and To-tag,
/// an absent tag empty.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DialogKey {
    pub call_id: String,
    pub from_tag: String,
    pub to_tag: String,
}

/// A request and its method, or a response and its status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Request(Method),
    Response(u16),
}

/// What one captured message carries that a predicate reads.
#[derive(Clone, Debug)]
pub struct Captured {
    pub kind: Kind,
    pub call_id: String,
    pub cseq: u32,
    pub cseq_method: Method,
    pub from_tag: Option<String>,
    pub to_tag: Option<String>,
    /// Its transaction; `None` without a top Via branch.
    pub txn: Option<TxnKey>,
    /// A response's RSeq (RFC 3262 §7.1).
    pub rseq: Option<u32>,
    /// A request's RAck: the RSeq and the CSeq number it acknowledges
    /// (RFC 3262 §7.2).
    pub rack: Option<(u32, u32)>,
}

impl Captured {
    /// A request's method; `None` for a response.
    pub fn method(&self) -> Option<&Method> {
        match &self.kind {
            Kind::Request(method) => Some(method),
            Kind::Response(_) => None,
        }
    }

    /// A response's status; `None` for a request.
    pub fn status(&self) -> Option<u16> {
        match self.kind {
            Kind::Request(_) => None,
            Kind::Response(status) => Some(status),
        }
    }

    /// The transaction of a request whose method is none of `excluded`.
    pub fn request_txn(&self, excluded: &[Method]) -> Option<&TxnKey> {
        self.method().filter(|m| !excluded.contains(m))?;
        self.txn.as_ref()
    }

    /// The dialog the message names.
    pub fn dialog(&self) -> DialogKey {
        DialogKey {
            call_id: self.call_id.clone(),
            from_tag: self.from_tag.clone().unwrap_or_default(),
            to_tag: self.to_tag.clone().unwrap_or_default(),
        }
    }

    fn of(m: &SipMessage) -> Captured {
        let kind = match msg_reads::request_method(m) {
            Some(method) => Kind::Request(method.clone()),
            None => Kind::Response(msg_reads::status(m)),
        };
        let method = match &kind {
            Kind::Request(method) => method.clone(),
            Kind::Response(_) => Method::from(msg_reads::cseq_method(m)),
        };
        let txn = msg_reads::top_via_branch(m).map(|branch| TxnKey {
            call_id: msg_reads::call_id(m).to_string(),
            cseq: msg_reads::cseq_seq(m),
            method,
            branch,
        });
        Captured {
            kind,
            call_id: msg_reads::call_id(m).to_string(),
            cseq: msg_reads::cseq_seq(m),
            cseq_method: Method::from(msg_reads::cseq_method(m)),
            from_tag: msg_reads::from_tag(m).map(str::to_string),
            to_tag: msg_reads::to_tag(m).map(str::to_string),
            txn,
            rseq: msg_reads::rseq(m),
            rack: msg_reads::rack(m),
        }
    }
}

/// The copies of one transaction's request and the finals it drew, as
/// indices into the capture.
#[derive(Clone, Debug, Default)]
pub struct Transaction {
    pub requests: Vec<usize>,
    pub finals: Vec<usize>,
}

/// A capture's wire entries, each parsed once, and its transactions.
pub struct CaptureIndex {
    entries: Vec<RecordedSipEntry>,
    messages: Vec<Option<Captured>>,
    transactions: HashMap<TxnKey, Transaction>,
}

impl CaptureIndex {
    /// Parse every entry of `entries` once; an entry no parser reads holds no
    /// message.
    pub fn new(entries: Vec<RecordedSipEntry>) -> CaptureIndex {
        let parser = super::lenient_parser();
        let messages: Vec<Option<Captured>> =
            entries.iter().map(|e| parser.parse(&e.raw).ok().map(|m| Captured::of(&m))).collect();
        let mut transactions: HashMap<TxnKey, Transaction> = HashMap::new();
        for (i, m) in messages.iter().enumerate() {
            let Some((m, key)) = m.as_ref().and_then(|m| Some((m, m.txn.as_ref()?))) else {
                continue;
            };
            match m.kind {
                Kind::Request(_) => transactions.entry(key.clone()).or_default().requests.push(i),
                Kind::Response(status) if status >= 200 => {
                    transactions.entry(key.clone()).or_default().finals.push(i)
                }
                Kind::Response(_) => {}
            }
        }
        CaptureIndex { entries, messages, transactions }
    }

    /// Every entry, in capture order.
    pub fn entries(&self) -> &[RecordedSipEntry] {
        &self.entries
    }

    /// The entry at `i`.
    pub fn entry(&self, i: usize) -> Option<&RecordedSipEntry> {
        self.entries.get(i)
    }

    /// The message the entry at `i` carries.
    pub fn message(&self, i: usize) -> Option<&Captured> {
        self.messages.get(i)?.as_ref()
    }

    /// The transaction `key` names.
    pub fn transaction(&self, key: &TxnKey) -> Option<&Transaction> {
        self.transactions.get(key)
    }

    /// Every entry holding a message, with its index, in capture order.
    pub fn iter(
        &self,
    ) -> impl DoubleEndedIterator<Item = (usize, &RecordedSipEntry, &Captured)> + '_ {
        self.within(0, self.entries.len())
    }

    /// [`iter`](Self::iter) over the entries captured before the one at `i`.
    pub fn before(
        &self,
        i: usize,
    ) -> impl DoubleEndedIterator<Item = (usize, &RecordedSipEntry, &Captured)> + '_ {
        self.within(0, i)
    }

    /// [`iter`](Self::iter) over the entries captured after the one at `i`.
    pub fn after(
        &self,
        i: usize,
    ) -> impl DoubleEndedIterator<Item = (usize, &RecordedSipEntry, &Captured)> + '_ {
        self.within(i.saturating_add(1), self.entries.len())
    }

    fn within(
        &self,
        start: usize,
        end: usize,
    ) -> impl DoubleEndedIterator<Item = (usize, &RecordedSipEntry, &Captured)> + '_ {
        let end = end.min(self.entries.len());
        let start = start.min(end);
        self.entries[start..end]
            .iter()
            .zip(&self.messages[start..end])
            .enumerate()
            .filter_map(move |(k, (e, m))| Some((start + k, e, m.as_ref()?)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(raw: &str) -> RecordedSipEntry {
        RecordedSipEntry {
            seq: 1,
            from: "127.0.0.1:5060".parse().unwrap(),
            to: "127.0.0.1:5080".parse().unwrap(),
            raw: raw.as_bytes().to_vec(),
            sent_ms: 1,
            received_ms: None,
            delivered: true,
            recv_note: None,
            reemit: None,
            from_lane: None,
            to_lane: None,
        }
    }

    fn message(start: &str, method: &str, extra: &str) -> String {
        format!(
            "{start}\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-1\r\n\
             From: <sip:a@x>;tag=a\r\nTo: <sip:b@x>;tag=b\r\nCall-ID: c1\r\n\
             CSeq: 1 {method}\r\n{extra}Content-Length: 0\r\n\r\n"
        )
    }

    #[test]
    fn a_transaction_holds_its_request_copies_and_its_finals() {
        let capture = CaptureIndex::new(vec![
            entry(&message("INVITE sip:b@x SIP/2.0", "INVITE", "")),
            entry(&message("SIP/2.0 180 Ringing", "INVITE", "RSeq: 4\r\n")),
            entry("not SIP"),
            entry(&message("INVITE sip:b@x SIP/2.0", "INVITE", "")),
            entry(&message("SIP/2.0 486 Busy Here", "INVITE", "")),
        ]);
        let key = capture.message(0).and_then(|m| m.txn.clone()).expect("a transaction");
        let txn = capture.transaction(&key).expect("indexed");
        assert_eq!(
            (txn.requests.as_slice(), txn.finals.as_slice()),
            ([0, 3].as_slice(), [4].as_slice())
        );
        assert_eq!(capture.message(1).and_then(|m| m.rseq), Some(4));
        assert!(capture.message(2).is_none(), "an unreadable entry holds no message");
        let before: Vec<usize> = capture.before(3).map(|(i, _, _)| i).collect();
        assert_eq!(before, [0, 1]);
        let after: Vec<usize> = capture.after(1).map(|(i, _, _)| i).collect();
        assert_eq!(after, [3, 4]);
    }

    #[test]
    fn a_prack_names_what_it_acknowledges_and_its_dialog() {
        let capture = CaptureIndex::new(vec![entry(&message(
            "PRACK sip:b@x SIP/2.0",
            "PRACK",
            "RAck: 4 1 INVITE\r\n",
        ))]);
        let prack = capture.message(0).expect("a PRACK");
        assert_eq!(prack.rack, Some((4, 1)));
        assert_eq!(prack.dialog().to_tag, "b");
        assert!(prack.request_txn(&[Method::Prack]).is_none());
        assert!(prack.request_txn(&[Method::Ack]).is_some());
    }
}
