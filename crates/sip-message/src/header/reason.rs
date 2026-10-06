//! [`Reason`] (RFC 3326): the protocol and cause a `BYE` or `CANCEL` states.

use crate::sip_str::SipStr;

use super::aliases::Reason;
use super::value::HeaderValue;

/// The RFC 3326 §2 protocol token of an ITU-T Q.850 cause.
pub const Q850: &str = "Q.850";

impl Reason {
    /// The protocol the value states: `SIP`, `Q.850`, or an extension token.
    pub fn protocol(&self) -> &str {
        self.token()
    }

    /// The `cause` the value states (RFC 3326 §2, `1*DIGIT`) as written,
    /// leading zeros kept; `None` when it states none or one that is not
    /// decimal.
    pub fn cause_digits(&self) -> Option<&str> {
        let value = self.param("cause")?.as_str()?;
        (!value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())).then_some(value)
    }

    /// The `cause` the value states as a number; `None` when it states none,
    /// one that is not decimal, or one past `u16`.
    pub fn cause(&self) -> Option<u16> {
        self.cause_digits()?.parse().ok()
    }
}

/// The `Reason` values of `lines` (a message's `Reason` header lines, in wire
/// order) that read: a line that does not parse is skipped, so it hides none
/// of the others.
pub fn readable_reasons(lines: impl IntoIterator<Item = SipStr>) -> Vec<Reason> {
    lines.into_iter().filter_map(|line| Reason::parse_line(&line).ok()).flatten().collect()
}

/// The first of `reasons` naming `protocol`, matched case-insensitively
/// (RFC 3326 §2 allows one value per protocol).
pub fn reason_for<'a>(reasons: &'a [Reason], protocol: &str) -> Option<&'a Reason> {
    reasons.iter().find(|r| r.is(protocol))
}

/// The first value of `reasons` restated alone where it is a Q.850 value,
/// `Q.850;cause=N` with its cause digits as written: every parameter and every
/// other value dropped. `None` where the first value is of another protocol or
/// states no decimal cause.
pub fn q850_cause_alone(reasons: &[Reason]) -> Option<String> {
    let first = reasons.first().filter(|r| r.is(Q850))?;
    let digits = first.cause_digits()?;
    Some(format!("{Q850};cause={digits}"))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The grammar's LWS around `;` and `=` (RFC 3261 §25.1 SEMI, EQUAL) reads
    /// as the same cause; its digits read as written.
    #[test]
    fn spacing_reads_the_same_cause_and_digits_read_as_written() {
        for raw in ["Q.850 ;cause=16", "Q.850; cause=16", "Q.850;cause = 16", "q.850;cause=16"] {
            let reasons = [reason(raw)];
            assert_eq!(reason_for(&reasons, Q850).and_then(Reason::cause), Some(16), "{raw:?}");
        }
        let padded = reason("Q.850;cause=016");
        assert_eq!((padded.cause_digits(), padded.cause()), (Some("016"), Some(16)));
    }

    #[test]
    fn a_value_without_a_decimal_cause_states_none() {
        assert_eq!(reason("SIP;text=\"Call terminated\"").cause(), None);
        assert_eq!(reason("Q.850;cause").cause(), None);
        assert_eq!(reason("Q.850;cause=abc").cause_digits(), None);
        let huge = reason("Q.850;cause=99999999");
        assert_eq!((huge.cause_digits(), huge.cause()), (Some("99999999"), None));
    }

    /// Several values (one per protocol) are read by protocol, never by position.
    #[test]
    fn the_value_is_the_one_stated_for_the_protocol() {
        let both = [reason("SIP;cause=600"), reason("Q.850;cause=17")];
        assert_eq!(reason_for(&both, Q850).and_then(Reason::cause), Some(17));
        assert_eq!(reason_for(&both, "SIP").and_then(Reason::cause), Some(600));
        assert!(reason_for(&both[..1], Q850).is_none(), "no Q.850 value");
    }

    /// A leading Q.850 value is restated alone, digits as written; a list led
    /// by another protocol, or by a Q.850 value stating no cause, restates
    /// nothing. One value per protocol, as RFC 3326 §2 allows.
    #[test]
    fn a_leading_q850_cause_is_restated_alone() {
        let led = readable_reasons(
            ["Q.850 ;cause=031 ;text=\"x\";Location=A", "SIP;cause=487;text=\"ORIGINATOR_CANCEL\""]
                .map(SipStr::owned),
        );
        assert_eq!(q850_cause_alone(&led).as_deref(), Some("Q.850;cause=031"));
        let folded = readable_reasons([SipStr::owned("Q.850;cause=17, SIP;cause=486")]);
        assert_eq!(q850_cause_alone(&folded).as_deref(), Some("Q.850;cause=17"));
        let sip_first = readable_reasons(["SIP;cause=487", "Q.850;cause=16"].map(SipStr::owned));
        assert_eq!(q850_cause_alone(&sip_first), None, "a SIP value leads");
        let sip_only = readable_reasons([SipStr::owned("SIP;cause=480;text=\"NO_ANSWER\"")]);
        assert_eq!(q850_cause_alone(&sip_only), None);
        let causeless = readable_reasons([SipStr::owned("Q.850;text=\"x\"")]);
        assert_eq!(q850_cause_alone(&causeless), None, "the leading Q.850 value states none");
        assert_eq!(q850_cause_alone(&[]), None);
    }

    /// A line that does not parse is skipped; the other lines, and every value
    /// of a comma-folded line, still read.
    #[test]
    fn a_line_that_does_not_read_hides_no_other() {
        let lines = [";cause=1", "SIP;cause=200, Q.850;cause=17"].map(SipStr::owned);
        let reasons = readable_reasons(lines);
        let protocols: Vec<&str> = reasons.iter().map(Reason::protocol).collect();
        assert_eq!(protocols, ["SIP", "Q.850"]);
        assert!(readable_reasons(Vec::<SipStr>::new()).is_empty());
    }
}
