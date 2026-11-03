//! The external face check's variables: the interface `/readyz` requires up
//! and the next hops it requires one of resolved.

use std::net::Ipv4Addr;

use super::reader::EnvReader;
use crate::external_face::{valid_interface_name, FaceCheck};

/// `PROXY_EXT_INTERFACE` (an interface name) and `PROXY_EXT_NEXT_HOPS`
/// (comma-separated IPv4 addresses): the external face `/readyz` requires
/// usable. Next hops without an interface are refused.
pub(super) fn external_face_check(env: &mut EnvReader) -> Option<FaceCheck> {
    let interface = env.present("PROXY_EXT_INTERFACE");
    let hops = env.present("PROXY_EXT_NEXT_HOPS").unwrap_or_default();
    let mut next_hops = Vec::new();
    for entry in hops.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        match entry.parse::<Ipv4Addr>() {
            Ok(ip) => next_hops.push(ip),
            Err(_) => {
                env.refuse(format!("PROXY_EXT_NEXT_HOPS entry {entry:?} is not an IPv4 address"))
            }
        }
    }
    let Some(interface) = interface else {
        if !next_hops.is_empty() {
            env.refuse(
                "PROXY_EXT_NEXT_HOPS needs PROXY_EXT_INTERFACE: the next hops are read on that \
                 interface"
                    .to_string(),
            );
        }
        return None;
    };
    if !valid_interface_name(&interface) {
        env.refuse(format!(
            "PROXY_EXT_INTERFACE {interface:?} is not an interface name (1 to 15 bytes, no '/', \
             no whitespace)"
        ));
        return None;
    }
    Some(FaceCheck { interface, next_hops })
}
