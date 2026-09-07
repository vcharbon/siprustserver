//! The one retained emission a retransmission repeats (ADR-0029 X3): the
//! datagram exactly as it left the socket, where it went, and how it is
//! repeated. Every dialog-level repeat — the un-ACKed 2xx (RFC 3261
//! §13.3.1.4), the re-ACK of a repeated 2xx (§13.2.2.4), the un-PRACKed
//! reliable provisional (RFC 3262 §3) — is one of these.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use sip_retransmit::{Class, Ladder, Schedule};

/// How a retained emission is repeated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Repeat {
    /// On the ladder `class` paces. `rung` is the rung currently armed and is
    /// the whole of the ladder's state (ADR-0029 X2): no epoch anchor rides in
    /// the replicated body, so a takeover resumes the ladder where it stood.
    Paced { class: Class, rung: u32 },
    /// Only when the peer provokes it — the §13.2.2.4 re-ACK of a repeated
    /// 2xx. No rung, no timer of its own.
    OnTrigger,
}

impl Repeat {
    /// What paced the repeat, as a counter labels it (`ladder="…"`): the
    /// class's own name, or `trigger` for a repeat no timer paced.
    pub fn ladder(self) -> &'static str {
        match self {
            Repeat::Paced { class, .. } => class.as_str(),
            Repeat::OnTrigger => "trigger",
        }
    }
}

/// What a retained emission is a copy of, as the two facts a counter labels a
/// repeat by: the CSeq method, and the status for a response. Captured at
/// retention, while the message is still parsed — the datagram is opaque
/// from then on, and no repeat re-reads it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repeated {
    method: String,
    code: Option<u16>,
}

impl Repeated {
    /// A request of `method`.
    pub fn request(method: &str) -> Self {
        Repeated { method: method.to_string(), code: None }
    }

    /// A response of status `code` to a request of `method`.
    pub fn response(method: &str, code: u16) -> Self {
        Repeated { method: method.to_string(), code: Some(code) }
    }

    /// The CSeq method of the repeated message.
    pub fn method(&self) -> &str {
        &self.method
    }

    /// The status, for a response; `None` for a request.
    pub fn code(&self) -> Option<u16> {
        self.code
    }
}

/// A message retained as the exact datagram it left as — a typed message's
/// `image()`, which the transaction layer sends verbatim — so a repeat of it
/// IS the message the RFC names rather than a re-composition of one. Opaque:
/// the one outward operation yields the bytes and their destination, so no
/// caller can parse, edit and re-emit them as a "repeat". Replicated with the
/// call, so a takeover node keeps the obligation byte for byte.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetainedEmission {
    #[serde(with = "serde_bytes")]
    datagram: Vec<u8>,
    dest_host: String,
    dest_port: u16,
    repeat: Repeat,
    repeated: Repeated,
}

impl RetainedEmission {
    /// Retain `datagram`, a copy of `repeated`, on the ladder `class` paces,
    /// its first rung armed. Returns the emission and the wait before that
    /// rung; the owner arms its timer from it.
    pub fn paced(
        datagram: Vec<u8>,
        dest: (String, u16),
        class: Class,
        repeated: Repeated,
    ) -> (Self, Duration) {
        let (ladder, first) = Ladder::armed(Schedule::rfc(class))
            .expect("an RFC class owes its first re-send unconditionally");
        let emission = RetainedEmission {
            datagram,
            dest_host: dest.0,
            dest_port: dest.1,
            repeat: Repeat::Paced { class, rung: ladder.rung() },
            repeated,
        };
        (emission, first)
    }

    /// Retain `datagram`, a copy of `repeated`, to be re-sent only when the
    /// peer provokes it.
    pub fn on_trigger(datagram: Vec<u8>, dest: (String, u16), repeated: Repeated) -> Self {
        RetainedEmission {
            datagram,
            dest_host: dest.0,
            dest_port: dest.1,
            repeat: Repeat::OnTrigger,
            repeated,
        }
    }

    /// The bytes and the `(host, port)` they go to — the only way out.
    pub fn wire(&self) -> (&[u8], (&str, u16)) {
        (&self.datagram, (self.dest_host.as_str(), self.dest_port))
    }

    /// How this emission is repeated.
    pub fn repeat(&self) -> Repeat {
        self.repeat
    }

    /// What this emission is a copy of — the method and status a repeat of it
    /// is counted under.
    pub fn repeated(&self) -> &Repeated {
        &self.repeated
    }

    /// Step a paced emission onto its next rung and return the wait before it,
    /// or `None` when that rung would land at or past the bound: the ladder is
    /// over and the emission spent. `give_up` is the owner's own deadline; it
    /// tightens the class's bound and never extends it — the RFC ceases at its
    /// own bound (Timer L for a 2xx) whatever local policy says. An `OnTrigger`
    /// emission has no rung and never advances.
    pub fn advance(&mut self, give_up: Option<Duration>) -> Option<Duration> {
        let Repeat::Paced { class, rung } = self.repeat else {
            return None;
        };
        let schedule = match give_up {
            Some(bound) => Schedule::rfc(class).tightened_to(bound),
            None => Schedule::rfc(class),
        };
        let mut ladder = Ladder::at_rung(schedule, rung);
        let next = ladder.advance()?;
        self.repeat = Repeat::Paced { class, rung: ladder.rung() };
        Some(next)
    }
}
