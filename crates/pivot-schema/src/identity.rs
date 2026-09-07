//! The `identities` registry (`PCAP2TEST_PIVOT_V3.md` §8.5): every number or
//! domain the document names, declared once and referenced by NAME everywhere
//! else.
//!
//! The registry names and classifies; it never binds. Which real number a name
//! becomes is a per-lane translation the driver performs — a mock lane's
//! allocation and a provisioned backend's leased number are different numbers
//! for one entry — so a document that embedded the value could only ever be
//! replayed on the lane it was cut from.
//!
//! Every token here is OPEN. `kind`, the dial `forms` and the `catalog` class
//! name a deployment's numbering plan and number catalog, which this crate does
//! not model and must not enumerate; the shape is the contract, the vocabulary
//! belongs to whoever provisions the numbers.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One party's identity: what the plan made of it, under the name the rest of
/// the document refers to it by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    /// Unique within the document, and the only handle onto this identity:
    /// `attempts[].callee`, `actors[].identity` and `${num:<name>:<form>}` all
    /// name it. A generated document synthesizes the name from the party's
    /// position (`caller`, `called-0-1`), so one encoding covers captured and
    /// authored documents alike.
    pub name: String,
    /// Open plan token classifying the identity (e.g. a caller class, a
    /// catalog class, or `unknown` when the plan does not recognize it).
    pub kind: String,
    /// On a captured document: the anonymized value the capture carried, and
    /// the provenance that discipline is about. On an authored one: optional
    /// and purely informative — the lane binds the NAME, never this — so
    /// prefer omitting it and letting `${num:…}` compose the value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed: Option<String>,
    /// Open dial-form tokens the plan resolved this value into, and the forms
    /// `${num:<name>:<form>}` may ask for. Omitted when the plan resolved none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forms: Vec<String>,
    /// What the number catalog says about the value, when a catalog answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog: Option<CatalogEntry>,
}

impl Identity {
    /// Whether this identity can be dialed in `form` — what a `${num:…}`
    /// accessor needs of it, since a lane can only bind a form the plan
    /// resolved.
    pub fn has_form(&self, form: &str) -> bool {
        self.forms.iter().any(|f| f == form)
    }
}

/// The catalog's own classification of a provisioned number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CatalogEntry {
    /// Open catalog class token. Deployment vocabulary — never interpreted
    /// here.
    pub class: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_identity_answers_for_the_forms_the_plan_resolved() {
        let identity: Identity = serde_json::from_str(
            r#"{"name":"called-0-0","kind":"site","observed":"+33000900004","forms":["e164","trunk-composed"]}"#,
        )
        .unwrap();
        assert!(identity.has_form("e164"));
        assert!(!identity.has_form("private"));
    }

    #[test]
    fn an_authored_identity_may_state_an_observed_value_or_none() {
        let symbolic: Identity =
            serde_json::from_str(r#"{"name":"transferee","kind":"site","forms":["e164"]}"#).unwrap();
        assert_eq!(symbolic.observed, None);
        assert!(!serde_json::to_string(&symbolic).unwrap().contains("observed"));
        // Carrying one is legal and unlinted: the discipline binds captures.
        let stated: Identity = serde_json::from_str(
            r#"{"name":"transferee","kind":"site","observed":"+33000900006","forms":["e164"]}"#,
        )
        .unwrap();
        assert_eq!(stated.observed.as_deref(), Some("+33000900006"));
    }

    #[test]
    fn an_unknown_identity_field_is_refused_rather_than_ignored() {
        assert!(serde_json::from_str::<Identity>(r#"{"name":"x","kind":"site","number":"1"}"#).is_err());
    }
}
