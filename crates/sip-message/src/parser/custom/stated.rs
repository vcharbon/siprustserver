//! The mandatory values a stack-built message states as typed values.
//!
//! A draft that pushed a typed From, To, CSeq, Via or Contact
//! already holds the value its rendered line reads back as (`parse(render(v))
//! == v`), so assembling the frozen message takes that value instead of
//! parsing its own render again. Only the lenient extraction of a message the
//! stack built reads them; a line the draft holds as bytes is read from its
//! text as before.

use crate::draft::Entry;
use crate::error::SipParseError;
use crate::header::{self, HeaderId, HeaderValue, Via};

use super::top_via;

/// The typed values of a message's mandatory headers: a single-valued header
/// by its first line, a multi-valued one only when every line of it is typed.
#[derive(Debug, Default, Clone, Copy)]
pub struct Stated<'a> {
    pub from: Option<&'a header::From>,
    pub to: Option<&'a header::To>,
    pub cseq: Option<&'a header::CSeq>,
    entries: &'a [Entry],
}

impl<'a> Stated<'a> {
    /// What `entries` state.
    pub(crate) fn of(entries: &'a [Entry]) -> Self {
        Self {
            from: first_typed(entries, HeaderId::From),
            to: first_typed(entries, HeaderId::To),
            cseq: first_typed(entries, HeaderId::CSeq),
            entries,
        }
    }

    /// The stated Via entries as the lenient extraction holds them, `None`
    /// when a Via line is not typed: a port out of range or an empty branch
    /// refuses the message exactly as on its text.
    pub(super) fn vias(&self) -> Option<Result<Vec<Via>, SipParseError>> {
        let typed: Vec<&Via> = all_typed(self.entries, HeaderId::Via)?;
        let mut out = Vec::with_capacity(typed.len());
        for via in typed {
            let top = out.is_empty();
            if let Some(p) = via.port().filter(|&p| top_via::port_out_of_range(u64::from(p), top)) {
                return Some(Err(SipParseError::new(format!("Via port out of range: {p}"))));
            }
            if via.branch() == Some("") {
                return Some(Err(SipParseError::new("Empty Via branch parameter")));
            }
            out.push(via.clone());
        }
        Some(Ok(out))
    }

    /// The stated Contact entries, `None` when a Contact line is not typed.
    pub(super) fn contacts(&self) -> Option<Vec<header::Contact>> {
        let typed: Vec<&header::Contact> = all_typed(self.entries, HeaderId::Contact)?;
        Some(typed.into_iter().cloned().collect())
    }
}

fn lines_of(entries: &[Entry], id: HeaderId) -> impl Iterator<Item = &Entry> {
    entries.iter().filter(move |e| e.name().id() == Some(id))
}

fn first_typed<H: HeaderValue>(entries: &[Entry], id: HeaderId) -> Option<&H> {
    lines_of(entries, id).next()?.typed_ref::<H>()
}

/// Every line of `id` as an `H`, or `None` when one is not typed or there is
/// none.
fn all_typed<H: HeaderValue>(entries: &[Entry], id: HeaderId) -> Option<Vec<&H>> {
    let typed: Option<Vec<&H>> = lines_of(entries, id).map(Entry::typed_ref::<H>).collect();
    typed.filter(|values| !values.is_empty())
}
