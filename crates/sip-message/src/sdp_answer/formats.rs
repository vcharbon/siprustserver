//! Payload-format matching between two media lines (RFC 3264 §6.1): which
//! format of one line is the same payload format as a format of the other.

use std::collections::HashMap;

use crate::sdp_doc::{canonical_rtpmap, extract_fmtps, extract_rtpmaps, MediaLine};

/// The payload format a format of a media line names: its encoding, lower-cased
/// and canonical (RFC 4566 §6), and for AMR / AMR-WB the `octet-align` mode,
/// which makes two formats of one encoding distinct payload formats (RFC 4867).
#[derive(Clone, PartialEq, Eq, Hash)]
struct PayloadFormat {
    encoding: String,
    alignment: Option<String>,
}

/// The formats of one media line, each read once into the payload format it
/// names and indexed for matching, so pairing two lines costs O(formats +
/// attributes) whatever their counts.
pub(super) struct FormatTable<'a> {
    media: &'a MediaLine,
    payload: HashMap<&'a str, PayloadFormat>,
    /// The first format naming each payload format.
    first: HashMap<PayloadFormat, usize>,
    /// The first dynamic format naming each payload format.
    first_dynamic: HashMap<PayloadFormat, usize>,
    /// The first position of each static format.
    first_static: HashMap<&'a str, usize>,
}

impl<'a> FormatTable<'a> {
    pub(super) fn of(media: &'a MediaLine) -> Self {
        let rtpmaps: HashMap<String, String> = extract_rtpmaps(media).into_iter().collect();
        let fmtps: HashMap<String, String> = extract_fmtps(media).into_iter().collect();
        let mut table = Self {
            media,
            payload: HashMap::new(),
            first: HashMap::new(),
            first_dynamic: HashMap::new(),
            first_static: HashMap::new(),
        };
        for (i, format) in media.formats.iter().enumerate() {
            let format = format.as_str();
            if !is_dynamic(format) {
                table.first_static.entry(format).or_insert(i);
            }
            if !table.payload.contains_key(format) {
                let Some(named) = payload_format(format, media, &rtpmaps, &fmtps) else {
                    continue;
                };
                table.payload.insert(format, named);
            }
            let named = &table.payload[format];
            table.first.entry(named.clone()).or_insert(i);
            if is_dynamic(format) {
                table.first_dynamic.entry(named.clone()).or_insert(i);
            }
        }
        table
    }

    /// The encoding `format` names on this line, if any.
    pub(super) fn encoding(&self, format: &str) -> Option<&str> {
        self.payload.get(format).map(|p| p.encoding.as_str())
    }

    /// The format of this line that is the same payload format as `format` of
    /// `from`: a dynamic format on either side matches by payload format, two
    /// static formats by number.
    pub(super) fn counterpart(&self, format: &str, from: &FormatTable) -> Option<String> {
        let named = from.payload.get(format);
        let at = if is_dynamic(format) {
            named.and_then(|p| self.first.get(p)).copied()
        } else {
            let dynamic = named.and_then(|p| self.first_dynamic.get(p)).copied();
            let same = self.first_static.get(format).copied();
            dynamic.into_iter().chain(same).min()
        };
        at.map(|i| self.media.formats[i].clone())
    }
}

/// The payload format `format` of `media` names: its `rtpmap`, else the RFC
/// 3551 static assignment; `None` when neither names one.
fn payload_format(
    format: &str,
    media: &MediaLine,
    rtpmaps: &HashMap<String, String>,
    fmtps: &HashMap<String, String>,
) -> Option<PayloadFormat> {
    let encoding = rtpmaps
        .get(format)
        .map(|enc| canonical_rtpmap(&media.r#type, enc))
        .or_else(|| static_encoding(format).map(|enc| canonical_rtpmap(&media.r#type, enc)))?
        .to_ascii_lowercase();
    let alignment = amr_alignment(fmtps.get(format).map(String::as_str), &encoding);
    Some(PayloadFormat { encoding, alignment })
}

/// Whether `format` is a dynamic RTP payload type (RFC 3551 §3: 96–127).
pub(super) fn is_dynamic(format: &str) -> bool {
    format.parse::<u8>().is_ok_and(|pt| pt >= 96)
}

/// The `octet-align` mode the `fmtp` parameters of an AMR or AMR-WB format
/// state (RFC 4867 §8.1: absent is 0), or `None` for any other encoding.
fn amr_alignment(params: Option<&str>, enc: &str) -> Option<String> {
    let name = enc.split('/').next()?;
    if name != "amr" && name != "amr-wb" {
        return None;
    }
    let aligned = params.unwrap_or_default().split(';').find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim().eq_ignore_ascii_case("octet-align").then(|| v.trim().to_string())
    });
    Some(aligned.unwrap_or_else(|| "0".to_string()))
}

/// The encoding RFC 3551 §6 (tables 4 and 5) assigns a static payload type.
fn static_encoding(format: &str) -> Option<&'static str> {
    Some(match format.parse::<u8>().ok()? {
        0 => "PCMU/8000",
        3 => "GSM/8000",
        4 => "G723/8000",
        5 => "DVI4/8000",
        6 => "DVI4/16000",
        7 => "LPC/8000",
        8 => "PCMA/8000",
        9 => "G722/8000",
        10 => "L16/44100/2",
        11 => "L16/44100",
        12 => "QCELP/8000",
        13 => "CN/8000",
        14 => "MPA/90000",
        15 => "G728/8000",
        16 => "DVI4/11025",
        17 => "DVI4/22050",
        18 => "G729/8000",
        25 => "CelB/90000",
        26 => "JPEG/90000",
        28 => "nv/90000",
        31 => "H261/90000",
        32 => "MPV/90000",
        33 => "MP2T/90000",
        34 => "H263/90000",
        _ => return None,
    })
}
