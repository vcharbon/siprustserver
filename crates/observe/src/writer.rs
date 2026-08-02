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

/// The writer thread body: write each line, flushing whenever the queue has
/// gone quiet so an idle process still shows its last line immediately.
fn drain(rx: Receiver<Msg>) {
    let stdout = io::stdout();
    let mut out = io::BufWriter::with_capacity(64 * 1024, stdout.lock());
    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Line(line) => {
                let _ = out.write_all(&line);
                if rx.try_recv().is_err() {
                    let _ = out.flush();
                }
            }
            Msg::Shutdown => break,
        }
    }
    // `try_recv` above may have consumed a line; the remaining backlog is
    // written before the thread ends.
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
    use super::*;

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
