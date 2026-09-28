//! One party's session across a dialog (RFC 3264 §8): what it has stated so
//! far, a description from another author restated as the next version of
//! that session, and the way back to the author's own stream order.
//!
//! A back-to-back UA is one party on each dialog, whoever authored the
//! description it forwards. When the author changes mid-dialog, the far party
//! must still see one session: the `o=` identity it holds with the version one
//! up (§8), every m-line it was shown kept at its position (§8, §8.2), each of
//! the author's streams in the slot of the same media type where one is live.
//! What the far party then describes comes back to the author in the author's
//! own stream order, without the slots the author never described (§6: an
//! answer carries exactly the offer's m-lines).
//!
//! Pure and deterministic: no clocks, no randomness, no I/O.

use crate::sdp_doc::parse_origin;

/// What one party has stated about its session on a dialog: the value of the
/// `o=` line it last sent and the value of each of its `m=` lines, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatedSession {
    /// The `o=` line's value — everything after `o=`.
    pub origin: String,
    /// Each `m=` line's value, in document order.
    pub media: Vec<String>,
}

impl StatedSession {
    /// The session `sdp` states, or `None` where it is not a session
    /// description carrying a readable `o=` line.
    pub fn of(sdp: &[u8]) -> Option<Self> {
        let origin = parse_origin(sdp)?;
        let text = String::from_utf8_lossy(sdp);
        let media = Sections::of(&text).media.iter().map(|s| s.value().to_string()).collect();
        Some(Self { origin: origin.raw_origin_line[2..].to_string(), media })
    }
}

/// A description restated under a session another author opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Restated {
    /// The description as it leaves.
    pub sdp: Vec<u8>,
    /// For each of its m-lines, the position of the author's stream it
    /// carries; `None` for a stream kept rejected in place of one the author
    /// does not describe.
    pub slots: Vec<Option<u32>>,
}

/// `sdp` restated as the next description of the session `stated` names
/// (RFC 3264 §8), or `None` where it leaves as written: it carries the stated
/// sess-id — its author continues that session itself — or either side has no
/// readable `o=` line.
///
/// Session-level lines are the author's with the stated `o=` identity, the
/// version one above the stated one (every restatement is a new version, an
/// unchanged description included; §8 constrains only a version that does not
/// move). Each of the author's streams, in order, takes the first stated slot
/// not yet taken: one of its media type with a non-zero port, else one of its
/// media type, else a rejected one (§8.3.3 lets a slot change type), else a new
/// slot appended (§8.1). A stated slot left over keeps its position rejected:
/// its `m=` line with port 0, no attribute (§8.2), and the author's first
/// stream-level `c=` where the description states no session-level one
/// (RFC 4566 §5.7).
pub fn restate_session(sdp: &[u8], stated: &StatedSession) -> Option<Restated> {
    let current = parse_origin(sdp)?;
    let previous = parse_origin(format!("v=0\r\no={}\r\n", stated.origin).as_bytes())?;
    if current.session_id == previous.session_id {
        return None;
    }
    let text = String::from_utf8_lossy(sdp);
    let authored = Sections::of(&text);
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };

    let mut slots: Vec<Option<u32>> = vec![None; stated.media.len()];
    for (j, stream) in authored.media.iter().enumerate() {
        let free = |i: &usize| slots[*i].is_none();
        let kind = stream.media_type();
        let slot = (0..stated.media.len())
            .filter(free)
            .find(|&i| media_type(&stated.media[i]) == kind && port(&stated.media[i]) != Some(0))
            .or_else(|| {
                (0..stated.media.len()).filter(free).find(|&i| media_type(&stated.media[i]) == kind)
            })
            .or_else(|| {
                (0..stated.media.len()).filter(free).find(|&i| port(&stated.media[i]) == Some(0))
            });
        match slot {
            Some(i) => slots[i] = Some(j as u32),
            None => slots.push(Some(j as u32)),
        }
    }

    let mut out = with_line_end(
        &authored.session.replacen(&current.raw_origin_line, &previous.next_version_line(), 1),
        eol,
    );
    let stub_c = (!authored.has_session_c())
        .then(|| authored.media.iter().find_map(|s| s.c_line()))
        .flatten();
    for (i, slot) in slots.iter().enumerate() {
        match slot {
            Some(j) => out.push_str(&with_line_end(authored.media[*j as usize].text, eol)),
            None => {
                out.push_str(&format!("m={}{eol}", rejected(&stated.media[i])));
                if let Some(c) = stub_c {
                    out.push_str(&format!("{c}{eol}"));
                }
            }
        }
    }
    if !text.ends_with('\n') && slots.last().is_some_and(Option::is_some) {
        out.truncate(out.len() - eol.len());
    }
    Some(Restated { sdp: out.into_bytes(), slots })
}

/// `sdp`, described by the far party of a dialog whose last restatement
/// carried `slots`, in the stream order of the author of that restatement, or
/// `None` where it already is: the stream in the slot carrying the author's
/// stream `j` becomes its `j`-th, a rejected slot the author never described is
/// taken out, and any other stream (live in a slot the author never described,
/// or past the last slot) follows in order as a stream the far party adds.
pub fn in_author_order(sdp: &[u8], slots: &[Option<u32>]) -> Option<Vec<u8>> {
    let text = String::from_utf8_lossy(sdp);
    let described = Sections::of(&text);
    let mut ordered: Vec<(u32, &Section<'_>)> = Vec::new();
    let mut added: Vec<&Section<'_>> = Vec::new();
    for (i, stream) in described.media.iter().enumerate() {
        match slots.get(i).copied().flatten() {
            Some(j) => ordered.push((j, stream)),
            None if stream.port() == Some(0) => {}
            None => added.push(stream),
        }
    }
    ordered.sort_by_key(|(j, _)| *j);
    let kept: Vec<&Section<'_>> = ordered.into_iter().map(|(_, s)| s).chain(added).collect();
    let unchanged = kept.len() == described.media.len()
        && kept.iter().zip(&described.media).all(|(a, b)| std::ptr::eq(*a, b));
    if unchanged {
        return None;
    }
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = with_line_end(described.session, eol);
    for s in &kept {
        out.push_str(&with_line_end(s.text, eol));
    }
    if !text.ends_with('\n') {
        out.truncate(out.len().saturating_sub(eol.len()));
    }
    Some(out.into_bytes())
}

/// A description cut at its `m=` lines: the session-level text, then each
/// media section (its `m=` line and every line up to the next one), each piece
/// with its line endings as written.
struct Sections<'a> {
    session: &'a str,
    media: Vec<Section<'a>>,
}

struct Section<'a> {
    text: &'a str,
}

impl<'a> Sections<'a> {
    fn of(text: &'a str) -> Self {
        let mut starts: Vec<usize> = Vec::new();
        let mut offset = 0usize;
        for line in text.split_inclusive('\n') {
            if line.starts_with("m=") {
                starts.push(offset);
            }
            offset += line.len();
        }
        let session_end = starts.first().copied().unwrap_or(text.len());
        let media = starts
            .iter()
            .enumerate()
            .map(|(k, &at)| Section {
                text: &text[at..starts.get(k + 1).copied().unwrap_or(text.len())],
            })
            .collect();
        Self { session: &text[..session_end], media }
    }

    fn has_session_c(&self) -> bool {
        self.session.split('\n').any(|l| l.starts_with("c="))
    }
}

impl Section<'_> {
    /// The `m=` line's value.
    fn value(&self) -> &str {
        let line = self.text.split('\n').next().unwrap_or_default();
        line.strip_suffix('\r').unwrap_or(line).get(2..).unwrap_or_default()
    }

    fn media_type(&self) -> &str {
        media_type(self.value())
    }

    fn port(&self) -> Option<u32> {
        port(self.value())
    }

    /// The section's first `c=` line, as written.
    fn c_line(&self) -> Option<&str> {
        self.text
            .split('\n')
            .skip(1)
            .map(|l| l.strip_suffix('\r').unwrap_or(l))
            .find(|l| l.starts_with("c="))
    }
}

/// An `m=` value's media type.
fn media_type(m_value: &str) -> &str {
    m_value.split_whitespace().next().unwrap_or_default()
}

/// An `m=` value's port (the part before any `/<number of ports>`).
fn port(m_value: &str) -> Option<u32> {
    m_value.split_whitespace().nth(1)?.split('/').next()?.parse().ok()
}

/// An `m=` value with its port set to 0 — the stream rejected (RFC 3264 §6),
/// transport and formats as stated.
fn rejected(m_value: &str) -> String {
    let mut tokens = m_value.split_whitespace();
    let kind = tokens.next().unwrap_or_default();
    let _port = tokens.next();
    tokens.fold(format!("{kind} 0"), |mut out, t| {
        out.push(' ');
        out.push_str(t);
        out
    })
}

/// `piece` ending with `eol` — a last line written without one gets it.
fn with_line_end(piece: &str, eol: &str) -> String {
    let mut s = piece.to_string();
    if !s.is_empty() && !s.ends_with('\n') {
        s.push_str(eol);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELD: &[u8] = b"v=0\r\no=b2b 700 7 IN IP4 192.0.2.10\r\ns=-\r\nc=IN IP4 192.0.2.10\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=sendrecv\r\nm=video 20002 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n";
    const NEW_AUTHOR: &[u8] = b"v=0\r\no=carol 900 3 IN IP4 198.51.100.7\r\ns=carol\r\nc=IN IP4 198.51.100.7\r\nt=0 0\r\nm=audio 30000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

    fn stated(sdp: &[u8]) -> StatedSession {
        StatedSession::of(sdp).expect("a stated session")
    }

    fn text(b: Vec<u8>) -> String {
        String::from_utf8(b).unwrap()
    }

    #[test]
    fn a_stated_session_keeps_the_origin_and_each_m_line() {
        let s = stated(HELD);
        assert_eq!(s.origin, "b2b 700 7 IN IP4 192.0.2.10");
        assert_eq!(s.media, vec!["audio 20000 RTP/AVP 0", "video 20002 RTP/AVP 96"]);
        assert!(StatedSession::of(b"v=0\r\ns=-\r\n").is_none(), "no o= line");
        assert!(StatedSession::of(b"").is_none());
    }

    /// RFC 3264 §8: the identity stays, the version rises by one, the
    /// session-level lines are the new author's; the video stream the author
    /// does not describe keeps its position, rejected (§8.2).
    #[test]
    fn a_new_author_is_restated_under_the_held_session() {
        let out = restate_session(NEW_AUTHOR, &stated(HELD)).expect("another session");
        assert_eq!(
            text(out.sdp),
            "v=0\r\no=b2b 700 8 IN IP4 192.0.2.10\r\ns=carol\r\nc=IN IP4 198.51.100.7\r\nt=0 0\r\nm=audio 30000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\nm=video 0 RTP/AVP 96\r\n"
        );
        assert_eq!(out.slots, vec![Some(0), None]);
    }

    /// The author's audio goes to the dialog's live audio slot, wherever it
    /// sits; the slot before it stays where it is, rejected.
    #[test]
    fn a_stream_takes_the_live_slot_of_its_media_type() {
        let held = b"v=0\r\no=b2b 700 7 IN IP4 192.0.2.10\r\ns=-\r\nc=IN IP4 192.0.2.10\r\nt=0 0\r\nm=video 20002 RTP/AVP 96\r\nm=audio 0 RTP/AVP 0\r\nm=audio 20000 RTP/AVP 0\r\n";
        let out = restate_session(NEW_AUTHOR, &stated(held)).unwrap();
        assert_eq!(out.slots, vec![None, None, Some(0)]);
        assert!(text(out.sdp).ends_with(
            "t=0 0\r\nm=video 0 RTP/AVP 96\r\nm=audio 0 RTP/AVP 0\r\nm=audio 30000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n"
        ));
    }

    /// A second stream of the author's takes a rejected slot of its type,
    /// then any rejected slot (§8.3.3), then a new one (§8.1).
    #[test]
    fn further_streams_reuse_rejected_slots_then_append() {
        let held = b"v=0\r\no=b2b 700 7 IN IP4 192.0.2.10\r\ns=-\r\nc=IN IP4 192.0.2.10\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\nm=image 0 udptl t38\r\n";
        let two = b"v=0\r\no=carol 900 3 IN IP4 198.51.100.7\r\ns=-\r\nc=IN IP4 198.51.100.7\r\nt=0 0\r\nm=audio 30000 RTP/AVP 8\r\nm=audio 30002 RTP/AVP 8\r\nm=video 30004 RTP/AVP 96\r\n";
        let out = restate_session(two, &stated(held)).unwrap();
        assert_eq!(out.slots, vec![Some(0), Some(1), Some(2)]);
        assert!(text(out.sdp).ends_with(
            "m=audio 30000 RTP/AVP 8\r\nm=audio 30002 RTP/AVP 8\r\nm=video 30004 RTP/AVP 96\r\n"
        ));
    }

    /// An unchanged description from the new author is still a new version.
    #[test]
    fn every_restatement_is_a_new_version() {
        let first = restate_session(NEW_AUTHOR, &stated(HELD)).unwrap();
        let again = restate_session(NEW_AUTHOR, &stated(&first.sdp)).unwrap();
        assert!(text(again.sdp).contains("o=b2b 700 9 IN IP4 192.0.2.10\r\n"));
        assert_eq!(again.slots, vec![Some(0), None]);
    }

    /// The same sess-id is the same session, whatever else of `o=` moved: the
    /// author continues it itself and the description leaves as written.
    #[test]
    fn the_stated_session_id_leaves_the_description_as_written() {
        let same =
            b"v=0\r\no=other 700 12 IN IP4 203.0.113.1\r\ns=-\r\nt=0 0\r\nm=audio 5 RTP/AVP 0\r\n";
        assert_eq!(restate_session(same, &stated(HELD)), None);
        assert_eq!(restate_session(b"v=0\r\ns=-\r\n", &stated(HELD)), None, "no o= to restate");
    }

    /// A rejected slot needs a connection address when the description states
    /// none at session level (RFC 4566 §5.7): it takes the author's first.
    #[test]
    fn a_rejected_slot_takes_a_connection_line_where_the_session_has_none() {
        let media_c = b"v=0\r\no=carol 900 3 IN IP4 198.51.100.7\r\ns=-\r\nt=0 0\r\nm=audio 30000 RTP/AVP 8\r\nc=IN IP4 198.51.100.7\r\n";
        let out = restate_session(media_c, &stated(HELD)).unwrap();
        assert!(text(out.sdp).ends_with("m=video 0 RTP/AVP 96\r\nc=IN IP4 198.51.100.7\r\n"));
    }

    #[test]
    fn a_bare_lf_description_keeps_its_line_ending() {
        let lf = b"v=0\no=carol 900 3 IN IP4 198.51.100.7\ns=-\nt=0 0\nm=audio 30000 RTP/AVP 8";
        let out = restate_session(lf, &stated(HELD)).unwrap();
        assert_eq!(
            text(out.sdp),
            "v=0\no=b2b 700 8 IN IP4 192.0.2.10\ns=-\nt=0 0\nm=audio 30000 RTP/AVP 8\nm=video 0 RTP/AVP 96\n"
        );
    }

    /// RFC 3264 §6: the answer to the restated offer comes back to its author
    /// with exactly the author's m-lines, in the author's order.
    #[test]
    fn the_far_answer_returns_in_the_author_order() {
        let answer = b"v=0\r\no=a 1 2 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=video 0 RTP/AVP 96\r\nm=audio 4000 RTP/AVP 8\r\na=sendrecv\r\n";
        assert_eq!(
            text(in_author_order(answer, &[None, Some(0)]).unwrap()),
            "v=0\r\no=a 1 2 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 8\r\na=sendrecv\r\n"
        );
        assert!(text(in_author_order(answer, &[Some(1), Some(0)]).unwrap()).ends_with(
            "t=0 0\r\nm=audio 4000 RTP/AVP 8\r\na=sendrecv\r\nm=video 0 RTP/AVP 96\r\n"
        ));
        assert_eq!(in_author_order(answer, &[Some(0), Some(1)]), None, "already in order");
    }

    /// A stream the far party brings live in a slot the author never
    /// described, or past the last slot, is a stream it adds: kept, after the
    /// author's.
    #[test]
    fn a_stream_the_far_party_adds_follows_the_authors() {
        let offer = b"v=0\r\no=a 1 3 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=video 5000 RTP/AVP 96\r\nm=audio 4000 RTP/AVP 8\r\n";
        assert_eq!(
            text(in_author_order(offer, &[None, Some(0)]).unwrap()),
            "v=0\r\no=a 1 3 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 8\r\nm=video 5000 RTP/AVP 96\r\n"
        );
    }
}
