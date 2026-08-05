//! The path-MTU-discovery mode this stack pins on every signalling socket
//! (ADR-0026).
//!
//! Signalling is UDP-only and has no TCP fallback, so a SIP message that
//! outgrows the path MTU must still leave: the socket is pinned to
//! `IP_PMTUDISC_DONT` (DF clear), the datagram fragments at IP and the
//! receiving kernel reassembles it. The kernel default (`IP_PMTUDISC_WANT`)
//! already fragments for a UDP socket that never learned a path MTU; pinning
//! `DONT` states the choice instead of inheriting it, and forecloses the
//! `IP_PMTUDISC_DO` mode under which an oversize send fails with `EMSGSIZE`.

/// The mode this stack pins: never set DF, so an oversize datagram fragments
/// instead of failing. IPv4 and IPv6 spell the same value.
#[cfg(target_os = "linux")]
pub const PMTUDISC_DONT: libc::c_int = libc::IP_PMTUDISC_DONT;

/// The mode that makes an oversize send fail with `EMSGSIZE` — what UDP-only
/// signalling must never run under, and the regression the pin forecloses.
#[cfg(target_os = "linux")]
pub const PMTUDISC_DO: libc::c_int = libc::IP_PMTUDISC_DO;

/// Pin the IPv4 or IPv6 path-MTU-discovery mode to "never set DF" on `socket`.
/// Off Linux the option does not exist and the socket keeps the platform
/// default.
pub(crate) fn pin_fragmentation(socket: &socket2::Socket, ipv4: bool) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;

        let (level, name, value) = if ipv4 {
            (libc::IPPROTO_IP, libc::IP_MTU_DISCOVER, libc::IP_PMTUDISC_DONT)
        } else {
            (libc::IPPROTO_IPV6, libc::IPV6_MTU_DISCOVER, libc::IPV6_PMTUDISC_DONT)
        };
        let value: libc::c_int = value;
        // SAFETY: `fd` is owned by `socket` and live for the call; the value
        // pointer and length describe one `c_int`, the shape both options take.
        #[allow(unsafe_code)]
        let rc = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                level,
                name,
                std::ptr::addr_of!(value).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (socket, ipv4);
        Ok(())
    }
}

/// The path-MTU-discovery mode a socket currently carries — what
/// [`pin_fragmentation`] set, read back from the kernel. Linux only; the
/// values are the `IP_PMTUDISC_*` / `IPV6_PMTUDISC_*` constants, so
/// `!= IP_PMTUDISC_DO` is the property an oversize SIP message depends on.
#[cfg(target_os = "linux")]
pub fn mtu_discover_mode(fd: std::os::fd::RawFd, ipv4: bool) -> std::io::Result<libc::c_int> {
    let (level, name) = if ipv4 {
        (libc::IPPROTO_IP, libc::IP_MTU_DISCOVER)
    } else {
        (libc::IPPROTO_IPV6, libc::IPV6_MTU_DISCOVER)
    };
    let mut value: libc::c_int = -1;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `fd` is a live socket for the duration of the call, and the
    // out-pointer/length pair describes the single `c_int` the option returns.
    #[allow(unsafe_code)]
    let rc = unsafe {
        libc::getsockopt(fd, level, name, std::ptr::addr_of_mut!(value).cast(), &mut len)
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(value)
}
