//! Security primitives for the proxy — currently the [`hmac`] routing-cookie
//! key provider (sign/verify the Record-Route stickiness MAC).

pub mod hmac;
