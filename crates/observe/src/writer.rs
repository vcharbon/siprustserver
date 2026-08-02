//! Non-blocking, bounded, lossy stdout writer for lifecycle logs.
//!
//! A formatted line is handed to a bounded channel drained by one dedicated
//! writer thread. When the queue is full the line is DROPPED and
//! `counters::LOG_LINES_DROPPED` bumped — a SIP task never blocks on stdout,
//! whatever the reader downstream is doing. The returned [`LogWriterGuard`]
//! drains and joins the thread, so a runner that holds it until exit loses no
//! line to shutdown.

use std::io::{self, Write};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::thread::JoinHandle;

use tracing_subscriber::fmt::MakeWriter;

use crate::counters;

/// Lines the writer queue holds before it starts dropping. Sized so a burst of
/// lifecycle logging rides through while a stalled stdout reader can never
/// retain unbounded memory.
const QUEUE_LINES: usize = 8192;

enum Msg {
    Line(Vec<u8>),
    Shutdown,
}

/// The `MakeWriter` the fmt layer emits through. Cloneable and cheap: every
/// event gets a fresh [`LineBuf`] that ships its bytes on drop.
#[derive(Clone)]
pub struct LossyStdout {
    tx: SyncSender<Msg>,
}

/// Drains the queue and joins the writer thread on drop.
pub struct LogWriterGuard {
    tx: Option<SyncSender<Msg>>,
    handle: Option<JoinHandle<()>>,
}

/// Spawn the writer thread and return the `MakeWriter` plus its shutdown guard.
pub fn spawn() -> (LossyStdout, LogWriterGuard) {
    let (tx, rx) = sync_channel::<Msg>(QUEUE_LINES);
    let handle = std::thread::Builder::new()
        .name("observe-log-writer".to_string())
        .spawn(move || drain(rx))
        .expect("spawn log writer thread");
    (
        LossyStdout { tx: tx.clone() },
        LogWriterGuard { tx: Some(tx), handle: Some(handle) },
    )
}

/// The writer thread body.
fn drain(rx: Receiver<Msg>) {
    let stdout = io::stdout();
    drain_into(rx, io::BufWriter::with_capacity(64 * 1024, stdout.lock()));
}

/// Write every queued line to `out` until the sentinel or a disconnect, flushing
/// whenever the queue has gone quiet so an idle process shows its last line
/// immediately.
///
/// The quiet probe is a lookahead: the message it takes is carried into the next
/// iteration, never discarded — a queued line is written and the queued sentinel
/// ends the loop, whatever the arrival timing.
fn drain_into<W: Write>(rx: Receiver<Msg>, mut out: W) {
    let mut next = rx.recv().ok();
    while let Some(msg) = next.take() {
        let line = match msg {
            Msg::Line(line) => line,
            Msg::Shutdown => break,
        };
        let _ = out.write_all(&line);
        next = match rx.try_recv() {
            Ok(queued) => Some(queued),
            Err(_) => {
                let _ = out.flush();
                rx.recv().ok()
            }
        };
    }
    // Lines that raced the sentinel still belong to this process.
    while let Ok(Msg::Line(line)) = rx.try_recv() {
        let _ = out.write_all(&line);
    }
    let _ = out.flush();
}

impl Drop for LogWriterGuard {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Msg::Shutdown);
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// One event's bytes, shipped to the writer thread when the fmt layer is done
/// with it.
pub struct LineBuf {
    buf: Vec<u8>,
    tx: SyncSender<Msg>,
}

impl Write for LineBuf {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.ship();
        Ok(())
    }
}

impl LineBuf {
    fn ship(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let line = std::mem::take(&mut self.buf);
        match self.tx.try_send(Msg::Line(line)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                counters::bump(&counters::LOG_LINES_DROPPED)
            }
        }
    }
}

impl Drop for LineBuf {
    fn drop(&mut self) {
        self.ship();
    }
}

impl<'a> MakeWriter<'a> for LossyStdout {
    type Writer = LineBuf;

    fn make_writer(&'a self) -> Self::Writer {
        // 256 B covers a typical lifecycle line without a second growth.
        LineBuf { buf: Vec::with_capacity(256), tx: self.tx.clone() }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::*;

    /// How long a test waits for the writer thread before calling it hung.
    const JOIN_TIMEOUT: Duration = Duration::from_secs(5);

    /// A `Write` sink the test reads once the writer thread has ended.
    #[derive(Clone)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuf {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Queue `msgs`, run [`drain_into`] on its own thread with the sender still
    /// alive, and return what it wrote. Panics rather than hangs if the thread
    /// never ends.
    fn drained(msgs: Vec<Msg>) -> Vec<u8> {
        let (tx, rx) = sync_channel::<Msg>(16);
        for m in msgs {
            tx.send(m).expect("queue fits the test burst");
        }
        let buf = SharedBuf(Arc::new(Mutex::new(Vec::new())));
        let sink = buf.clone();
        let (done_tx, done_rx) = sync_channel::<()>(1);
        let handle = std::thread::spawn(move || {
            drain_into(rx, sink);
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(JOIN_TIMEOUT)
            .expect("writer thread ends on the queued sentinel while a sender is still alive");
        handle.join().unwrap();
        drop(tx);
        let out = buf.0.lock().unwrap().clone();
        out
    }

    #[test]
    fn a_queued_burst_is_written_in_full_and_the_queued_sentinel_ends_the_thread() {
        let out = drained(vec![
            Msg::Line(b"one\n".to_vec()),
            Msg::Line(b"two\n".to_vec()),
            Msg::Line(b"three\n".to_vec()),
            Msg::Shutdown,
        ]);
        assert_eq!(String::from_utf8(out).unwrap(), "one\ntwo\nthree\n");
    }

    #[test]
    fn lines_queued_behind_the_sentinel_are_still_written() {
        let out = drained(vec![
            Msg::Line(b"before\n".to_vec()),
            Msg::Shutdown,
            Msg::Line(b"raced\n".to_vec()),
        ]);
        assert_eq!(String::from_utf8(out).unwrap(), "before\nraced\n");
    }

    #[test]
    fn dropping_the_guard_joins_the_spawned_writer() {
        let (done_tx, done_rx) = sync_channel::<()>(1);
        // The guard is dropped on another thread so a join that never returns
        // fails this test instead of hanging the lane.
        std::thread::spawn(move || {
            let (make, guard) = spawn();
            drop(guard);
            // The `MakeWriter` outlives the guard, as it does in a real process
            // where the global subscriber owns it.
            drop(make);
            let _ = done_tx.send(());
        });
        done_rx.recv_timeout(JOIN_TIMEOUT).expect("guard drop joins the writer thread");
    }

    /// One test, because both halves read the same process-wide drop counter
    /// and the default lane runs tests concurrently.
    #[test]
    fn overflow_drops_lines_against_the_counter_and_empty_events_ship_nothing() {
        let before = counters::get(&counters::LOG_LINES_DROPPED);
        // A channel nobody drains: every send past the bound is a counted drop.
        let (tx, rx) = sync_channel::<Msg>(1);
        let make = LossyStdout { tx };
        for _ in 0..4 {
            let mut w = make.make_writer();
            w.write_all(b"line\n").unwrap();
        }
        assert_eq!(
            counters::get(&counters::LOG_LINES_DROPPED) - before,
            3,
            "one line fits the bound, the other three are dropped"
        );

        // An event that wrote no bytes ships no line and drops nothing.
        let (tx2, rx2) = sync_channel::<Msg>(1);
        let empty = LossyStdout { tx: tx2 };
        let after_fill = counters::get(&counters::LOG_LINES_DROPPED);
        drop(empty.make_writer());
        assert!(rx2.try_recv().is_err(), "no bytes written = no line shipped");
        assert_eq!(counters::get(&counters::LOG_LINES_DROPPED), after_fill);
        drop(rx);
    }
}
