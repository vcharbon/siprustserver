//! Classification of a `reqwest` client error into a [`FailureCause`].
//!
//! The useful signal lives *below* the top of the chain. `reqwest`'s own
//! `Display` names the error kind and the target url and nothing else — it does
//! not append its source — while hyper and `std::io` underneath carry the
//! refused/reset/dns detail. So the typed API answers "timeout?" and
//! "connect?", the chain **below** the reqwest error answers "why", and
//! anything the chain does not identify is `other` rather than a guess.

use std::error::Error;

use crate::failures::FailureCause;

/// Classify one `reqwest` send/body error.
pub fn classify(err: &reqwest::Error) -> FailureCause {
    if err.is_timeout() {
        return if err.is_connect() {
            FailureCause::ConnectTimeout
        } else {
            FailureCause::RequestTimeout
        };
    }
    classify_below(err, err.is_connect())
}

/// Walk the error chain **below** `top` and return the first definite cause.
///
/// `top`'s own text is never matched: it is the url-bearing reqwest message, so
/// matching it would let a target named `/tls/admit` report a refused
/// connection as a TLS failure. The kind `top` names is already read off the
/// typed API by [`classify`], so nothing is lost. `connect` says whether the
/// failure happened while connecting, which is what splits a timed-out io leaf
/// between the connect and request budgets.
fn classify_below(top: &(dyn Error + 'static), connect: bool) -> FailureCause {
    let mut source = top.source();
    while let Some(e) = source {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            match io.kind() {
                std::io::ErrorKind::ConnectionRefused => return FailureCause::Refused,
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe => {
                    return FailureCause::ConnReset
                }
                std::io::ErrorKind::TimedOut => {
                    return if connect {
                        FailureCause::ConnectTimeout
                    } else {
                        FailureCause::RequestTimeout
                    }
                }
                _ => {}
            }
        }
        if let Some(cause) = classify_text(&e.to_string()) {
            return cause;
        }
        source = e.source();
    }
    FailureCause::Other
}

/// The chain's leaf is often a plain message (`hyper_util`'s resolver and the
/// TLS stack both surface as text). Only definite markers count.
///
/// Order matters: a marker naming a whole failure phrase is tested first, and
/// the bare `tls`/`ssl`/`certificate` tokens — which any incidental text can
/// carry — are a catch-all that applies only when nothing definite matched.
fn classify_text(text: &str) -> Option<FailureCause> {
    let t = text.to_ascii_lowercase();
    if t.contains("dns error") || t.contains("failed to lookup address") || t.contains("name or service not known") {
        return Some(FailureCause::Dns);
    }
    if t.contains("connection refused") {
        return Some(FailureCause::Refused);
    }
    if t.contains("connection reset") || t.contains("broken pipe") {
        return Some(FailureCause::ConnReset);
    }
    if t.contains("tls") || t.contains("certificate") || t.contains("ssl") {
        return Some(FailureCause::Tls);
    }
    None
}

#[cfg(test)]
mod tests {
    use std::fmt;

    use super::*;

    /// A chain link with a chosen message — the shape `reqwest` wraps: a
    /// url-bearing top, a hyper middle, an `io::Error` leaf.
    #[derive(Debug)]
    struct Link {
        text: &'static str,
        source: Option<Box<dyn Error + Send + Sync + 'static>>,
    }

    impl Link {
        fn new(text: &'static str, source: impl Error + Send + Sync + 'static) -> Self {
            Self {
                text,
                source: Some(Box::new(source)),
            }
        }
    }

    impl fmt::Display for Link {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.text)
        }
    }

    impl Error for Link {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            self.source.as_ref().map(|e| &**e as &(dyn Error + 'static))
        }
    }

    fn io(kind: std::io::ErrorKind, text: &'static str) -> std::io::Error {
        std::io::Error::new(kind, text)
    }

    #[test]
    fn definite_markers_classify_and_anything_else_is_other() {
        assert_eq!(classify_text("dns error: failed to lookup address information"), Some(FailureCause::Dns));
        assert_eq!(classify_text("invalid peer certificate"), Some(FailureCause::Tls));
        assert_eq!(classify_text("tcp connect error: Connection refused (os error 111)"), Some(FailureCause::Refused));
        assert_eq!(classify_text("connection reset by peer"), Some(FailureCause::ConnReset));
        assert_eq!(classify_text("error sending request"), None);
    }

    #[test]
    fn a_target_named_tls_does_not_disguise_the_transport_failure() {
        // The real reqwest shape: the top-level message carries the request url
        // and NOT its source, so the url's own `/tls/admit` is the only `tls` in
        // it. The refused leaf below must still win.
        let top = Link::new(
            "error sending request for url (http://limiter.svc:8080/tls/admit)",
            Link::new(
                "client error (Connect)",
                io(std::io::ErrorKind::ConnectionRefused, "tcp connect error"),
            ),
        );
        assert_eq!(classify_below(&top, true), FailureCause::Refused);

        let reset = Link::new(
            "error sending request for url (http://ssl-limiter.svc:8080/v1/certificates)",
            io(std::io::ErrorKind::ConnectionReset, "reset"),
        );
        assert_eq!(classify_below(&reset, false), FailureCause::ConnReset);

        // Nothing definite below a tls-named url is `other`, never `tls`.
        let opaque = Link::new(
            "error sending request for url (http://limiter.svc:8080/tls/admit)",
            io(std::io::ErrorKind::Other, "client error (SendRequest)"),
        );
        assert_eq!(classify_below(&opaque, false), FailureCause::Other);
    }

    #[test]
    fn a_genuine_tls_failure_below_the_top_still_classifies_as_one() {
        let top = Link::new(
            "error sending request for url (http://peer:8080/v1/admit)",
            io(std::io::ErrorKind::InvalidData, "invalid peer certificate: Expired"),
        );
        assert_eq!(classify_below(&top, false), FailureCause::Tls);
    }

    #[test]
    fn a_timed_out_leaf_splits_by_the_phase_it_timed_out_in() {
        let top = Link::new(
            "error sending request for url (http://peer:8080/v1/admit)",
            io(std::io::ErrorKind::TimedOut, "timed out"),
        );
        assert_eq!(classify_below(&top, true), FailureCause::ConnectTimeout);
        assert_eq!(classify_below(&top, false), FailureCause::RequestTimeout);
    }
}
