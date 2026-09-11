//! Capture-byte acquisition: a path becomes the capture image, whatever
//! wrappers it arrived in. Archived corpora are inconsistently packaged —
//! gzipped, tarred, or a tar misnamed `.gz` — so wrappers are detected from
//! the bytes and peeled in a bounded loop rather than trusted from the file
//! extension.
//!
//! Multi-member gzip is handled: archived corpora are regularly produced by
//! concatenating per-file gzip streams, and stopping at the first member
//! would silently truncate the capture.
//!
//! Unwrapping only. Deciding what the unwrapped bytes *are* is the container
//! dispatch in [`crate::read_capture_files`].

use std::fs::File;
use std::io::{BufReader, Cursor, Read};
use std::path::Path;

use flate2::read::MultiGzDecoder;

/// Gzip's fixed two-byte header.
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
/// POSIX tar (`ustar`) magic and its offset in the 512-byte header block.
const TAR_MAGIC: &[u8; 5] = b"ustar";
const TAR_MAGIC_OFF: usize = 257;
const TAR_BLOCK: usize = 512;

/// Wrappers peeled before giving up — `tar(gz(pcap))` is two, and anything
/// deeper is a packaging accident we refuse to chase.
const MAX_UNWRAP_DEPTH: u8 = 4;

/// Read `path` into memory and peel any gzip/tar wrapping.
pub fn load(path: &Path) -> std::io::Result<Vec<u8>> {
    let file = File::open(path)?;
    let hint = file.metadata().map(|m| m.len() as usize).unwrap_or(0);
    let mut bytes = read_maybe_gzipped(BufReader::new(file), hint)?;

    for _ in 0..MAX_UNWRAP_DEPTH {
        if !is_tar(&bytes) {
            break;
        }
        let member = tar_largest_regular_member(&bytes).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "tar archive holds no regular file member",
            )
        })?;
        let len = member.len();
        bytes = read_maybe_gzipped(Cursor::new(member), len)?;
    }
    Ok(bytes)
}

/// Drain `stream`, inflating it if its first two bytes are the gzip magic.
fn read_maybe_gzipped<R: Read>(mut stream: R, hint: usize) -> std::io::Result<Vec<u8>> {
    let mut magic = [0u8; 2];
    let mut got = 0usize;
    while got < magic.len() {
        match stream.read(&mut magic[got..])? {
            0 => break,
            n => got += n,
        }
    }
    let rewound = Cursor::new(magic[..got].to_vec()).chain(stream);

    let gzipped = magic[..got] == GZIP_MAGIC;
    // Captures in this corpus inflate to ~5x their gzip size; the hint only
    // saves reallocations, so an under- or over-estimate is harmless.
    let mut out = Vec::with_capacity(if gzipped { hint.saturating_mul(5) } else { hint });
    if gzipped {
        MultiGzDecoder::new(rewound).read_to_end(&mut out)?;
    } else {
        rewound.take(u64::MAX).read_to_end(&mut out)?;
    }
    Ok(out)
}

fn is_tar(bytes: &[u8]) -> bool {
    bytes.get(TAR_MAGIC_OFF..TAR_MAGIC_OFF + TAR_MAGIC.len()) == Some(&TAR_MAGIC[..])
}

/// The largest regular-file member of a tar archive. Corpus archives hold one
/// capture, but picking by size rather than position ignores any checksum or
/// metadata file packed alongside it.
fn tar_largest_regular_member(bytes: &[u8]) -> Option<&[u8]> {
    let mut off = 0usize;
    let mut best: Option<&[u8]> = None;
    while let Some(header) = bytes.get(off..off + TAR_BLOCK) {
        if header.iter().all(|b| *b == 0) {
            break; // end-of-archive marker
        }
        let size = tar_octal(header.get(124..136)?)?;
        let data_start = off + TAR_BLOCK;
        // '0' and NUL are the two spellings of "regular file"; every other
        // type flag (directory, link, PAX/GNU extension) carries no capture.
        let regular = matches!(header.get(156), Some(b'0') | Some(0));
        if regular {
            let data = bytes.get(data_start..data_start + size)?;
            if best.is_none_or(|b| data.len() > b.len()) {
                best = Some(data);
            }
        }
        off = data_start + size.div_ceil(TAR_BLOCK) * TAR_BLOCK;
    }
    best
}

/// A tar header's NUL/space-terminated octal number field.
fn tar_octal(field: &[u8]) -> Option<usize> {
    let digits = field.split(|b| *b == 0 || *b == b' ').find(|s| !s.is_empty())?;
    digits.iter().try_fold(0usize, |acc, b| {
        b.is_ascii_digit().then(|| acc.checked_mul(8)?.checked_add((b - b'0') as usize))?
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// One-member ustar archive (no end-of-archive padding needed to parse).
    fn tar_of(name: &str, data: &[u8]) -> Vec<u8> {
        let mut header = vec![0u8; TAR_BLOCK];
        header[..name.len()].copy_from_slice(name.as_bytes());
        let size = format!("{:011o}\0", data.len());
        header[124..124 + size.len()].copy_from_slice(size.as_bytes());
        header[156] = b'0'; // regular file
        header[TAR_MAGIC_OFF..TAR_MAGIC_OFF + 5].copy_from_slice(TAR_MAGIC);
        let mut out = header;
        out.extend_from_slice(data);
        out.resize(TAR_BLOCK + data.len().div_ceil(TAR_BLOCK) * TAR_BLOCK, 0);
        out.extend_from_slice(&[0u8; TAR_BLOCK * 2]); // end-of-archive
        out
    }

    fn gz_of(data: &[u8]) -> Vec<u8> {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    fn write_tmp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("sip-pcap-src-{}-{name}", std::process::id()));
        std::fs::write(&p, bytes).unwrap();
        p
    }

    /// Every packaging the archived corpus actually uses — bare, gzipped,
    /// tarred, and a tar holding a gzipped capture — yields the same bytes.
    #[test]
    fn every_wrapper_peels_to_the_same_capture() {
        let capture: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
        let cases: [(&str, Vec<u8>); 4] = [
            ("bare", capture.clone()),
            ("gz", gz_of(&capture)),
            ("tar", tar_of("capture_x.pcap", &capture)),
            ("targz", tar_of("capture_x.pcap.gz", &gz_of(&capture))),
        ];
        for (name, wrapped) in cases {
            let p = write_tmp(name, &wrapped);
            let got = load(&p).unwrap();
            std::fs::remove_file(&p).ok();
            assert_eq!(got, capture, "wrapper {name}");
        }
    }

    /// A tar packing metadata beside the capture yields the capture, not the
    /// first member.
    #[test]
    fn largest_regular_member_wins_over_position() {
        let capture: Vec<u8> = vec![9u8; 4096];
        let mut archive = tar_of("checksum.txt", b"deadbeef");
        archive.truncate(archive.len() - TAR_BLOCK * 2); // drop the end marker
        archive.extend_from_slice(&tar_of("capture_x.pcap", &capture));
        let p = write_tmp("multimember", &archive);
        let got = load(&p).unwrap();
        std::fs::remove_file(&p).ok();
        assert_eq!(got, capture);
    }

    /// An unwrapped payload is returned as-is: deciding whether it is a
    /// capture at all belongs to the container dispatch, not here.
    #[test]
    fn unrecognized_payload_is_returned_untouched() {
        let riff = b"RIFF\x24\x08\x00\x00WAVEfmt ".to_vec();
        let p = write_tmp("riff", &gz_of(&riff));
        let got = load(&p).unwrap();
        std::fs::remove_file(&p).ok();
        assert_eq!(got, riff);
    }
}
