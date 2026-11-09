//! The invariant over what the record keeps as bytes.

use super::Turn;

/// Every message the record keeps as bytes reads back
/// ([`sip_message::read_stored`]), each read once, on the turn that stored it:
/// the read is a function of the bytes.
pub(super) fn every_stored_image_reads(turn: &Turn<'_>) -> Result<(), String> {
    let held = turn.before.map(call::helpers::stored_images).unwrap_or_default();
    for image in call::helpers::stored_images(turn.after) {
        if held.contains(&image) {
            continue;
        }
        if let Err(refusal) = sip_message::read_stored(image) {
            return Err(format!(
                "a stored image does not read ({}): {}",
                refusal.reason,
                String::from_utf8_lossy(image.get(..image.len().min(80)).unwrap_or_default())
            ));
        }
    }
    Ok(())
}
