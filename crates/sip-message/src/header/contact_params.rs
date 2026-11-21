//! The values of a Contact's own `contact-params` (RFC 3261 §20.10): the
//! `c-p-q` preference, a §25.1 `qvalue`, and the `c-p-expires` lifetime, a
//! `delta-seconds`. Every other contact-param is a `generic-param`, read by
//! [`super::Params`].

/// The preference a `q` contact-param's text states, when it is a `qvalue`:
/// `"0" [ "." 0*3DIGIT ]` or `"1" [ "." 0*3("0") ]`.
pub fn read_qvalue(text: &str) -> Option<f32> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let digits = fraction.len() <= 3 && fraction.bytes().all(|b| b.is_ascii_digit());
    let reads = match whole {
        "0" => digits,
        "1" => digits && fraction.bytes().all(|b| b == b'0'),
        _ => false,
    };
    reads.then(|| text.parse().ok()).flatten()
}

/// The `qvalue` text of the preference `q`, rounded to the three decimals the
/// grammar holds, with no trailing zero and no sign; `None` for a `q` outside
/// `0..=1`.
pub fn qvalue_text(q: f32) -> Option<String> {
    if !(0.0..=1.0).contains(&q) {
        return None;
    }
    let text = format!("{:.3}", q.abs());
    Some(text.trim_end_matches('0').trim_end_matches('.').to_string())
}

/// Whether `text` is a `delta-seconds` (`1*DIGIT`), the value `expires` takes
/// as a contact-param.
pub fn is_delta_seconds(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_qvalue_reads_between_zero_and_one_with_three_decimals_at_most() {
        assert_eq!(read_qvalue("0.5"), Some(0.5));
        assert_eq!(read_qvalue("0"), Some(0.0));
        assert_eq!(read_qvalue("0."), Some(0.0));
        assert_eq!(read_qvalue("0.125"), Some(0.125));
        assert_eq!(read_qvalue("1"), Some(1.0));
        assert_eq!(read_qvalue("1.000"), Some(1.0));
    }

    #[test]
    fn text_outside_the_qvalue_grammar_states_no_preference() {
        for text in ["", "1.5", "1.001", "0.1234", "2", "-0.5", ".5", "0.5x", "00.5", "abc", "0,5"]
        {
            assert_eq!(read_qvalue(text), None, "{text:?}");
        }
    }

    #[test]
    fn a_preference_renders_as_its_shortest_qvalue() {
        assert_eq!(qvalue_text(1.0).as_deref(), Some("1"));
        assert_eq!(qvalue_text(0.0).as_deref(), Some("0"));
        assert_eq!(qvalue_text(-0.0).as_deref(), Some("0"));
        assert_eq!(qvalue_text(0.5).as_deref(), Some("0.5"));
        assert_eq!(qvalue_text(0.1).as_deref(), Some("0.1"));
        assert_eq!(qvalue_text(0.1234).as_deref(), Some("0.123"));
        for q in [1.5, -0.1, f32::NAN] {
            assert_eq!(qvalue_text(q), None, "{q}");
        }
    }

    #[test]
    fn delta_seconds_is_digits_only() {
        assert!(is_delta_seconds("60"));
        assert!(is_delta_seconds("0"));
        for text in ["", "-1", "6 0", "60s", "1.5"] {
            assert!(!is_delta_seconds(text), "{text:?}");
        }
    }
}
