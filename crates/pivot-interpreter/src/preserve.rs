//! Verifying a **preserved emission** (`PCAP2TEST_PIVOT_V3.md` §11):
//! `verbatim-emission` and `raw-order` state that the stored message rides as
//! the document holds it, and this is where the composed message is held to it.
//!
//! What the property can cover is exactly what the document stores. Tier-1 is
//! never stored (§8), so there is no absolute interleaving to restore: the
//! guarantee is over the stored block — every stated header present, in the
//! stated order, under the stated name casing, with the stated value — sitting
//! among the tier-1 lines the stack regenerates around it.
//!
//! The check runs on the COMPOSED message rather than on the document, because
//! a property nothing verifies is a property a later change can drop silently.

use sip_message::{SipHeader, TemplateHeader};

/// Hold `emitted` to the stored block `stated` names: each stated header occurs
/// in the emitted message, in order, spelled and valued as stated. Names the
/// first breach.
pub fn verify(stated: &[TemplateHeader], emitted: &[SipHeader]) -> Result<(), String> {
    let mut at = 0usize;
    for header in stated {
        let found = emitted[at..]
            .iter()
            .position(|h| h.name.as_str() == header.name && h.value.as_str() == header.value);
        match found {
            Some(offset) => at += offset + 1,
            None => {
                let mispositioned = emitted[..at.min(emitted.len())]
                    .iter()
                    .any(|h| h.name.as_str() == header.name && h.value.as_str() == header.value);
                let miscased = emitted.iter().any(|h| {
                    h.name.eq_ignore_ascii_case(&header.name) && h.value.as_str() == header.value
                });
                let why = if mispositioned {
                    "emitted out of the stated order"
                } else if miscased {
                    "emitted under a different name casing"
                } else {
                    "not emitted"
                };
                return Err(format!(
                    "{:?}: {why} (emitted block: {})",
                    header.name,
                    emitted.iter().map(|h| h.name.to_string()).collect::<Vec<_>>().join(", ")
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stated() -> Vec<TemplateHeader> {
        vec![
            TemplateHeader::frozen("p-asserted-identity", "<sip:+33000900004@lane.invalid>"),
            TemplateHeader::frozen("Supported", "timer, 100rel"),
            TemplateHeader::frozen("User-Agent", "Pivot/1"),
        ]
    }

    fn emitted(names: &[(&str, &str)]) -> Vec<SipHeader> {
        names.iter().map(|(n, v)| SipHeader::new(*n, *v)).collect()
    }

    /// The stored block among the tier-1 lines the stack regenerates: present,
    /// in order, cased as stated.
    #[test]
    fn a_stored_block_that_survived_tier_one_regeneration_verifies() {
        let out = emitted(&[
            ("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1"),
            ("From", "<sip:a@lane.invalid>;tag=t1"),
            ("CSeq", "1 INVITE"),
            ("p-asserted-identity", "<sip:+33000900004@lane.invalid>"),
            ("Supported", "timer, 100rel"),
            ("User-Agent", "Pivot/1"),
            ("Content-Length", "0"),
        ]);
        assert_eq!(verify(&stated(), &out), Ok(()));
    }

    #[test]
    fn a_reordered_block_names_the_header_that_moved() {
        let out = emitted(&[
            ("Supported", "timer, 100rel"),
            ("p-asserted-identity", "<sip:+33000900004@lane.invalid>"),
            ("User-Agent", "Pivot/1"),
        ]);
        let error = verify(&stated(), &out).unwrap_err();
        assert!(error.contains("p-asserted-identity"), "{error}");
        assert!(error.contains("out of the stated order"), "{error}");
    }

    #[test]
    fn a_canonicalized_name_is_a_casing_breach_and_says_so() {
        let out = emitted(&[
            ("P-Asserted-Identity", "<sip:+33000900004@lane.invalid>"),
            ("Supported", "timer, 100rel"),
            ("User-Agent", "Pivot/1"),
        ]);
        let error = verify(&stated(), &out).unwrap_err();
        assert!(error.contains("different name casing"), "{error}");
    }

    #[test]
    fn a_dropped_header_is_named_as_absent() {
        let out = emitted(&[
            ("p-asserted-identity", "<sip:+33000900004@lane.invalid>"),
            ("User-Agent", "Pivot/1"),
        ]);
        let error = verify(&stated(), &out).unwrap_err();
        assert!(error.contains("\"Supported\": not emitted"), "{error}");
    }

    /// Duplicate rows are positions, not a set: two `Accept` rows must both
    /// ride, in their stated order.
    #[test]
    fn duplicate_rows_each_take_their_own_position() {
        let stated = vec![
            TemplateHeader::frozen("Accept", "application/sdp"),
            TemplateHeader::frozen("Accept", "application/vnd.example.indata"),
        ];
        let both =
            emitted(&[("Accept", "application/sdp"), ("Accept", "application/vnd.example.indata")]);
        assert_eq!(verify(&stated, &both), Ok(()));
        let merged = emitted(&[("Accept", "application/sdp, application/vnd.example.indata")]);
        assert!(verify(&stated, &merged).is_err(), "a merged row is not two rows");
    }
}
