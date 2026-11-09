//! What a seed needs of the bytes a record keeps: a copy holding an image one of
//! its seeds names that does not read cannot serve the transactions its record
//! names (ADR-0052 §11).

use call::Call;
use sip_message::{read_stored, SipMessage};

use super::rejected_dialled::rejected_dialled_reading;
use super::seeds::{answered_reading, seeds_reading};

/// What the reads of a seed's image met: the first refusal.
#[derive(Default)]
pub(super) struct ImageReads(std::cell::Cell<Option<String>>);

impl ImageReads {
    /// The message `image` holds; a refusal is kept and reads as none.
    pub(super) fn read(&self, image: &[u8]) -> Option<SipMessage> {
        match read_stored(image) {
            Ok(message) => Some(message),
            Err(refused) => {
                let first = self.0.take();
                self.0.set(first.or(Some(refused.reason)));
                None
            }
        }
    }
}

/// The first refusal an image the seeds of `call` name at `now_ms` draws: an
/// image no seed reads is not asked. A copy with one cannot serve the
/// transactions its record names.
pub(in crate::router) fn unreadable_seed_image(call: &Call, now_ms: i64) -> Option<String> {
    let reads = ImageReads::default();
    seeds_reading(call, now_ms, &reads);
    answered_reading(call, now_ms, &reads);
    rejected_dialled_reading(call, now_ms, &reads);
    reads.0.take()
}
