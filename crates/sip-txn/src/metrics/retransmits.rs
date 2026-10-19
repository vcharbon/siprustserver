//! The retransmit family: one row per repeat the layer puts on the wire, a
//! fixed set of atomics keyed by ladder, method and status.

use std::sync::atomic::{AtomicU64, Ordering};

use metric_catalogue::OpenRows;
use sip_message::Method;
use sip_retransmit::Class;

/// The transaction ladders this layer drives, in the order the family
/// enumerates them. A dialog-level class is the TU's and never reaches here.
const REQUEST_LADDERS: [Class; 4] =
    [Class::InviteClient, Class::NonInviteClient, Class::NonInviteProceeding, Class::CancelClient];

/// The `method` label slots of the fixed rows: every method `sip_message`
/// models natively. An extension method's rows are held apart, by token.
const METHOD_LABELS: [&str; 14] = Method::NATIVE_TOKENS;

/// The `method` label of a rung, resolved before its send: the fixed slot of
/// a native method (no allocation), or an extension method's token.
#[derive(Debug, Clone)]
pub(crate) enum MethodSlot {
    Native(usize),
    Extension(Box<str>),
}

/// The `method` label of `method`.
pub(crate) fn method_slot(method: &Method) -> MethodSlot {
    let slot = match method {
        Method::Invite => 0,
        Method::Ack => 1,
        Method::Bye => 2,
        Method::Cancel => 3,
        Method::Options => 4,
        Method::Register => 5,
        Method::Info => 6,
        Method::Update => 7,
        Method::Prack => 8,
        Method::Subscribe => 9,
        Method::Notify => 10,
        Method::Publish => 11,
        Method::Message => 12,
        Method::Refer => 13,
        Method::Other(token) => return MethodSlot::Extension(token.as_str().into()),
    };
    MethodSlot::Native(slot)
}

/// The lowest status Timer G ever paces: a non-2xx INVITE final (§17.2.1).
const FIRST_FINAL_CODE: u16 = 300;
/// One slot per status 300..=699.
const FINAL_CODES: usize = 400;
/// The lowest status a server transaction caches for replay (its auto-100).
const FIRST_TRIGGER_CODE: u16 = 100;
/// One slot per status 100..=699.
const TRIGGER_CODES: usize = 600;

/// The `ladder` label of a repeat no timer paced.
const TRIGGER: &str = "trigger";

/// `{ladder,method,code}` — one row per repeat this layer put on the wire: a
/// transaction-ladder rung — Timer A / Timer E (`method` is the request's, no
/// `code`), the CANCEL sub-ladder, Timer G (`INVITE` and the final's status)
/// — and the `trigger` replay of a server transaction's cached response to a
/// retransmitted request (§17.2.1; the request's method and the response's
/// status). A fixed set of atomics keyed by class and label slot, on the
/// heap: the owner task bumps one with no lock, and the scrape reads them
/// off-thread.
#[derive(Debug)]
pub(crate) struct RetransmitFamily {
    /// `ladder * METHODS + method` for the request ladders.
    requests: Box<[AtomicU64]>,
    /// `status - 300` for `InviteServerFinal`.
    finals: Box<[AtomicU64]>,
    /// `method * TRIGGER_CODES + (status - 100)` for the cached-response
    /// replay.
    triggers: Box<[AtomicU64]>,
    /// The rows of extension methods, `[ladder, token]` or `[ladder, token,
    /// code]`, under the family's cap ([`crate::catalogue::RETRANSMITS`]);
    /// past it a rung lands on its ladder's and code's row whose method is
    /// [`OVERFLOW`](metric_catalogue::OVERFLOW).
    extensions: OpenRows,
}

/// One row of the retransmit family with a non-zero count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetransmitRow {
    pub ladder: &'static str,
    pub method: String,
    /// The final's status, on the Timer G row; `None` on a request's.
    pub code: Option<u16>,
    pub count: u64,
}

/// The `method` label slots of a request ladder's or a replay's rows.
const METHODS: usize = METHOD_LABELS.len();

/// `n` counters at zero.
fn counters(n: usize) -> Box<[AtomicU64]> {
    (0..n).map(|_| AtomicU64::new(0)).collect()
}

/// Add one to the counter at `slot` of `cells`.
fn bump(cells: &[AtomicU64], slot: usize) {
    if let Some(cell) = cells.get(slot) {
        cell.fetch_add(1, Ordering::Relaxed);
    }
}

fn sum(cells: &[AtomicU64]) -> u64 {
    cells.iter().map(|a| a.load(Ordering::Relaxed)).sum()
}

impl RetransmitFamily {
    pub(super) fn new() -> Self {
        Self {
            requests: counters(REQUEST_LADDERS.len() * METHODS),
            finals: counters(FINAL_CODES),
            triggers: counters(METHODS * TRIGGER_CODES),
            extensions: OpenRows::new(&crate::catalogue::RETRANSMITS),
        }
    }

    /// Rungs of extension methods that landed on the overflow row.
    pub(super) fn overflowed(&self) -> u64 {
        self.extensions.overflowed()
    }

    /// The extension-method rows' total over the rows of `ladder`.
    fn extension_total(&self, ladder: &str) -> u64 {
        self.extensions
            .rows()
            .iter()
            .filter(|(labels, _)| labels.first().is_some_and(|l| l == ladder))
            .map(|(_, n)| n)
            .sum()
    }

    /// Count one rung of `ladder` re-sending a request whose method is
    /// `slot` ([`method_slot`]).
    pub(crate) fn record_request(&self, ladder: Class, slot: MethodSlot) {
        let Some(row) = REQUEST_LADDERS.iter().position(|c| *c == ladder) else { return };
        match slot {
            MethodSlot::Native(slot) => bump(&self.requests, row * METHODS + slot),
            MethodSlot::Extension(token) => self.extensions.add(&[ladder.as_str(), &token], 1),
        }
    }

    /// Count one Timer G rung re-sending an INVITE's non-2xx final of `code`.
    pub(crate) fn record_final(&self, code: u16) {
        if let Some(slot) = code.checked_sub(FIRST_FINAL_CODE) {
            bump(&self.finals, usize::from(slot));
        }
    }

    /// Count one replay of the cached response of status `code` to a
    /// retransmitted request of `method`.
    pub(crate) fn record_trigger(&self, method: &Method, code: u16) {
        let Some(slot) =
            code.checked_sub(FIRST_TRIGGER_CODE).map(usize::from).filter(|s| *s < TRIGGER_CODES)
        else {
            return;
        };
        match method_slot(method) {
            MethodSlot::Native(row) => bump(&self.triggers, row * TRIGGER_CODES + slot),
            MethodSlot::Extension(token) => {
                self.extensions.add(&[TRIGGER, &token, &code.to_string()], 1)
            }
        }
    }

    /// Every cached-response replay, whatever it repeated.
    pub(super) fn triggered(&self) -> u64 {
        sum(&self.triggers) + self.extension_total(TRIGGER)
    }

    pub(super) fn total(&self, ladder: Class) -> u64 {
        match REQUEST_LADDERS.iter().position(|c| *c == ladder) {
            Some(row) => {
                self.requests.chunks(METHODS).nth(row).map_or(0, sum)
                    + self.extension_total(ladder.as_str())
            }
            None if ladder == Class::InviteServerFinal => sum(&self.finals),
            None => 0,
        }
    }

    pub(super) fn rows(&self) -> Vec<RetransmitRow> {
        let mut out = Vec::new();
        for (ladder, cells) in REQUEST_LADDERS.iter().zip(self.requests.chunks(METHODS)) {
            for (method, cell) in METHOD_LABELS.iter().zip(cells) {
                let count = cell.load(Ordering::Relaxed);
                if count > 0 {
                    out.push(RetransmitRow {
                        ladder: ladder.as_str(),
                        method: method.to_string(),
                        code: None,
                        count,
                    });
                }
            }
        }
        for (code, cell) in (FIRST_FINAL_CODE..).zip(self.finals.iter()) {
            let count = cell.load(Ordering::Relaxed);
            if count > 0 {
                out.push(RetransmitRow {
                    ladder: Class::InviteServerFinal.as_str(),
                    method: "INVITE".to_string(),
                    code: Some(code),
                    count,
                });
            }
        }
        for (method, cells) in METHOD_LABELS.iter().zip(self.triggers.chunks(TRIGGER_CODES)) {
            for (code, cell) in (FIRST_TRIGGER_CODE..).zip(cells) {
                let count = cell.load(Ordering::Relaxed);
                if count > 0 {
                    out.push(RetransmitRow {
                        ladder: TRIGGER,
                        method: method.to_string(),
                        code: Some(code),
                        count,
                    });
                }
            }
        }
        let ladders = REQUEST_LADDERS.map(Class::as_str);
        for (labels, count) in self.extensions.rows() {
            let (Some(first), Some(method)) = (labels.first(), labels.get(1)) else { continue };
            let Some(ladder) = ladders.into_iter().chain([TRIGGER]).find(|l| l == first) else {
                continue;
            };
            out.push(RetransmitRow {
                ladder,
                method: method.clone(),
                code: labels.get(2).and_then(|code| code.parse().ok()),
                count,
            });
        }
        out
    }
}
