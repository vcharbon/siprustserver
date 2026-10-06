//! What the kernel knows about a bound UDP socket and the stack cannot count
//! itself: its effective buffer sizes and the datagrams it dropped before any
//! `recv_from` saw them.

use std::os::fd::AsFd;

use crate::types::SocketBuffers;

/// The buffer sizes the kernel granted `socket`, as `getsockopt` reports them
/// (Linux doubles the request for its own overhead and clamps it at
/// `net.core.rmem_max` / `wmem_max`).
pub fn socket_buffers(socket: &impl AsFd) -> std::io::Result<SocketBuffers> {
    let socket = socket2::SockRef::from(socket);
    Ok(SocketBuffers { recv: socket.recv_buffer_size()?, send: socket.send_buffer_size()? })
}

/// Whether the kernel granted less than `requested`, given the size it
/// reports back: Linux reports twice the grant, so an unclamped request reads
/// back doubled.
pub fn was_clamped(requested: usize, reported: usize) -> bool {
    let unclamped = if cfg!(target_os = "linux") { requested.saturating_mul(2) } else { requested };
    reported < unclamped
}

/// Datagrams the kernel dropped on `socket` since it was created: a full
/// receive buffer, plus the few checksum and filter drops the kernel charges to
/// the same socket counter (`sk_drops`, the `drops` column of `/proc/net/udp`).
/// The kernel keeps it as a 32-bit counter, so it wraps at 2^32. Always 0 off
/// Linux.
pub fn rx_dropped(socket: &impl AsFd) -> std::io::Result<u64> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;

        const DROPS: usize = libc::SK_MEMINFO_DROPS as usize;
        // Sized up to the drop slot: the kernel copies at most `len` bytes of
        // its array and reports how many it wrote.
        let mut meminfo = [0u32; DROPS + 1];
        let mut len = std::mem::size_of_val(&meminfo) as libc::socklen_t;
        // SAFETY: the fd is borrowed live for the call, and the out-pointer and
        // length describe the `u32` array `SO_MEMINFO` fills.
        #[allow(unsafe_code)]
        let rc = unsafe {
            libc::getsockopt(
                socket.as_fd().as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_MEMINFO,
                meminfo.as_mut_ptr().cast(),
                &mut len,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if (len as usize) < std::mem::size_of_val(&meminfo) {
            return Err(std::io::Error::other("SO_MEMINFO carries no drop count"));
        }
        Ok(u64::from(meminfo[DROPS]))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = socket;
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::was_clamped;

    /// A 4 MiB request under a 3 MiB `rmem_max` reads back 6 MiB, above the
    /// request, and is still a clamp.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_buffer_reported_below_twice_the_request_was_clamped() {
        const MIB: usize = 1 << 20;
        assert!(!was_clamped(4 * MIB, 8 * MIB));
        assert!(was_clamped(4 * MIB, 6 * MIB));
        assert!(was_clamped(4 * MIB, 416 * 1024));
    }
}
