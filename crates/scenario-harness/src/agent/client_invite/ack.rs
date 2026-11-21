//! The ACK to a [`ClientInvite`]'s 2xx (RFC 3261 §13.2.2.4), its panicking
//! forms; the fallible ones are in the sibling `fallible` module.

use super::super::dialog::Dialog;
use super::super::step::unwrap_step;
use super::ClientInvite;

impl ClientInvite {
    /// Generate and send the ACK for the 2xx (CSeq reused from the INVITE per
    /// RFC 3261 §13.2.2.4), then return the confirmed [`Dialog`]. With a route
    /// set the ACK carries Route headers and goes to the first hop (the proxy).
    /// The ACK-to-2xx is a dedicated primitive: it takes no template in v1 — a
    /// captured ACK's frozen-header quirks are not replayable yet.
    pub async fn ack(&mut self) -> Dialog {
        self.ack_with(None).await
    }

    /// The EXPLICIT delayed-ACK form: asserts a `delayed-automatic` declaration
    /// exists (declare it via [`Invite::delayed_ack`](super::super::Invite::delayed_ack))
    /// and then ACKs — identical to [`ack`](Self::ack), which also honours the
    /// declaration. Reach for it when a test states its intent to hold the ACK.
    pub async fn ack_delayed(&mut self) -> Dialog {
        assert!(
            self.delayed_automatic.is_some(),
            "ack_delayed requires a declared delayed-automatic (Invite::delayed_ack) — use ack()",
        );
        self.ack_with(None).await
    }

    /// ACK the 2xx carrying an optional SDP body — the delayed-offer answer
    /// rides the ACK when the 200 OK carried the offer (RFC 3264 §4). Honours a
    /// declared `delayed-automatic` (holds the ACK first), so a plain `ack()`
    /// after a declaration still delays.
    pub async fn ack_with(&mut self, sdp: Option<&str>) -> Dialog {
        self.ack_with_stating(sdp, &[]).await
    }

    /// [`ack`](Self::ack) with the acknowledging party's own header lines on
    /// the ACK (RFC 3261 §13.2.2.4 makes it a request of its own, so it may
    /// state end-to-end headers like any other).
    pub async fn ack_stating(&mut self, stated: &[(&str, &str)]) -> Dialog {
        self.ack_with_stating(None, stated).await
    }

    /// [`ack_with`](Self::ack_with) and [`ack_stating`](Self::ack_stating) at
    /// once: the delayed-offer answer and the party's own lines on one ACK.
    pub async fn ack_with_stating(&mut self, sdp: Option<&str>, stated: &[(&str, &str)]) -> Dialog {
        unwrap_step(self.try_ack_with_stating(sdp, stated).await)
    }

    /// [`ack_with_stating`](Self::ack_with_stating) with the delayed-offer
    /// answer as `body` typed `content_type`: a session description framed
    /// beside other parts (RFC 5621 §3), or any body the ACK may carry
    /// (RFC 3261 §13.2.2.4).
    pub async fn ack_with_body_stating(
        &mut self,
        content_type: &str,
        body: &[u8],
        stated: &[(&str, &str)],
    ) -> Dialog {
        unwrap_step(self.try_ack_with_body_stating(content_type, body, stated).await)
    }
}
