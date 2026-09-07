//! The one Contact statement policy: the stack states its own `Contact` ONLY
//! where the header establishes a dialog or refreshes the dialog target
//! (RFC 3261 §8.1.1.8, §12.1.1, §12.2.1.1) or names a retry target (3xx/485).
//! Anywhere else the header would name nothing a peer may use, so no generator
//! or relay states one.

use crate::method::Method;

/// Whether a request of `method` states the sender's Contact: the
/// dialog-establishing / target-refresh methods — INVITE, initial and re-INVITE
/// (RFC 3261 §12.2.2), UPDATE (RFC 3311 §5.1), SUBSCRIBE / NOTIFY / REFER
/// (RFC 6665 §4.4.1, RFC 3515 §2.4.2), REGISTER (§10.2, the binding itself).
/// ACK, BYE, CANCEL, PRACK, OPTIONS, INFO and MESSAGE refresh no target, so
/// they state none.
pub fn request_states_contact(method: &Method) -> bool {
    matches!(
        method,
        Method::Invite
            | Method::Update
            | Method::Subscribe
            | Method::Notify
            | Method::Refer
            | Method::Register
    )
}

/// Whether a response of `status` answering a `cseq_method` request states the
/// sender's Contact: any 3xx and a 485 name where to retry (§8.3, §21.4.23),
/// and a tagged 1xx / a 2xx does so only when it answers a request
/// [`request_states_contact`] names (§12.1.1 REQUIRES it on a
/// dialog-establishing 2xx; RFC 3311 §5.3 on a target-refresh 2xx). A 2xx to
/// PRACK / OPTIONS / BYE / INFO / MESSAGE / CANCEL establishes nothing and
/// states none; a 100 names no dialog yet.
pub fn response_states_contact(cseq_method: &Method, status: u16) -> bool {
    matches!(status, 300..=399 | 485)
        || (request_states_contact(cseq_method) && matches!(status, 101..=299))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_refresh_requests_state_contact() {
        for m in [Method::Invite, Method::Update, Method::Subscribe, Method::Notify, Method::Refer, Method::Register] {
            assert!(request_states_contact(&m), "{m:?}");
        }
    }

    #[test]
    fn non_refresh_requests_state_none() {
        for m in [Method::Ack, Method::Bye, Method::Cancel, Method::Prack, Method::Options, Method::Info, Method::Message] {
            assert!(!request_states_contact(&m), "{m:?}");
        }
    }

    #[test]
    fn dialog_establishing_responses_state_contact() {
        assert!(response_states_contact(&Method::Invite, 180));
        assert!(response_states_contact(&Method::Invite, 200));
        assert!(response_states_contact(&Method::Update, 200));
    }

    #[test]
    fn redirects_state_contact_for_every_method() {
        assert!(response_states_contact(&Method::Options, 302));
        assert!(response_states_contact(&Method::Invite, 485));
    }

    #[test]
    fn non_refresh_finals_state_none() {
        assert!(!response_states_contact(&Method::Prack, 200));
        assert!(!response_states_contact(&Method::Options, 200));
        assert!(!response_states_contact(&Method::Bye, 200));
        assert!(!response_states_contact(&Method::Info, 200));
        assert!(!response_states_contact(&Method::Message, 200));
        assert!(!response_states_contact(&Method::Cancel, 200));
        assert!(!response_states_contact(&Method::Invite, 100));
        assert!(!response_states_contact(&Method::Invite, 486));
    }
}
