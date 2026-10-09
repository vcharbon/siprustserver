//! A replayed media resource's MSCML id echo (RFC 5022 §4.1, §10.2).
//!
//! The client stamps a request's command element with an `id` of its own
//! choosing, live, so a recorded `<response>` can never carry it. Each leg
//! remembers, per command type, the id the last request of that type it
//! RECEIVED carried — or that it carried none — and a response the leg SENDS
//! takes the id of the last request of the type its `request` attribute names.
//! A response of a type the leg never received rides as recorded. The MSCML
//! reading and writing is `sip_message::mscml`'s; this module binds it to run
//! state.

use std::collections::BTreeMap;

use crate::gate::Inbound;

/// What one leg has received: command type → the client-defined id the last
/// request of that type carried, `None` where it carried none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MscmlRequests(BTreeMap<String, Option<String>>);

impl MscmlRequests {
    /// Remember every MSCML request `inbound` carries. A response, or a request
    /// with no MSCML body, leaves the record as it was.
    pub fn remember(&mut self, inbound: &Inbound) {
        if inbound.method.is_none() {
            return;
        }
        for request in
            sip_message::mscml::requests_in(inbound.content_type.as_deref(), &inbound.body)
        {
            self.0.insert(request.command, request.id);
        }
    }

    /// The id the last request of `command` carried: `Some(None)` where it
    /// carried none, `None` where no request of that type was received.
    pub fn id_of(&self, command: &str) -> Option<Option<&str>> {
        self.0.get(command).map(Option::as_deref)
    }
}

/// `payload` as a send emits it under `content_type`: an MSCML `<response>`
/// takes the id `requests` remembered for its type; every other payload, and a
/// response of a type never received, rides byte for byte.
pub fn echo(payload: Vec<u8>, content_type: &str, requests: &MscmlRequests) -> Vec<u8> {
    if !sip_message::mscml::is_mscml(content_type) {
        return payload;
    }
    let Ok(text) = std::str::from_utf8(&payload) else { return payload };
    let Some(id) = sip_message::mscml::response_request(text).and_then(|t| requests.id_of(t))
    else {
        return payload;
    };
    sip_message::mscml::with_response_id(text, id).into_bytes()
}
