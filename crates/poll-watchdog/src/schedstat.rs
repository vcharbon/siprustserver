//! A thread's id and CPU time as Linux states them under `/proc`: the id
//! from the `/proc/thread-self` link, the CPU time from the first field of
//! `/proc/self/task/<tid>/schedstat` (nanoseconds on the CPU).

use std::io;
use std::time::Duration;

/// The kernel id of the calling thread.
pub fn own_tid() -> io::Result<u32> {
    let link = std::fs::read_link("/proc/thread-self")?;
    let tid = link.file_name().and_then(|name| name.to_str()).and_then(|name| name.parse().ok());
    tid.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, format!("/proc/thread-self -> {link:?}"))
    })
}

/// The CPU time thread `tid` of this process has spent.
pub fn cpu(tid: u32) -> io::Result<Duration> {
    let path = format!("/proc/self/task/{tid}/schedstat");
    let text = std::fs::read_to_string(&path)?;
    parse(&text)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("{path}: {text:?}")))
}

/// The CPU time a `schedstat` line states: its first field, in nanoseconds.
fn parse(text: &str) -> Option<Duration> {
    let ns = text.split_ascii_whitespace().next()?.parse().ok()?;
    Some(Duration::from_nanos(ns))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_field_is_the_cpu_time() {
        assert_eq!(parse("123456789 2000 17\n"), Some(Duration::from_nanos(123_456_789)));
    }

    #[test]
    fn a_line_without_a_count_reads_nothing() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("abc 1 2"), None);
        assert_eq!(parse("-5 1 2"), None);
    }

    #[test]
    fn the_calling_thread_reads_its_own_cpu_time() {
        let tid = own_tid().expect("this thread's id");
        let before = cpu(tid).expect("this thread's schedstat");
        let spin = std::time::Instant::now();
        while spin.elapsed() < Duration::from_millis(20) {
            std::hint::black_box(0);
        }
        let after = cpu(tid).expect("this thread's schedstat");
        assert!(after > before, "the spin spent CPU: {before:?} then {after:?}");
    }
}
