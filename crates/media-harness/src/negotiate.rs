//! Run a full SDP offer/answer exchange across two media transports using the
//! production O/A engine, with a pluggable "B2BUA rewrite" applied to the offer
//! and answer as they cross the middle.
//!
//! Port of `../sipjsserver/tests/media/support-negotiate.ts`. The identity
//! rewrite models a transparent (signaling-only) B2BUA; a corrupting rewrite
//! models an SDP-rewrite bug that misdirects media — which a recorded-clip
//! verdict must then catch.

use media::sdp::negotiator::SdpNegotiationError;
use media::sdp::{NegotiatedMedia, OfferAnswerEngine, Sdp};
use media::{CommitReason, MediaSession, MediaTransport};

/// A transform applied to SDP as it crosses the (modelled) B2BUA.
pub type SdpRewrite = Box<dyn Fn(Sdp) -> Sdp>;

/// Corrupt the connection address — models a B2BUA that misdirects media.
pub fn corrupt_connection_addr(bogus_ip: impl Into<String>) -> SdpRewrite {
    let bogus = bogus_ip.into();
    Box::new(move |mut sdp: Sdp| {
        if sdp.connection_addr.is_some() {
            sdp.connection_addr = Some(bogus.clone());
        }
        for m in sdp.media.iter_mut() {
            if m.connection_addr.is_some() {
                m.connection_addr = Some(bogus.clone());
            }
        }
        sdp
    })
}

fn identity(sdp: Sdp) -> Sdp {
    sdp
}

pub struct NegotiatedCall {
    pub alice_session: MediaSession,
    pub bob_session: MediaSession,
    pub alice_negotiated: NegotiatedMedia,
    pub bob_negotiated: NegotiatedMedia,
}

/// Options for [`negotiate_call`].
#[derive(Default)]
pub struct NegotiateOptions {
    pub rewrite_offer: Option<SdpRewrite>,
    pub rewrite_answer: Option<SdpRewrite>,
    pub dialog_id: Option<String>,
}

/// Negotiate a confirmed call between two transports. `rewrite_offer` is applied
/// to the offer Alice sends before Bob sees it; `rewrite_answer` likewise to
/// Bob's answer before Alice sees it (both default to identity).
pub fn negotiate_call(
    alice: &MediaTransport,
    bob: &MediaTransport,
    opts: NegotiateOptions,
) -> Result<NegotiatedCall, SdpNegotiationError> {
    let rewrite_offer = opts.rewrite_offer.unwrap_or_else(|| Box::new(identity));
    let rewrite_answer = opts.rewrite_answer.unwrap_or_else(|| Box::new(identity));
    let dialog_id = opts.dialog_id.unwrap_or_else(|| "d".to_string());

    let mut alice_eng =
        OfferAnswerEngine::new(alice.local_addr().clone(), alice.supported_codecs().to_vec());
    let mut bob_eng =
        OfferAnswerEngine::new(bob.local_addr().clone(), bob.supported_codecs().to_vec());

    // Alice → INVITE (offer); Bob answers the observed (rewritten) offer.
    let offer = alice_eng.local_offer();
    let answer_sdp = bob_eng.answer_to(&rewrite_offer(offer))?;
    let bob_negotiated = bob_eng.negotiated().expect("bob negotiated").clone();
    let bob_session = bob.session(&dialog_id);
    bob_session
        .configure(bob_negotiated.clone())
        .expect("bob configure");

    // Alice applies the observed (rewritten) answer → points her at Bob.
    let alice_negotiated = alice_eng.apply_remote(&rewrite_answer(answer_sdp), true)?;
    let alice_session = alice.session(&dialog_id);
    alice_session
        .configure(alice_negotiated.clone())
        .expect("alice configure");

    // 200 OK both ways → both sessions commit (become active peers).
    alice_session.commit(CommitReason::Confirmed);
    bob_session.commit(CommitReason::Confirmed);

    Ok(NegotiatedCall {
        alice_session,
        bob_session,
        alice_negotiated,
        bob_negotiated,
    })
}
