//! Endian-aware reads over a capture byte slice. Every accessor is bounds-
//! checked and returns `None` past the end, so a truncated or corrupt file
//! ends a walk instead of panicking.

/// Big-endian `u16` (network byte order — protocol headers).
pub fn u16be(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*b.get(off)?, *b.get(off + 1)?]))
}

/// `u16` in the container's declared byte order.
pub fn u16at(b: &[u8], off: usize, le: bool) -> Option<u16> {
    let raw = [*b.get(off)?, *b.get(off + 1)?];
    Some(if le { u16::from_le_bytes(raw) } else { u16::from_be_bytes(raw) })
}

/// `u32` in the container's declared byte order.
pub fn u32at(b: &[u8], off: usize, le: bool) -> Option<u32> {
    let raw = [*b.get(off)?, *b.get(off + 1)?, *b.get(off + 2)?, *b.get(off + 3)?];
    Some(if le { u32::from_le_bytes(raw) } else { u32::from_be_bytes(raw) })
}
