//! What a BYE or CANCEL this stack mints carries of the peer that asked for
//! the teardown: its relayable headers, a BYE's body, its hop count.

use call::QueuedRequest;
use sip_message::generators::{self, RelayScope, RelaySituation, RelayedMessage};
use sip_message::header::{q850_cause_alone, readable_reasons, HeaderName, MediaType};
use sip_message::{hops, Method, SipHeader};

use crate::rules::relay;
use b2bua_sdk::model::RuleContext;
use b2bua_sdk::release_reason::CancelReason;

/// What a minted teardown request carries of the peer's: its end-to-end
/// headers and, on a BYE minted from the peer's BYE, its body.
pub(super) struct Relayed {
    pub(super) headers: Vec<SipHeader>,
    pub(super) body: Vec<u8>,
    pub(super) content_type: Option<MediaType>,
}

impl Relayed {
    /// Nothing of a peer's: a request this stack mints on its own account.
    pub(super) fn none() -> Self {
        Self::headers(Vec::new())
    }

    /// Headers alone, no body.
    pub(super) fn headers(headers: Vec<SipHeader>) -> Self {
        Self { headers, body: Vec::new(), content_type: None }
    }

    /// The BYE carrying these and `reason`, `hops`, queued as what it says.
    pub(super) fn queued_bye(self, reason: Option<&str>, hops: Option<u32>) -> QueuedRequest {
        QueuedRequest::Bye {
            reason: reason.map(str::to_string),
            headers: self
                .headers
                .iter()
                .map(|h| (h.name.to_string(), h.value.to_string()))
                .collect(),
            body: self.body,
            content_type: self.content_type.map(|t| t.to_string()),
            max_forwards: hops,
        }
    }
}

/// What the peer said about the teardown it asked for, for the BYE or CANCEL
/// this stack mints toward the other leg (RFC 3261 §16.6). RFC 3326 §2 scopes
/// `Reason` to exactly these two methods, and the transaction layer already
/// answered the CANCEL, so its lines ride the event rather than a request.
/// Empty for a timer or a failure: those state a release of OURS, and nothing
/// is put in the peer's mouth. A minted request of another method than the
/// peer's (a CANCEL for an early BYE, §9.1, or a BYE for a CANCEL) relays
/// nothing of it but `Reason`. A `Timestamp` the peer stated is restated with
/// this turn's clock (RFC 3261 §20.38). A minted CANCEL restates that `Reason` as the
/// deployment's `relayed_cancel_reason` says. A BYE minted from the peer's BYE
/// carries its body verbatim with every header describing it (§20.11); any
/// other minted request carries no body, so the source's body metadata is
/// withheld. `situation` names the minted request for the relay policy.
pub(super) fn relayed_teardown(ctx: &RuleContext, situation: RelaySituation<'_>) -> Relayed {
    let bye = ctx.request().filter(|request| request.method() == Method::Bye);
    let (received, method) = match bye {
        Some(request) => (request.headers(), Method::Bye),
        None => (ctx.cancelled_headers(), Method::Cancel),
    };
    let same_method = situation.message == RelayedMessage::Request(&method);
    let carried = bye.filter(|request| same_method && !request.body().is_empty());
    let source_body = match carried {
        Some(_) => RelayScope::request(),
        None => RelayScope::request().without_source_body(),
    };
    let stamp = generators::timestamp_value(ctx.now_ms);
    let scope = ctx.config.relay_scope(source_body, situation).stamped(Some(&stamp));
    let mut headers = generators::relayable_headers(received, scope);
    if !same_method {
        headers.retain(|h| HeaderName::Reason.matches(&h.name));
    }
    if situation.message == RelayedMessage::Request(&Method::Cancel) {
        restate_cancel_reason(&mut headers, ctx.config.relayed_cancel_reason);
    }
    match carried {
        Some(request) => Relayed {
            headers,
            body: request.body().to_vec(),
            content_type: request.raw(HeaderName::ContentType).next().and_then(relay::media_type),
        },
        None => Relayed::headers(headers),
    }
}

/// `relayed`'s `Reason` lines as `how` restates them on a minted CANCEL:
/// unchanged, or a leading Q.850 value's cause alone (none where the first
/// value is not a Q.850 one stating a cause).
fn restate_cancel_reason(relayed: &mut Vec<SipHeader>, how: CancelReason) {
    if how == CancelReason::Verbatim {
        return;
    }
    let reasons = readable_reasons(
        relayed.iter().filter(|h| HeaderName::Reason.matches(&h.name)).map(|h| h.value.clone()),
    );
    relayed.retain(|h| !HeaderName::Reason.matches(&h.name));
    if let Some(value) = q850_cause_alone(&reasons) {
        relayed.push(SipHeader { name: "Reason".to_string().into(), value: value.into() });
    }
}

/// The hop count that teardown states (RFC 3261 §16.6 step 3): the releasing
/// peer's count less one where the peer ASKED for the teardown, and `None` —
/// the §8.1.1.6 default — for a release of OURS (a timer, a failure, a CANCEL
/// the transaction layer already answered), which starts a hop budget rather
/// than continuing one.
pub(super) fn relayed_teardown_hops(ctx: &RuleContext) -> Option<u32> {
    match ctx.request() {
        Some(request) if request.method() == Method::Bye => {
            Some(hops::forwarded_max_forwards(request).value())
        }
        _ => None,
    }
}
