//! [`AbortOnDrop`] — a task that dies with whoever holds it.

use tokio::task::JoinHandle;

/// Aborts the task it holds when dropped: a supervised task dies with its
/// supervisor, a reader with the flow that owns its connection.
pub(crate) struct AbortOnDrop(pub(crate) JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
