//! Unit tests for sipfrag helpers. Port of `tests/sip/sipfrag-utils.test.ts`.

use sip_message::sipfrag::sipfrag_from_status;

#[test]
fn produces_exact_bytes_for_180_ringing() {
    let bytes = sipfrag_from_status(180, "Ringing");
    assert_eq!(String::from_utf8(bytes).unwrap(), "SIP/2.0 180 Ringing\r\n");
}

#[test]
fn produces_exact_bytes_for_200_ok() {
    assert_eq!(sipfrag_from_status(200, "OK"), b"SIP/2.0 200 OK\r\n".to_vec());
}

#[test]
fn preserves_multi_word_reason() {
    let bytes = sipfrag_from_status(486, "Busy Here");
    assert_eq!(String::from_utf8(bytes).unwrap(), "SIP/2.0 486 Busy Here\r\n");
}

#[test]
fn crlf_terminated() {
    let bytes = sipfrag_from_status(100, "Trying");
    assert_eq!(bytes[bytes.len() - 2], 0x0d);
    assert_eq!(bytes[bytes.len() - 1], 0x0a);
}
