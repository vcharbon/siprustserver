//! Reading a relayed session description apart from its `o=` line, which
//! names the session of the dialog it crosses (RFC 3264 §8) rather than its
//! author's: `sdp_session_continuity` pins that line, the other scenarios pin
//! the rest.

/// `sdp` without its `o=` line.
pub fn apart_from_origin(sdp: &str) -> String {
    sdp.split_inclusive('\n').filter(|l| !l.starts_with("o=")).collect()
}
