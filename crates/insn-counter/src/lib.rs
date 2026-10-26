//! Instructions retired in user space by the calling thread over a measured
//! region — the work count behind scaling tests.
//!
//! An allocation count (`alloc-counter`) sees the work that reaches the
//! allocator; a loop that shifts a vector or compares strings allocates
//! nothing. The hardware instruction counter sees both, and unlike a clock it
//! does not move with load, frequency or the other processes on the box: the
//! same code over the same input retires the same instructions to within a
//! few parts in ten thousand. A test states a shape (how the count grows when
//! the input doubles), never an absolute figure, so the counter's small
//! run-to-run noise and its dependence on the compiler stay out of the verdict.
//!
//! ## Scope
//!
//! The counter follows the thread that opened it, user space only (the kernel
//! and hypervisor are excluded, which is what an unprivileged process may
//! count at `perf_event_paranoid` 2). [`InsnCounter::open`] fails where the
//! kernel offers no hardware counter — a VM without a virtual PMU, a container
//! whose seccomp profile refuses `perf_event_open` — and the caller decides
//! what a missing counter means for its verdict.

use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// `PERF_TYPE_HARDWARE`.
const PERF_TYPE_HARDWARE: u32 = 0;
/// `PERF_COUNT_HW_INSTRUCTIONS`.
const PERF_COUNT_HW_INSTRUCTIONS: u64 = 1;
/// `PERF_ATTR_SIZE_VER0`: the first 64 bytes of `perf_event_attr`, which every
/// kernel with `perf_event_open` accepts.
const PERF_ATTR_SIZE_VER0: u32 = 64;
/// `perf_event_attr` flag bits: `disabled`, `exclude_kernel`, `exclude_hv`.
const FLAG_DISABLED: u64 = 1 << 0;
const FLAG_EXCLUDE_KERNEL: u64 = 1 << 5;
const FLAG_EXCLUDE_HV: u64 = 1 << 6;
/// `_IO('$', 0)`, `_IO('$', 1)`, `_IO('$', 3)`.
const PERF_EVENT_IOC_ENABLE: libc::c_ulong = 0x2400;
const PERF_EVENT_IOC_DISABLE: libc::c_ulong = 0x2401;
const PERF_EVENT_IOC_RESET: libc::c_ulong = 0x2403;

/// The `PERF_ATTR_SIZE_VER0` layout of `struct perf_event_attr`
/// (`linux/perf_event.h`).
#[repr(C)]
struct PerfEventAttr {
    kind: u32,
    size: u32,
    config: u64,
    sample_period: u64,
    sample_type: u64,
    read_format: u64,
    flags: u64,
    wakeup_events: u32,
    bp_type: u32,
    config1: u64,
}

/// An open user-space instruction counter on the calling thread.
pub struct InsnCounter {
    file: File,
}

impl InsnCounter {
    /// Opens a counter of the instructions the calling thread retires in user
    /// space, stopped at zero.
    pub fn open() -> io::Result<Self> {
        let attr = PerfEventAttr {
            kind: PERF_TYPE_HARDWARE,
            size: PERF_ATTR_SIZE_VER0,
            config: PERF_COUNT_HW_INSTRUCTIONS,
            sample_period: 0,
            sample_type: 0,
            read_format: 0,
            flags: FLAG_DISABLED | FLAG_EXCLUDE_KERNEL | FLAG_EXCLUDE_HV,
            wakeup_events: 0,
            bp_type: 0,
            config1: 0,
        };
        // SAFETY: `attr` is a live `perf_event_attr` of the size its `size`
        // field states; pid 0 / cpu -1 / group -1 / flags 0 name the calling
        // thread on any CPU, no group. The kernel only reads `attr`.
        #[allow(unsafe_code)]
        let fd = unsafe {
            libc::syscall(
                libc::SYS_perf_event_open,
                &attr as *const PerfEventAttr,
                0 as libc::pid_t,
                -1 as libc::c_int,
                -1 as libc::c_int,
                0 as libc::c_ulong,
            )
        };
        let fd = libc::c_int::try_from(fd).map_err(|_| io::Error::other("fd out of range"))?;
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the syscall just returned `fd` as a new descriptor this
        // process owns and nothing else holds.
        #[allow(unsafe_code)]
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(Self { file: File::from(owned) })
    }

    /// Runs `f` and reports the instructions the calling thread retired in
    /// user space while it ran, alongside its result.
    pub fn measure<T>(&mut self, f: impl FnOnce() -> T) -> io::Result<(T, u64)> {
        self.ioctl(PERF_EVENT_IOC_RESET)?;
        self.ioctl(PERF_EVENT_IOC_ENABLE)?;
        let out = f();
        self.ioctl(PERF_EVENT_IOC_DISABLE)?;
        let mut count = [0u8; 8];
        self.file.read_exact(&mut count)?;
        Ok((out, u64::from_ne_bytes(count)))
    }

    fn ioctl(&self, request: libc::c_ulong) -> io::Result<()> {
        // SAFETY: the descriptor is the perf event this value owns; the three
        // requests used here take no argument.
        #[allow(unsafe_code)]
        let rc = unsafe { libc::ioctl(self.file.as_raw_fd(), request, 0) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A loop twice as long retires about twice the instructions.
    #[test]
    fn a_doubled_loop_retires_about_double() {
        let mut counter = InsnCounter::open().expect("a user-space instruction counter");
        let work = |n: u64| (0..n).fold(0u64, |acc, i| std::hint::black_box(acc ^ i));
        let (_, small) = counter.measure(|| work(100_000)).unwrap();
        let (_, large) = counter.measure(|| work(200_000)).unwrap();
        let ratio = large as f64 / small as f64;
        assert!((1.9..2.1).contains(&ratio), "{small} -> {large}: ratio {ratio}");
    }
}
