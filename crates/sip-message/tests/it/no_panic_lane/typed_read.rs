//! Every value of a header read through the accessor its cardinality gives,
//! for a test generic over every header type: `header` for a single-valued
//! one, `list` for a list.

use sip_message::header::{lines, HeaderValue, SingleHeader};
use sip_message::{SipMessage, SipParseError};

/// The read a cardinality gives `H`.
pub trait ReadLines<H: HeaderValue> {
    fn values(msg: &SipMessage) -> Result<Vec<H>, SipParseError>;
}

impl<H: SingleHeader> ReadLines<H> for lines::Single {
    fn values(msg: &SipMessage) -> Result<Vec<H>, SipParseError> {
        msg.header::<H>().transpose().map(|one| one.into_iter().collect())
    }
}

macro_rules! read_as_list {
    ($($marker:ident),+) => {
        $(impl<H: HeaderValue<Lines = lines::$marker>> ReadLines<H> for lines::$marker {
            fn values(msg: &SipMessage) -> Result<Vec<H>, SipParseError> {
                msg.list::<H>()
            }
        })+
    };
}

read_as_list!(Comma, Opaque, SetPerLine);

/// Every value of `H` `msg` carries.
pub fn values<H: HeaderValue>(msg: &SipMessage) -> Result<Vec<H>, SipParseError>
where
    H::Lines: ReadLines<H>,
{
    <H::Lines as ReadLines<H>>::values(msg)
}
