//! Security primitives for the proxy: the [`hmac`] routing-cookie key
//! provider (sign/verify the Record-Route stickiness MAC), and the key of
//! the To-tags of the proxy's own finals ([`own_final_tag`]).

pub mod hmac;
pub mod own_final_tag;
