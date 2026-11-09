//! Every value of a header read through the accessor its cardinality gives,
//! for a test generic over every header type: `header` for a single-valued
//! one, `list` for a list; and the reading only that cardinality has —
//! `first` for a list of values in order, `token_set` for a token set.

use sip_message::header::{lines, HeaderValue, SingleHeader};
use sip_message::{SipMessage, SipParseError};

/// The reads a cardinality gives `H`.
pub trait ReadLines<H: HeaderValue> {
    /// Every value of `H`.
    fn values(msg: &SipMessage) -> Result<Vec<H>, SipParseError>;
    /// The reading only this cardinality has, as its `Debug` text.
    fn own_reading(msg: &SipMessage) -> String;
}

impl<H: SingleHeader> ReadLines<H> for lines::Single {
    fn values(msg: &SipMessage) -> Result<Vec<H>, SipParseError> {
        msg.header::<H>().transpose().map(|one| one.into_iter().collect())
    }

    fn own_reading(msg: &SipMessage) -> String {
        format!("{:?}", msg.header::<H>())
    }
}

macro_rules! read_in_order {
    ($($marker:ident),+) => {
        $(impl<H: HeaderValue<Lines = lines::$marker>> ReadLines<H> for lines::$marker {
            fn values(msg: &SipMessage) -> Result<Vec<H>, SipParseError> {
                msg.list::<H>()
            }

            fn own_reading(msg: &SipMessage) -> String {
                format!("{:?}", msg.first::<H>())
            }
        })+
    };
}

read_in_order!(Comma, Opaque);

impl<H: sip_message::header::TokenSet> ReadLines<H> for lines::SetPerLine {
    fn values(msg: &SipMessage) -> Result<Vec<H>, SipParseError> {
        msg.list::<H>()
    }

    fn own_reading(msg: &SipMessage) -> String {
        format!("{:?}", msg.token_set::<H>())
    }
}

/// Every value of `H` `msg` carries.
pub fn values<H: HeaderValue>(msg: &SipMessage) -> Result<Vec<H>, SipParseError>
where
    H::Lines: ReadLines<H>,
{
    <H::Lines as ReadLines<H>>::values(msg)
}

/// The reading only `H`'s cardinality has, as its `Debug` text.
pub fn own_reading<H: HeaderValue>(msg: &SipMessage) -> String
where
    H::Lines: ReadLines<H>,
{
    <H::Lines as ReadLines<H>>::own_reading(msg)
}
