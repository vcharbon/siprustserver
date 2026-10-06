//! The reason phrase a final the stack sends wears when whoever decided it
//! states none: the RFC 3261 §21 phrase of its code; a code §21 does not name
//! wears the phrase of its class's `x00`, the code a UA treats it as (§8.1.3.2).

/// The RFC 3261 §21 phrase of `status`, else its class's `x00` phrase.
pub fn default_reason(status: u16) -> &'static str {
    match status {
        100 => "Trying",
        180 => "Ringing",
        181 => "Call Is Being Forwarded",
        182 => "Queued",
        183 => "Session Progress",
        200 => "OK",
        300 => "Multiple Choices",
        301 => "Moved Permanently",
        302 => "Moved Temporarily",
        305 => "Use Proxy",
        380 => "Alternative Service",
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        407 => "Proxy Authentication Required",
        408 => "Request Timeout",
        410 => "Gone",
        413 => "Request Entity Too Large",
        414 => "Request-URI Too Long",
        415 => "Unsupported Media Type",
        416 => "Unsupported URI Scheme",
        420 => "Bad Extension",
        421 => "Extension Required",
        423 => "Interval Too Brief",
        480 => "Temporarily Unavailable",
        481 => "Call/Transaction Does Not Exist",
        482 => "Loop Detected",
        483 => "Too Many Hops",
        484 => "Address Incomplete",
        485 => "Ambiguous",
        486 => "Busy Here",
        487 => "Request Terminated",
        488 => "Not Acceptable Here",
        491 => "Request Pending",
        493 => "Undecipherable",
        500 => "Server Internal Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Server Time-out",
        505 => "Version Not Supported",
        513 => "Message Too Large",
        600 => "Busy Everywhere",
        603 => "Decline",
        604 => "Does Not Exist Anywhere",
        606 => "Not Acceptable",
        _ => match status / 100 {
            1 => "Trying",
            2 => "OK",
            3 => "Multiple Choices",
            4 => "Bad Request",
            6 => "Busy Everywhere",
            _ => "Server Internal Error",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::default_reason;

    #[test]
    fn a_named_code_wears_its_section_21_phrase() {
        assert_eq!(default_reason(500), "Server Internal Error");
        assert_eq!(default_reason(501), "Not Implemented");
        assert_eq!(default_reason(487), "Request Terminated");
        assert_eq!(default_reason(603), "Decline");
        assert_eq!(default_reason(604), "Does Not Exist Anywhere");
        assert_eq!(default_reason(606), "Not Acceptable");
    }

    #[test]
    fn an_unnamed_code_wears_its_class_x00_phrase() {
        assert_eq!(default_reason(499), "Bad Request");
        assert_eq!(default_reason(599), "Server Internal Error");
        assert_eq!(default_reason(699), "Busy Everywhere");
    }
}
