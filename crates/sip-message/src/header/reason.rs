//! [`Reason`] (RFC 3326): the protocol and cause a `BYE` or `CANCEL` states.

use super::aliases::Reason;

/// The RFC 3326 §2 protocol token of an ITU-T Q.850 cause.
pub const Q850: &str = "Q.850";

impl Reason {
    /// The protocol the value states: `SIP`, `Q.850`, or an extension token.
    pub fn protocol(&self) -> &str {
        self.token()
    }

    /// The `cause` the value states (RFC 3326 §2, `1*DIGIT`); `None` when it
    /// states none or one that is not a decimal code.
    pub fn cause(&self) -> Option<u16> {
        let value = self.param("cause")?.as_str()?;
        if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        value.parse().ok()
    }
}

/// The cause `reasons` state for `protocol`, matched case-insensitively: the
/// first value naming it (RFC 3326 §2 allows one per protocol), `None` when
/// none names it or the one that does states no cause.
pub fn stated_cause(reasons: &[Reason], protocol: &str) -> Option<u16> {
    reasons.iter().find(|r| r.is(protocol)).and_then(Reason::cause)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::HeaderValue;
    use crate::sip_str::SipStr;

    fn reason(raw: &str) -> Reason {
        Reason::parse(&SipStr::owned(raw)).unwrap()
    }

    #[test]
    fn a_reason_reads_its_protocol_and_cause() {
        let r = reason("Q.850;cause=16;text=\"Terminated\"");
        assert_eq!(r.protocol(), "Q.850");
        assert_eq!(r.cause(), Some(16));
        let sip = reason("SIP;cause=200;text=\"Call completed elsewhere\"");
        assert_eq!((sip.protocol(), sip.cause()), ("SIP", Some(200)));
    }

    /// The grammar's LWS around `;` and `=` (RFC 3261 §25.1 SEMI, EQUAL) and a
    /// zero-padded code read as the same cause.
    #[test]
    fn spacing_and_leading_zeros_state_the_same_cause() {
        for raw in ["Q.850 ;cause=16", "Q.850; cause=16", "Q.850;cause = 16", "q.850;cause=016"] {
            assert_eq!(stated_cause(&[reason(raw)], Q850), Some(16), "{raw:?}");
        }
    }

    #[test]
    fn a_value_without_a_decimal_cause_states_none() {
        assert_eq!(reason("SIP;text=\"Call terminated\"").cause(), None);
        assert_eq!(reason("Q.850;cause").cause(), None);
        assert_eq!(reason("Q.850;cause=abc").cause(), None);
        assert_eq!(reason("Q.850;cause=99999999").cause(), None);
    }

    /// Several values (one per protocol) are read by protocol, never by position.
    #[test]
    fn the_cause_is_the_one_stated_for_the_protocol() {
        let both = [reason("SIP;cause=600"), reason("Q.850;cause=17")];
        assert_eq!(stated_cause(&both, Q850), Some(17));
        assert_eq!(stated_cause(&both, "SIP"), Some(600));
        assert_eq!(stated_cause(&both[..1], Q850), None, "no Q.850 value");
        assert_eq!(stated_cause(&[], Q850), None);
    }
}
