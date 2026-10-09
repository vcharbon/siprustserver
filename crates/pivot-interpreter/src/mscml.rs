//! A replayed media resource's MSCML id echo (RFC 5022 §4.1, §10.2).
//!
//! The client stamps a request's command element with an `id` of its own
//! choosing, live, so a recorded `<response>` can never carry it. A media
//! server does not queue requests, and every request it received draws its own
//! response, the stopped one included (§6). So each leg keeps, per command
//! type, the requests it RECEIVED that no response has answered yet, oldest
//! first, each with its id or none; a `<response request="T">` the leg SENDS
//! takes the id of the oldest unanswered T request and answers it. A response
//! of a type with no unanswered request rides as recorded. The MSCML reading
//! and writing is `sip_message::mscml`'s; this module binds it to run state.

use std::collections::{BTreeMap, VecDeque};

use crate::gate::Inbound;

/// What one leg has received and not yet answered: command type → the
/// client-defined id of each unanswered request of that type, oldest first,
/// `None` where it carried none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MscmlRequests(BTreeMap<String, VecDeque<Option<String>>>);

impl MscmlRequests {
    /// Queue every MSCML request `inbound` carries. A response, or a request
    /// with no MSCML body, leaves the record as it was.
    pub fn remember(&mut self, inbound: &Inbound) {
        if inbound.method.is_none() {
            return;
        }
        for request in
            sip_message::mscml::requests_in(inbound.content_type.as_deref(), &inbound.body)
        {
            self.0.entry(request.command).or_default().push_back(request.id);
        }
    }

    /// Answer the oldest unanswered request of each type `answered` names, once
    /// per entry: what a composed message's responses consumed ([`Echo`]).
    pub fn answer(&mut self, answered: &[String]) {
        for command in answered {
            if let Some(queue) = self.0.get_mut(command) {
                queue.pop_front();
            }
        }
    }

    /// The id of the `nth` oldest unanswered request of `command`: `Some(None)`
    /// where it carried none, `None` where there is no such request.
    fn unanswered(&self, command: &str, nth: usize) -> Option<Option<&str>> {
        self.0.get(command)?.get(nth).map(Option::as_deref)
    }
}

/// One message's echo pass: each MSCML `<response>` body it emits takes the
/// next unanswered request of its type, so two responses of one type in one
/// message answer two requests. The record is read, never written: the pass
/// names what it answered, and the caller hands that to
/// [`MscmlRequests::answer`] once the message is composed.
pub struct Echo<'a> {
    requests: &'a MscmlRequests,
    answered: Vec<String>,
}

impl<'a> Echo<'a> {
    pub fn new(requests: &'a MscmlRequests) -> Self {
        Echo { requests, answered: Vec::new() }
    }

    /// `payload` as a send emits it under `content_type`: an MSCML `<response>`
    /// takes the id of the next unanswered request of its type; every other
    /// payload, and a response of a type with none left, rides byte for byte.
    pub fn body(&mut self, payload: Vec<u8>, content_type: &str) -> Vec<u8> {
        if !sip_message::mscml::is_mscml(content_type) {
            return payload;
        }
        let Ok(text) = std::str::from_utf8(&payload) else { return payload };
        let Some(command) = sip_message::mscml::response_request(text) else { return payload };
        let nth = self.answered.iter().filter(|c| *c == command).count();
        let Some(id) = self.requests.unanswered(command, nth) else { return payload };
        let echoed = sip_message::mscml::with_response_id(text, id).into_bytes();
        self.answered.push(command.to_string());
        echoed
    }

    /// The request types this pass answered, one entry per response.
    pub fn answered(self) -> Vec<String> {
        self.answered
    }
}
