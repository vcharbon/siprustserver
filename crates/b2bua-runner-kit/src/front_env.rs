//! The addresses a worker states, and its deployment mode: `B2BUA_ADVERTISE`,
//! the worker's own address, and `B2BUA_MODE`, `proxied` or `standalone`, with
//! the addresses each mode takes. Every parameter the mode requires must be
//! there and valid, and every parameter it does not take must be absent: an
//! inconsistent configuration refuses the boot instead of stating a wrong
//! Contact.
//!
//! | variable                       | `proxied`                   | `standalone` |
//! |--------------------------------|-----------------------------|--------------|
//! | `B2BUA_ADVERTISE`              | required                    | required     |
//! | `B2BUA_OUTBOUND_PROXY`         | required                    | refused      |
//! | `B2BUA_CONTACT_ADDRESS`        | required, not the worker's  | refused      |
//! | `B2BUA_REPL` on                | allowed                     | refused      |
//! | `B2BUA_REQUIRE_OUTBOUND_PROXY` | refused                     | refused      |

use std::net::SocketAddr;

use b2bua::config::Front;

use crate::{is_truthy, stated};

/// What the worker states about where it stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerAddresses {
    /// `B2BUA_ADVERTISE`: the worker's own `ip:port`, on every Via it sends;
    /// standalone, on every Contact too.
    pub advertise: SocketAddr,
    /// The deployment mode, with the proxy's addresses behind it.
    pub front: Front,
}

/// The worker's addresses and mode read through `get`; `Err` names the
/// variable that refuses the boot.
pub fn addresses_from_lookup(
    get: impl Fn(&str) -> Option<String>,
) -> Result<WorkerAddresses, String> {
    let read = |name: &str| stated(get(name));
    if read("B2BUA_REQUIRE_OUTBOUND_PROXY").is_some() {
        return Err("B2BUA_REQUIRE_OUTBOUND_PROXY is not a setting: B2BUA_MODE=proxied requires \
                    B2BUA_OUTBOUND_PROXY"
            .to_string());
    }
    let advertise = match read("B2BUA_ADVERTISE") {
        Some(value) => stated_address("B2BUA_ADVERTISE", &value)?,
        None => {
            return Err("B2BUA_ADVERTISE is unset: the worker states its own ip:port on every \
                        Via it sends"
                .to_string())
        }
    };
    let outbound_proxy = read("B2BUA_OUTBOUND_PROXY");
    let contact_address = read("B2BUA_CONTACT_ADDRESS");
    let front = match read("B2BUA_MODE").as_deref() {
        Some("proxied") => {
            let Some(outbound_proxy) = outbound_proxy else {
                return Err("B2BUA_MODE=proxied requires B2BUA_OUTBOUND_PROXY, the proxy's \
                            worker-facing ip:port every b-leg request goes through"
                    .to_string());
            };
            let Some(contact_address) = contact_address else {
                return Err("B2BUA_MODE=proxied requires B2BUA_CONTACT_ADDRESS, the proxy's \
                            peer-facing ip:port every Contact names"
                    .to_string());
            };
            let outbound_proxy = stated_address("B2BUA_OUTBOUND_PROXY", &outbound_proxy)?;
            let contact_address = stated_address("B2BUA_CONTACT_ADDRESS", &contact_address)?;
            if contact_address == advertise {
                return Err(format!(
                    "B2BUA_CONTACT_ADDRESS={contact_address} is B2BUA_ADVERTISE: behind the \
                     proxy a Contact names the proxy, never the worker"
                ));
            }
            Front::Proxied {
                outbound_proxy: host_port(outbound_proxy),
                contact_address: host_port(contact_address),
            }
        }
        Some("standalone") => {
            for (name, value) in [
                ("B2BUA_OUTBOUND_PROXY", &outbound_proxy),
                ("B2BUA_CONTACT_ADDRESS", &contact_address),
            ] {
                if value.is_some() {
                    return Err(format!(
                        "{name} is set but B2BUA_MODE=standalone: a standalone worker has no \
                         front proxy"
                    ));
                }
            }
            if get("B2BUA_REPL").is_some_and(|v| is_truthy(&v)) {
                return Err("B2BUA_REPL is on but B2BUA_MODE=standalone: the copies of a call \
                            need the proxy's owner rule (ADR-0055 §8)"
                    .to_string());
            }
            Front::Standalone
        }
        Some(other) => {
            return Err(format!("B2BUA_MODE must be `proxied` or `standalone`, got {other:?}"))
        }
        None => {
            return Err("B2BUA_MODE is unset: state `proxied` (behind the front proxy) or \
                        `standalone`"
                .to_string())
        }
    };
    Ok(WorkerAddresses { advertise, front })
}

/// `value` of the variable `name` as a stated address: an IP-literal
/// `ip:port` (`[v6]:port` for IPv6), since the worker resolves no name, that
/// a peer can send to: neither unspecified nor port 0.
fn stated_address(name: &str, value: &str) -> Result<SocketAddr, String> {
    let address: SocketAddr = value.parse().map_err(|_| {
        format!("{name} must be an IP-literal ip:port (the worker resolves no name), got {value:?}")
    })?;
    if address.ip().is_unspecified() || address.port() == 0 {
        return Err(format!("{name}={value} names no address a peer can send to"));
    }
    Ok(address)
}

fn host_port(address: SocketAddr) -> (String, u16) {
    (address.ip().to_string(), address.port())
}

#[cfg(test)]
mod tests;
