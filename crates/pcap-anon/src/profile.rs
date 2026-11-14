//! Network profile: what the anonymiser knows of the network that captured a
//! document. A profile names the SUT's signalling sockets (IP list / CIDR
//! subnets, matched by [`crate::cut::Cut`]), whose addresses the anonymiser
//! keeps so the cut still finds the SUT, the texts the SUT itself writes,
//! kept as written, and the body types the network adds, each with how its
//! content is read ([`BodyTypes`]). The caller states it
//! (`pcap-anon --profile`). Format (JSON):
//!
//! ```json
//! { "sut": ["192.0.2.0/24", "198.51.100.7"], "kept_texts": ["Exampleswitch"],
//!   "bodies": [{ "media_type": "application/vnd.example.order", "read": "digit-runs",
//!                "vocabulary": ["order", "transfer"] }] }
//! ```

use serde::Deserialize;

use crate::cut::Cut;
use crate::fake::address::Scope;
use crate::identities::body_types::BodyType;
use crate::identities::BodyTypes;

/// A loaded network profile (see module doc for the on-disk format).
#[derive(Debug)]
pub struct Profile {
    /// The SUT socket set, reusing the cut matcher.
    sut: Cut,
    /// The strings the SUT itself writes (an SDP session name, its product
    /// string): vocabulary the anonymiser keeps as written and verify accepts.
    pub kept_texts: Vec<String>,
    /// The body types the network adds.
    pub bodies: BodyTypes,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileFile {
    sut: Vec<String>,
    #[serde(default)]
    kept_texts: Vec<String>,
    #[serde(default)]
    bodies: Vec<BodyType>,
}

impl Profile {
    /// Parse + validate a profile document; every violation is a loud error.
    pub fn from_json_str(json: &str) -> Result<Profile, String> {
        let pf: ProfileFile =
            serde_json::from_str(json).map_err(|e| format!("profile config is not valid: {e}"))?;
        let sut = Cut::from_json_str(&format!(
            "{{\"sut\":[{}]}}",
            pf.sut.iter().map(|s| format!("{s:?}")).collect::<Vec<_>>().join(",")
        ))?;
        let bodies = BodyTypes::new(pf.bodies).map_err(|e| format!("profile config: {e}"))?;
        Ok(Profile { sut, kept_texts: pf.kept_texts, bodies })
    }

    /// The anonymisation scope the profile states: its SUT addresses, its
    /// kept texts and its body types.
    pub fn scope(&self) -> Scope {
        Scope::default()
            .keeping(&self.sut)
            .keeping_texts(&self.kept_texts)
            .reading_bodies(&self.bodies)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scope keeps the profile's SUT addresses, whatever their range, and
    /// maps a public peer outside them.
    #[test]
    fn the_scope_keeps_the_sut_addresses_and_maps_a_public_peer() {
        let profile = Profile::from_json_str(r#"{"sut":["8.8.8.0/24","9.9.9.10"]}"#)
            .expect("the profile reads");
        let scope = profile.scope();
        assert!(!scope.maps_v4("8.8.8.9".parse().unwrap()));
        assert!(!scope.maps_v4("9.9.9.10".parse().unwrap()));
        assert!(scope.maps_v4("9.9.9.11".parse().unwrap()));
    }

    /// The body types a profile states reach its scope; a reading the
    /// anonymiser does not know is refused.
    #[test]
    fn the_scope_reads_the_body_types_the_profile_states() {
        let profile = Profile::from_json_str(
            r#"{"sut":[],"bodies":[{"media_type":"application/vnd.example.order","read":"digit-runs"}]}"#,
        )
        .expect("the profile reads");
        let reading = profile.scope().bodies().reading("application/vnd.example.order");
        assert_eq!(reading, Some(crate::identities::body_types::BodyReading::DigitRuns));
        let refused = Profile::from_json_str(
            r#"{"sut":[],"bodies":[{"media_type":"application/vnd.example.order","read":"ocr"}]}"#,
        )
        .unwrap_err();
        assert!(refused.contains("ocr"), "{refused}");
    }

    /// A body row's vocabulary is read under its own key: a key the profile
    /// does not know — a misspelt one — is refused, never left out.
    #[test]
    fn a_body_row_with_a_key_the_profile_does_not_know_is_refused() {
        let row = |key: &str| {
            format!(
                r#"{{"sut":[],"bodies":[{{"media_type":"application/vnd.example.order","read":"digit-runs","{key}":["inf"]}}]}}"#
            )
        };
        let profile = Profile::from_json_str(&row("vocabulary")).expect("the profile reads");
        assert!(profile.scope().bodies().lists("application/vnd.example.order", "inf"));
        let refused = Profile::from_json_str(&row("vocabularly")).unwrap_err();
        assert!(refused.contains("vocabularly"), "{refused}");
    }

    #[test]
    fn a_sut_entry_that_is_no_address_is_refused() {
        let refused = Profile::from_json_str(r#"{"sut":["not-an-ip"]}"#).unwrap_err();
        assert!(refused.contains("not-an-ip"), "{refused}");
    }
}
