//! Each target's input layout: how the bytes libFuzzer hands a target split
//! into what it drives, and the inverse the seed writer encodes with. Every
//! split reads any bytes; a short input yields empty parts.

/// A turn case: the script, the step whose SIP text `text` replaces whole.
pub struct TurnCase<'a> {
    pub script: u8,
    pub step: u8,
    pub text: &'a [u8],
}

/// Byte 0 the script, byte 1 the step, the rest the step's SIP text.
pub fn turn_case(data: &[u8]) -> TurnCase<'_> {
    match data {
        [script, step, text @ ..] => TurnCase { script: *script, step: *step, text },
        _ => TurnCase { script: 0, step: 0, text: &[] },
    }
}

pub fn turn_input(script: u8, step: u8, text: &[u8]) -> Vec<u8> {
    [&[script, step][..], text].concat()
}

/// Two byte strings: the first's length in two little-endian bytes, the
/// first, then the second. A length past the input takes all of it.
pub fn pair(data: &[u8]) -> (&[u8], &[u8]) {
    let [lo, hi, rest @ ..] = data else { return (&[], &[]) };
    let len = usize::from(u16::from_le_bytes([*lo, *hi]));
    rest.split_at(len.min(rest.len()))
}

/// The input [`pair`] reads as `(a, b)`; `a` is cut to what two bytes count.
pub fn pair_input(a: &[u8], b: &[u8]) -> Vec<u8> {
    let a = &a[..a.len().min(usize::from(u16::MAX))];
    let len = u16::try_from(a.len()).unwrap_or(u16::MAX).to_le_bytes();
    [&len[..], a, b].concat()
}

/// A proxy case: byte 0 the seed whose preamble and follow-up frame the
/// datagram, the rest the datagram.
pub fn proxy_case(data: &[u8]) -> (u8, &[u8]) {
    match data {
        [seed, datagram @ ..] => (*seed, datagram),
        [] => (0, &[]),
    }
}

pub fn proxy_input(seed: u8, datagram: &[u8]) -> Vec<u8> {
    [&[seed][..], datagram].concat()
}

/// An INVITE case: byte 0 the decision, byte 1 the script run over the
/// call it dials, the rest the caller's INVITE.
pub fn invite_case(data: &[u8]) -> (u8, u8, &[u8]) {
    let case = turn_case(data);
    (case.script, case.step, case.text)
}

pub fn invite_input(decision: u8, script: u8, invite: &[u8]) -> Vec<u8> {
    turn_input(decision, script, invite)
}

/// A proxy variant case: byte 0 the variant, byte 1 the seed of that
/// variant, the rest the datagram.
pub fn variant_case(data: &[u8]) -> (u8, u8, &[u8]) {
    let case = turn_case(data);
    (case.script, case.step, case.text)
}

pub fn variant_input(variant: u8, seed: u8, datagram: &[u8]) -> Vec<u8> {
    turn_input(variant, seed, datagram)
}

/// A probe case: byte 0 the status worker a answers the probe with, the
/// rest the header lines of its answer.
pub fn probe_case(data: &[u8]) -> (u8, &[u8]) {
    proxy_case(data)
}

pub fn probe_input(status: u8, lines: &[u8]) -> Vec<u8> {
    proxy_input(status, lines)
}

/// A multipart case: the boundary up to the first LF, the body after it.
pub fn multipart_case(data: &[u8]) -> (&[u8], &[u8]) {
    match data.iter().position(|&b| b == b'\n') {
        Some(at) => (&data[..at], &data[at + 1..]),
        None => (data, &[]),
    }
}

pub fn multipart_input(boundary: &[u8], body: &[u8]) -> Vec<u8> {
    [boundary, b"\n", body].concat()
}
