//! Relay primitives. Unlike a transparent proxy, the B2BUA *regenerates*
//! messages on the peer leg's own transaction/dialog (back-to-back UAs): a
//! response from bob is rebuilt as a fresh response on alice's INVITE server
//! transaction (stable a-facing To-tag), and an in-dialog request is rebuilt on
//! the peer dialog. This keeps the two dialogs independent (their tags, CSeq
//! spaces and Contacts are the B2BUA's own), which is the whole point of a
//! B2BUA. One module per concern:
//!
//! - [`originate`] — [`build_b_leg`], the single mint point for every leg the
//!   B2BUA originates (the callee leg and the REFER transfer leg)
//! - [`a_leg_response`] — the UAS response minted on the a-leg's INVITE
//! - [`ack`] — the ACK-for-2xx on a b-leg dialog
//! - [`egress`] — outbound routing policy (loose routes, front-proxy bootstrap)
//! - [`passthrough`] — the §16.6 transparency sets + RSeq ownership
//! - [`advert`] — the capability advertisement (`Allow`/`Supported`) stamp
//! - [`address`] — decision-supplied address reading and its refusal
//! - [`identity`] — the B2BUA's own per-leg Via/Contact
//! - [`dialog`] — reading the `call` crate's text-typed dialog state back
//! - [`body`] — the media type describing a body this stack emits
//! - [`failure_ext`] — the relayed-failure-headers `Call.ext` slot
//!
//! Header/message *extraction* does NOT live here — see `sip-message`.

mod a_leg_response;
mod ack;
mod address;
mod advert;
mod body;
mod dialog;
mod egress;
mod failure_ext;
mod identity;
mod originate;
mod passthrough;
mod repeat;

#[cfg(test)]
mod originate_tests;

// Originating a leg + answering/acknowledging on an existing one.
pub use a_leg_response::{provisional_after_final, response_to_a_leg};
pub use ack::ack_b_leg;
pub(crate) use ack::{acked_invite_carries_offer, acked_invite_cseq};
pub(crate) use originate::clamp_no_answer;
pub use originate::{build_b_leg, rebuild_a_leg_invite};
pub(crate) use repeat::{repeated_reliable_provisional, retransmitted_2xx};

// Wire routing for what those emit.
pub use egress::{apply_b_leg_egress, leg_egress_dest, outbound_proxy_route_set};

// Transparency + advertisement across the back-to-back UA.
pub use advert::stamp_a_facing_invite_advert;
pub use b2bua_sdk::provisional::reliable_rseq;
pub use passthrough::{
    own_the_rseq, relay_request_passthrough_headers, relay_response_passthrough_headers,
    strip_reliability,
};

// Reading text back into typed values (decision fields, dialog state, bodies).
pub use address::{redirect_contact, UnreadableAddress};
pub use body::{carries_sdp, media_type, sdp};
pub use dialog::{target_dest, to_gen_dialog};

// The relayed-failure-headers Call.ext slot.
pub use failure_ext::{
    failure_headers_ext, is_core_reserved_ext, relayed_failure_headers, RELAYED_FAILURE_HEADERS_EXT,
};

// The B2BUA's own per-leg identity.
pub use identity::{leg_contact, leg_via};
