//! One session description as the family reads it, and the rows a finding quotes.

use sip_message::sdp_doc::{self, SdpDoc, SdpOrigin};

use crate::wire::Msg;

/// One session description as the view carried it.
pub(super) struct Doc {
    pub(super) doc: SdpDoc,
    /// Its `o=` line, absent where the description spells none §5.2 accepts.
    pub(super) origin: Option<SdpOrigin>,
}

impl Doc {
    /// The description a message carried, or `None` where the vantage carried
    /// no body bytes or the message declared no session description. A body
    /// with no `Content-Type` declares none (RFC 3261 §20.15): `wellformed`
    /// charges that once, and this family reads the round as it was declared.
    pub(super) fn of(msg: &Msg) -> Option<Doc> {
        let body = msg.sdp()?;
        let doc = sdp_doc::parse_sdp_body(body)?;
        Some(Doc { origin: sdp_doc::parse_origin(body), doc })
    }

    /// The `m=` lines as `<media>/<transport>` pairs — the stream table a
    /// finding quotes. Empty where the description holds no `m=` line.
    pub(super) fn stream_table(&self) -> Vec<String> {
        self.doc.media.iter().map(|m| format!("{}/{}", m.r#type, m.transport)).collect()
    }
}

/// One stream of one description as a report spells it: `<media> <port>
/// <proto>`, the port `?` where the m-line carried no readable one.
pub(super) fn stream_row(m: &sip_message::sdp_doc::MediaLine) -> String {
    let port = m.port.map(|p| p.to_string()).unwrap_or_else(|| "?".to_string());
    format!("{} {} {}", m.r#type, port, m.transport)
}

/// The transport plan a description states — where the media goes and what
/// rides it: the session `c=` line, then one row per stream as `<media> <port>
/// <transport> <formats>` with the stream's own `c=` where it states one. The
/// `o=` line is deliberately absent: a plan re-stated under another origin is
/// [`SdpOriginContinuity`](super::SdpOriginContinuity)'s occasion.
pub(super) fn media_plan(d: &Doc) -> Vec<String> {
    let mut plan = vec![format!("c={}", d.doc.c_line.as_deref().unwrap_or(""))];
    plan.extend(d.doc.media.iter().map(|m| {
        let mut row = stream_row(m);
        for token in &m.formats {
            row.push(' ');
            row.push_str(token);
        }
        if let Some(c) = &m.c_line {
            row.push_str(" c=");
            row.push_str(c);
        }
        row
    }));
    plan
}
