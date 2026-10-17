//! The front-proxy env grammar: `B2BUA_OUTBOUND_PROXY`, the IP-literal
//! `ip:port` every b-leg request is forced through, and
//! `B2BUA_REQUIRE_OUTBOUND_PROXY`, the cluster profile's refusal to boot
//! without it.

/// Boot-time invariant for cluster-profile workers (pure, unit-testable):
/// when `B2BUA_REQUIRE_OUTBOUND_PROXY` is set, `B2BUA_OUTBOUND_PROXY` MUST be
/// configured — a worker without it dials b-leg R-URIs pod-direct, bypassing
/// the front proxy and (in the dual-face network split) reaching for a plane
/// it must not see. Checked at [`RunnerEnv::bind`](crate::RunnerEnv::bind);
/// exported for runners that assemble their own env.
pub fn validate_outbound_proxy_requirement(
    require_outbound_proxy: bool,
    outbound_proxy_set: bool,
) -> Result<(), String> {
    if require_outbound_proxy && !outbound_proxy_set {
        return Err("B2BUA_REQUIRE_OUTBOUND_PROXY is set but B2BUA_OUTBOUND_PROXY is unset: this \
             profile REQUIRES every b-leg to traverse the front proxy (workers must never \
             dial callees pod-direct — only the proxy bridges to the external plane). Set \
             B2BUA_OUTBOUND_PROXY=ip:port or unset B2BUA_REQUIRE_OUTBOUND_PROXY for \
             local/dev"
            .to_string());
    }
    Ok(())
}

/// `B2BUA_OUTBOUND_PROXY` as the `(host, port)` the egress policy preloads: an
/// IP-literal `ip:port` (`[v6]:port` for IPv6), since the worker resolves no
/// name and a name proxy would leave every b-leg request unsent.
pub fn parse_outbound_proxy(value: &str) -> Result<(String, u16), String> {
    value.parse::<std::net::SocketAddr>().map(|a| (a.ip().to_string(), a.port())).map_err(|_| {
        format!(
            "B2BUA_OUTBOUND_PROXY must be an IP-literal ip:port (the worker resolves no name), \
             got {value:?}"
        )
    })
}
