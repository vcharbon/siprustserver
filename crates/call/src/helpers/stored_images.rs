//! Every message the record keeps as bytes: a request as it left, a response
//! or an ACK retained to be sent again.

use crate::model::provisional::Ladder;
use crate::model::{Call, RequestRecord, RetainedEmission};

/// Each image `call` holds, in record order.
pub fn stored_images(call: &Call) -> Vec<&[u8]> {
    let mut images = Vec::new();
    for leg in std::iter::once(&call.a_leg).chain(call.b_legs.iter()) {
        let on_dialogs = leg.dialogs.iter().flat_map(|d| {
            d.sent_invite.iter().chain(d.answered_invite.iter()).chain(d.requests.iter())
        });
        for record in leg.invite.iter().chain(on_dialogs) {
            record_images(record, &mut images);
        }
    }
    images.extend(call.answered.iter().map(|a| a.response.wire().0));
    let shown = call.provisionals.iter().filter_map(|p| p.shown.as_ref());
    images.extend(shown.filter_map(|s| match &s.ladder {
        Ladder::Repeating(emission) => Some(emission.wire().0),
        _ => None,
    }));
    images
}

/// The images `record` holds, appended to `images`.
fn record_images<'a>(record: &'a RequestRecord, images: &mut Vec<&'a [u8]>) {
    images.extend(record.sent.iter().map(|s| s.image.as_slice()));
    let Some(x) = &record.invite else { return };
    let emissions: [Option<&RetainedEmission>; 5] = [
        x.emitted_ack.as_ref(),
        x.ack_ahead.as_ref(),
        x.answered_2xx.as_ref().map(|u| &u.emission),
        x.rejection.as_ref(),
        x.provisional.as_ref(),
    ];
    images.extend(emissions.into_iter().flatten().map(|e| e.wire().0));
}
