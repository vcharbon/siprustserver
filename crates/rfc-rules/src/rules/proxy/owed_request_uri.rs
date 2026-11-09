//! What a strict-routed request owes the proxy that takes it (RFC 3261 §16.4,
//! §16.6 step 6).

use sip_message::sniff;

/// The Request-URI a proxy at `proxy` owes the forward of a request carrying
/// `head` and `routes`, or `None` where the request is not strict-routed.
///
/// - Addressed to the proxy (§16.4): its Request-URI names the proxy — a URI
///   the proxy record-routed, `;lr` or not, since a strict-routing peer
///   addresses whatever URI it was given — no loose top Route names the
///   proxy, and the last Route is not loose: a strict-routed request carries
///   the remote target last (§12.2.1.1). That target is owed; a strict Route
///   left first takes its place (§16.6 step 6).
/// - Otherwise a loose top Route naming the proxy, by its authority or its
///   `maddr`, is popped (§16.4), and the first Route left is owed the Request-URI
///   when it is strict (§16.6 step 6).
pub(super) fn owed_request_uri(
    head: &[u8],
    routes: &[sniff::UriFacts],
    proxy: &str,
) -> Option<String> {
    let at_proxy = |host: &str, port: u16| {
        super::super::dialog::wire_addr(proxy).is_some_and(|(h, p)| h == host && p == port)
    };
    let loose_to_proxy = routes.first().is_some_and(|r| {
        r.loose && (at_proxy(&r.host, r.port) || at_proxy(&r.next_hop_host, r.port))
    });
    let target_last = routes.last().is_some_and(|r| !r.loose);
    let addressed = !loose_to_proxy
        && target_last
        && sniff::request_uri_facts(head).is_some_and(|uri| at_proxy(&uri.host, uri.port));
    if addressed {
        let (last, rest) = routes.split_last()?;
        return Some(match rest.first() {
            Some(next) if !next.loose => next.uri.clone(),
            _ => last.uri.clone(),
        });
    }
    let left = if loose_to_proxy { &routes[1..] } else { routes };
    left.first().filter(|r| !r.loose).map(|r| r.uri.clone())
}
