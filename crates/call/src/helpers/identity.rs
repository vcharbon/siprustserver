//! The tags an in-dialog request carries ([`RequestTags`]).

/// The To-tag and From-tag an in-dialog request carries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RequestTags<'a> {
    pub to: Option<&'a str>,
    pub from: Option<&'a str>,
}

impl<'a> RequestTags<'a> {
    pub fn new(to: Option<&'a str>, from: Option<&'a str>) -> Self {
        RequestTags { to, from }
    }
}
