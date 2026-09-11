//! The lane's **identity binding** (`PCAP2TEST_PIVOT_V3.md` §4.3): which real
//! number each registered identity name becomes, per dial form.
//!
//! The registry NAMES and classifies; it never binds. The driver compiles the
//! binding for its lane — a mock's allocation and a provisioned backend's leased
//! number are different numbers for one entry — and hands it to the run. The
//! interpreter substitutes and nothing more: it never reads a number out of the
//! document, and it never invents one for a name the lane did not bind.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Why a `${num:…}` cannot be answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingError {
    /// The lane bound no number for this identity.
    UnboundIdentity { name: String },
    /// The lane bound the identity, but not in the form the document asks for.
    UnboundForm { name: String, form: String, bound: Vec<String> },
}

impl std::fmt::Display for BindingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BindingError::UnboundIdentity { name } => {
                write!(f, "the lane bound no number for identity {name:?}")
            }
            BindingError::UnboundForm { name, form, bound } => {
                write!(f, "the lane bound identity {name:?} in {bound:?}, not in form {form:?}")
            }
        }
    }
}

impl std::error::Error for BindingError {}

/// Identity name → dial form → the number the lane allocated.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct IdentityBindings {
    by_name: BTreeMap<String, BTreeMap<String, String>>,
}

impl IdentityBindings {
    pub fn new() -> Self {
        IdentityBindings::default()
    }

    /// Bind `name` in `form` to `number`.
    pub fn bind(
        mut self,
        name: impl Into<String>,
        form: impl Into<String>,
        number: impl Into<String>,
    ) -> Self {
        self.by_name.entry(name.into()).or_default().insert(form.into(), number.into());
        self
    }

    /// The number for `name` in `form`, or why there is none. Never falls back
    /// to another form: two dial forms of one identity are different numbers on
    /// the wire, and substituting the wrong one silently mis-routes a call.
    pub fn resolve(&self, name: &str, form: &str) -> Result<&str, BindingError> {
        let forms = self
            .by_name
            .get(name)
            .ok_or_else(|| BindingError::UnboundIdentity { name: name.to_string() })?;
        forms.get(form).map(String::as_str).ok_or_else(|| BindingError::UnboundForm {
            name: name.to_string(),
            form: form.to_string(),
            bound: forms.keys().cloned().collect(),
        })
    }

    /// Whether the lane bound nothing at all.
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// Whether the binding says anything about `name`.
    pub fn holds(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.by_name.keys().map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_form_the_lane_did_not_bind_is_refused_by_name() {
        let bindings = IdentityBindings::new().bind("caller", "private", "0009001").bind(
            "called-0-0",
            "e164",
            "+33000900004",
        );
        assert_eq!(bindings.resolve("caller", "private").unwrap(), "0009001");
        assert_eq!(
            bindings.resolve("caller", "e164"),
            Err(BindingError::UnboundForm {
                name: "caller".into(),
                form: "e164".into(),
                bound: vec!["private".into()],
            })
        );
        assert_eq!(
            bindings.resolve("transferee", "e164"),
            Err(BindingError::UnboundIdentity { name: "transferee".into() })
        );
    }
}
