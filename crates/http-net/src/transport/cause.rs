//! Classification of a `reqwest` client error into a [`FailureCause`].
//!
//! The useful signal is spread across the error chain: `reqwest` states
//! connect-vs-request and timeout, hyper/`std::io` underneath carries the
//! refused/reset/dns detail. Walk the chain, take the first thing that is
//! definite, and fall back to `other` rather than guessing.

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
    let mut source: Option<&(dyn Error + 'static)> = Some(err);
    while let Some(e) = source {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            match io.kind() {
                std::io::ErrorKind::ConnectionRefused => return FailureCause::Refused,
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe => {
                    return FailureCause::ConnReset
                }
                std::io::ErrorKind::TimedOut => {
                    return if err.is_connect() {
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
/// Order matters: the markers that name a whole failure phrase are tested
/// first, and the bare `tls`/`ssl` tokens — which the target url embedded in
/// `reqwest`'s own Display can carry — only after none of them matched.
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
    use super::*;

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
        // reqwest's top-level Display embeds the request url, so the target's
        // own name reaches the classifier.
        assert_eq!(
            classify_text(
                "error sending request for url (http://tls-limiter.svc:8080/ssl/admit): \
                 tcp connect error: Connection refused (os error 111)"
            ),
            Some(FailureCause::Refused),
        );
        assert_eq!(
            classify_text("error sending request for url (http://ssl-host:8080/tls): connection reset by peer"),
            Some(FailureCause::ConnReset),
        );
        assert_eq!(
            classify_text("error sending request for url (http://tls-limiter.svc:8080/): dns error"),
            Some(FailureCause::Dns),
        );
        // A genuine TLS failure still classifies as one.
        assert_eq!(
            classify_text("error sending request for url (http://peer:8080/): invalid peer certificate: Expired"),
            Some(FailureCause::Tls),
        );
    }
}
