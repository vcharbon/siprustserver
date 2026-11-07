//! A header's cardinality as a type (RFC 3261 §7.3.1): a single-valued header
//! reads as at most one value, a list header as its list, and neither has the
//! other's accessor.
//!
//! The single-valued headers are the ones [`SINGLE_VALUED`](super::SINGLE_VALUED)
//! states. Each value type declares its [`Lines`](HeaderValue::Lines) beside a
//! compile-time check against that table, so a type and the gate never
//! disagree.
//!
//! ```compile_fail,E0277
//! # use sip_message::{header, SipRequest};
//! fn every_from(msg: &SipRequest) {
//!     let _ = msg.list::<header::From>(); // From is single-valued
//! }
//! ```
//!
//! ```compile_fail,E0271
//! # use sip_message::{header, SipRequest};
//! fn one_via(msg: &SipRequest) {
//!     let _ = msg.header::<header::Via>(); // Via is a list
//! }
//! ```
//!
//! ```compile_fail,E0277
//! # use sip_message::{header, Method, draft::RequestDraft, header::Uri};
//! // A draft appends a line only to a list header; a single one is `set`.
//! let _ = RequestDraft::new(Method::Options, Uri::sip("example.com"))
//!     .push(header::MaxForwards::new(70));
//! ```
//!
//! ```
//! # use sip_message::{header, Method, draft::RequestDraft, header::Uri, SipRequest};
//! fn reads(msg: &SipRequest) {
//!     let _ = msg.header::<header::From>();
//!     let _ = msg.list::<header::Via>();
//!     let _ = msg.token_set::<header::Supported>();
//! }
//! let _ = RequestDraft::new(Method::Options, Uri::sip("example.com"))
//!     .set(header::MaxForwards::new(70))
//!     .push(header::Via::udp("192.0.2.1", 5060));
//! ```

use super::single_valued::{is_listed, Listed};
use super::value::{Folding, HeaderValue};

mod sealed {
    pub trait Sealed {}
}

/// How many lines of a header a message carries, and how a list lays its
/// values out.
pub trait Cardinality: sealed::Sealed + Send + Sync + 'static {
    /// The runtime echo of this cardinality.
    const FOLDING: Folding;
}

/// A list cardinality: any number of lines.
#[diagnostic::on_unimplemented(
    message = "a single-valued header has no list accessor: read it with `header`, write it \
               with `set`",
    label = "a single-valued header"
)]
pub trait Many: Cardinality {}

/// The cardinality markers.
pub mod lines {
    /// At most one line, holding one value (From, CSeq, Content-Type).
    #[derive(Debug, Clone, Copy)]
    pub enum Single {}
    /// Several values, which may share a line comma-separated (Via, Contact).
    #[derive(Debug, Clone, Copy)]
    pub enum Comma {}
    /// One value per line, a comma inside it being data (the credentials).
    #[derive(Debug, Clone, Copy)]
    pub enum Opaque {}
    /// One token set per line, the lines one set (Require, Supported).
    #[derive(Debug, Clone, Copy)]
    pub enum SetPerLine {}
}

macro_rules! cardinalities {
    ($($marker:ident => $folding:ident;)+) => {
        $(
            impl sealed::Sealed for lines::$marker {}
            impl Cardinality for lines::$marker {
                const FOLDING: Folding = Folding::$folding;
            }
        )+
    };
}

cardinalities! {
    Single => Single;
    Comma => Comma;
    Opaque => Opaque;
    SetPerLine => SetPerLine;
}

impl Many for lines::Comma {}
impl Many for lines::Opaque {}
impl Many for lines::SetPerLine {}

/// A single-valued header: a message carries at most one line of it, read
/// with `header`.
pub trait SingleHeader: HeaderValue<Lines = lines::Single> {}

impl<H: HeaderValue<Lines = lines::Single>> SingleHeader for H {}

/// A list header: a message may carry several lines of it, read with `list`
/// (or `first`, or `token_set` for a token set).
pub trait ListHeader: HeaderValue {}

impl<H: HeaderValue> ListHeader for H where H::Lines: Many {}

/// A list header whose lines are one token set, read whole with `token_set`.
pub trait TokenSet: HeaderValue<Lines = lines::SetPerLine> {
    /// The one set the lines of a message make.
    fn union(sets: impl IntoIterator<Item = Self>) -> Option<Self>;
}

/// Whether `L` is single exactly when the table lists `header`.
pub(crate) const fn agrees<L: Cardinality>(header: Listed) -> bool {
    matches!(L::FOLDING, Folding::Single) == is_listed(header)
}

/// The cardinality `$lines` declared for `$header`, held to the table at
/// compile time: one that disagrees does not build, the two array lengths
/// differing.
macro_rules! held_to_the_table {
    ($lines:ty, $header:expr) => {
        const _: [(); 1] = [(); $crate::header::cardinality::agrees::<$lines>($header) as usize];
    };
}

pub(crate) use held_to_the_table;
