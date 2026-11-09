//! A request the record keeps as bytes, read back in a turn
//! ([`sip_message::read_stored`]).

use call::Call;
use sip_message::{read_stored, SipMessage, SipRequest};

use crate::turn_notes::{TurnNotes, STORED_IMAGE_READS};

/// The request `image` holds. For a rule's predicate, which holds no notes: a
/// refusal reads as no such request, and the oracle's
/// `every-stored-image-reads` reports the image.
pub(crate) fn request(image: &[u8]) -> Option<SipRequest> {
    match read_stored(image).ok()? {
        SipMessage::Request(r) => Some(r),
        SipMessage::Response(_) => None,
    }
}

/// The request `image` of `call` holds, for `step`; a refusal is the defect
/// [`STORED_IMAGE_READS`] on `notes`.
pub(crate) fn request_noted(
    image: &[u8],
    call: &Call,
    step: &str,
    notes: &mut TurnNotes,
) -> Option<SipRequest> {
    match read_stored(image) {
        Ok(SipMessage::Request(r)) => Some(r),
        Ok(SipMessage::Response(r)) => {
            let detail = format!("{step}: the stored image is a {} response", r.status());
            notes.defect(STORED_IMAGE_READS, detail, call);
            None
        }
        Err(refusal) => {
            let detail = format!("{step}: {}", refusal.reason);
            notes.defect(STORED_IMAGE_READS, detail, call);
            None
        }
    }
}
