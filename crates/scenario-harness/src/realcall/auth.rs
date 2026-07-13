//! Authentication challenge seam — the **deferred-by-design** hook for digest
//! auth (RFC 3261 §22 / RFC 8760). Auth is NOT implemented here: this module
//! defines the ONE adapter point so that adding real digest later touches an
//! implementation of [`ChallengeResponder`], not the call choreography.
//!
//! # Why a seam and not the thing
//!
//! Today's SUT under load does not challenge (`401`/`407` classify as
//! `status_401`/`status_407` and the call is counted as a deviation). But a real
//! registrar / edge proxy WILL challenge, and the day we point the load fleet at
//! one we want digest to be a single pluggable object, wired through
//! [`CallEnv`](super::CallEnv) exactly like the correlation strategy — never a
//! branch sprinkled through the actor caller's INVITE goal / the request builders.
//!
//! # The retry contract (RFC 3261 §22.2 / §8.1.3.5)
//!
//! When a request draws a `401 Unauthorized` (UAS challenge, `WWW-Authenticate`)
//! or `407 Proxy Authentication Required` (proxy challenge, `Proxy-Authenticate`)
//! **and** a responder is configured, the choreography:
//!   1. ACKs the failed INVITE transaction (RFC 3261 §17.1.1.3) — the challenge
//!      is a final response, so its transaction must be completed on the wire —
//!      or simply lets the completed non-INVITE transaction go;
//!   2. asks the [`ChallengeResponder`] for the credential header value;
//!   3. re-sends the SAME request with the credential header added and its CSeq
//!      **bumped by one** (a new client transaction, RFC 3261 §22.2), exactly
//!      **once**.
//!
//! Without a responder (the default) nothing changes: the `4xx` classifies as
//! `status_401` / `status_407` byte-for-byte as before. The retry is a no-op
//! unless someone plugs an implementation in — so the load lane's shipped
//! behaviour is untouched until the first real challenging SUT.
//!
//! # Adding real digest later (the sketch)
//!
//! Implement [`ChallengeResponder`] over an RFC 2617 / RFC 7616 digest computer
//! (the `challenge` param carries the parsed `nonce`/`realm`/`qop`/`algorithm`;
//! `method` + `ruri` are the two request-line inputs the `A2` hash needs), return
//! `Some("Digest username=…, realm=…, nonce=…, uri=…, response=…")`, and attach
//! it to the [`CallEnv`](super::CallEnv) (or the load `MixEntry`) as
//! `Some(Arc::new(MyDigest{…}))`. No choreography / builder change is needed —
//! the retry primitive (`ClientInvite::ack_and_resend_with_auth`) is already
//! wired; only the actor caller's INVITE path has to call it.

use sip_message::message_helpers::get_header;
use sip_message::SipResponse;

/// A parsed authentication challenge — the input a [`ChallengeResponder`] needs
/// to mint a credential. Deliberately transport-agnostic and free of any digest
/// math: it carries the challenge status and the raw challenge header so a
/// future digest implementation parses `realm`/`nonce`/`qop`/`algorithm` off
/// [`Challenge::header_value`] itself (RFC 2617 §3.2.1 grammar), and knows which
/// credential header to emit back via [`Challenge::credential_header`].
#[derive(Debug, Clone)]
pub struct Challenge {
    /// The challenge status: `401` (UAS challenge) or `407` (proxy challenge).
    pub status: u16,
    /// The raw challenge header value — `WWW-Authenticate` for a `401`,
    /// `Proxy-Authenticate` for a `407` (RFC 3261 §20.44 / §20.27). Empty if the
    /// challenge response carried none (a malformed challenge — a responder may
    /// still choose to answer, e.g. a static-credential fixture).
    pub header_value: String,
}

impl Challenge {
    /// The credential header a response to THIS challenge must carry:
    /// `Authorization` answers a `401`, `Proxy-Authorization` answers a `407`
    /// (RFC 3261 §20.7 / §20.28). The retry point stamps the responder's returned
    /// value under this name.
    pub fn credential_header(&self) -> &'static str {
        if self.status == 407 {
            "Proxy-Authorization"
        } else {
            "Authorization"
        }
    }
}

/// Parse a `401`/`407` response into a [`Challenge`], or `None` if the response
/// is neither (so a non-challenge `4xx` is left to classify exactly as today).
/// A `401` reads `WWW-Authenticate`, a `407` reads `Proxy-Authenticate`
/// (RFC 3261 §22.1); a missing header yields an empty [`Challenge::header_value`]
/// rather than dropping the challenge, so a fixture responder that ignores the
/// header (a static credential) still fires.
pub fn parse_challenge(resp: &SipResponse) -> Option<Challenge> {
    let hdr = match resp.status {
        401 => "www-authenticate",
        407 => "proxy-authenticate",
        _ => return None,
    };
    Some(Challenge {
        status: resp.status,
        header_value: get_header(&resp.headers, hdr).unwrap_or("").to_string(),
    })
}

/// The pluggable authentication adapter — **deferred by design** (see the module
/// docs). Given a parsed [`Challenge`] and the request-line inputs a credential
/// is computed over (the request `method` and its Request-URI `ruri`), return the
/// value for the credential header the retry should carry
/// ([`Challenge::credential_header`]), or `None` to decline (the challenge then
/// classifies as `status_401` / `status_407` as if no responder existed).
///
/// Implementations are `Send + Sync` and shared across the load fleet (held as an
/// `Arc<dyn ChallengeResponder>` on the [`CallEnv`](super::CallEnv)), so they must
/// be stateless or internally synchronised. The default is **no responder**: the
/// choreography never retries, preserving today's classification exactly.
pub trait ChallengeResponder: Send + Sync {
    /// Produce the credential header value answering `challenge` for a request
    /// with the given `method` (e.g. `"INVITE"`) and `ruri` (the wire
    /// Request-URI). `None` declines the challenge.
    fn respond(&self, challenge: &Challenge, method: &str, ruri: &str) -> Option<String>;
}
