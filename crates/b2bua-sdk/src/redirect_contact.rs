//! [`RedirectContact`]: one Contact a decision-authored 3xx names
//! (RFC 3261 §20.10), and its reading from the `{uri, q, params}` JSON object
//! a decision payload carries.

use serde::Serialize;

/// One redirect target of a 3xx's Contact list, rendered
/// `<uri>;q=…;params…` in list order (the platform does not reorder,
/// ADR-0017 X5; the caller orders by `q`, §8.1.3.4).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RedirectContact {
    /// The `addr-spec` the caller dials next, its URI parameters its own:
    /// rendered inside the angle brackets.
    pub uri: String,
    /// The `c-p-q` preference, a `qvalue` in `0..=1` (§25.1); advisory to the
    /// caller. `None` states none.
    pub q: Option<f32>,
    /// The contact-params after `q`, in order: `c-p-expires` and any
    /// `contact-extension`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<ContactParam>,
}

/// One `generic-param` of a [`RedirectContact`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ContactParam {
    pub name: String,
    /// The `gen-value` as the wire writes it: a token or host bare, a
    /// quoted-string with its quotes and escapes; `None` for a bare flag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

impl RedirectContact {
    /// A target with no preference and no other contact-param.
    pub fn new(uri: impl Into<String>) -> Self {
        Self { uri: uri.into(), q: None, params: Vec::new() }
    }

    /// Read one `{uri, q?, params?}` object, `None` without a string `uri`. A
    /// `q` that is not a number states no preference; a `params` entry is a
    /// `{name, value?}` object, one without a string `name` left out.
    pub fn from_json(value: &serde_json::Value) -> Option<Self> {
        let uri = value.get("uri")?.as_str()?.to_string();
        let q = value.get("q").filter(|q| q.is_number()).and_then(|q| q.to_string().parse().ok());
        let params = value
            .get("params")
            .and_then(serde_json::Value::as_array)
            .map(|params| params.iter().filter_map(ContactParam::from_json).collect())
            .unwrap_or_default();
        Some(Self { uri, q, params })
    }
}

impl ContactParam {
    fn from_json(value: &serde_json::Value) -> Option<Self> {
        Some(Self {
            name: value.get("name")?.as_str()?.to_string(),
            value: value.get("value").and_then(serde_json::Value::as_str).map(str::to_string),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The JSON a target serializes to reads back as the same target.
    #[test]
    fn a_target_round_trips_through_its_json() {
        let target = RedirectContact {
            uri: "sip:carol@192.0.2.11;transport=tcp".into(),
            q: Some(0.5),
            params: vec![
                ContactParam { name: "expires".into(), value: Some("60".into()) },
                ContactParam { name: "+sip.text".into(), value: Some("\"a b\"".into()) },
                ContactParam { name: "flag".into(), value: None },
            ],
        };
        let json = serde_json::to_value(&target).unwrap();
        assert_eq!(RedirectContact::from_json(&json), Some(target));
    }

    /// A target with no contact-param serializes `q` and no `params`: the
    /// `{uri, q}` shape a payload pins.
    #[test]
    fn a_bare_target_serializes_uri_and_q() {
        let json = serde_json::to_value(RedirectContact::new("sip:carol@192.0.2.11")).unwrap();
        assert_eq!(json, json!({"uri": "sip:carol@192.0.2.11", "q": null}));
    }

    #[test]
    fn what_is_not_a_target_or_a_param_is_left_out() {
        assert_eq!(RedirectContact::from_json(&json!({"q": 0.5})), None);
        let read = RedirectContact::from_json(&json!({
            "uri": "sip:a@192.0.2.1", "q": "0.5",
            "params": [{"value": "60"}, {"name": "expires", "value": "60"}]
        }))
        .unwrap();
        assert_eq!(read.q, None);
        assert_eq!(
            read.params,
            [ContactParam { name: "expires".into(), value: Some("60".into()) }]
        );
    }
}
