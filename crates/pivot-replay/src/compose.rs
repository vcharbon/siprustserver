//! A lane's URI composition: a Request-URI is addressed at the system under
//! test's ingress, a From/To at the lane's own domain. A `trunk-composed`
//! Request-URI dials `+<trunk><number>`, a numbering-plan fact of the
//! deployment, which is why the LANE composes it and the interpreter only
//! asks.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use pivot_interpreter::UriComposer;
use sip_message::header::Uri;

pub struct LaneComposer {
    pub ingress: SocketAddr,
    pub domain: String,
    /// Tier-2 position token → the number the lane allocated for it.
    pub numbers: BTreeMap<String, String>,
    pub trunk: String,
}

impl UriComposer for LaneComposer {
    fn compose(&self, pos: &str, form: Option<&str>, is_ruri: bool) -> Option<String> {
        let number = self.numbers.get(pos)?;
        let user = match (is_ruri, form) {
            (true, Some("trunk-composed")) => format!("+{}{number}", self.trunk),
            _ => number.clone(),
        };
        Some(if is_ruri {
            format!("sip:{user}@{}", self.ingress)
        } else {
            format!("sip:{user}@{}", self.domain)
        })
    }

    /// A frozen value replays verbatim. Three shapes: a value with a scheme IS
    /// a URI; a value the cut classified (`kind`) is a whole ADDRESS missing
    /// only its scheme — RFC 3323 §4.1.1.3's `anonymous@anonymous.invalid`, or
    /// a host-only URI's bare host — and composing the lane host onto it would
    /// mint a second `@`; a bare userpart composes over the lane host.
    ///
    /// Scheme detection is RFC 3261 §25.1 grammar, never "contains a colon": a
    /// port colon read as a scheme colon emits a name-addr holding no URI, and
    /// the SUT drops the whole request.
    fn frozen(&self, value: &str, kind: Option<&str>, is_ruri: bool) -> String {
        if Uri::value_has_scheme(value) {
            return value.to_string();
        }
        if kind.is_some() || value.contains('@') {
            return format!("sip:{value}");
        }
        let host = if is_ruri { self.ingress.to_string() } else { self.domain.clone() };
        format!("sip:{value}@{host}")
    }
}
