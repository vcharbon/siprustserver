//! JSON → [`Query`]. Hand-rolled rather than derived so every rejection names
//! the offending path and what was expected — a query is written by hand, and
//! a silently-ignored key is a wrong answer, not a warning.
//!
//! Unknown keys are ALWAYS an error: a misspelled predicate that is skipped
//! would widen the match set without saying so.

use std::fmt;

use serde_json::Value;

use crate::flow::{CorrelateStrategy, FlowConfig, DEFAULT_DEDUP_WINDOW_US, DEFAULT_PAIR_WINDOW_US};
use crate::txn::TxnKind;

use super::ast::*;

/// A rejected query: where in the document, and what was wrong.
#[derive(Debug, Clone)]
pub struct QueryError {
    pub path: String,
    pub msg: String,
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.msg)
    }
}

impl std::error::Error for QueryError {}

type R<T> = Result<T, QueryError>;

fn err<T>(path: &str, msg: impl Into<String>) -> R<T> {
    Err(QueryError { path: path.to_string(), msg: msg.into() })
}

/// Default derived-Call-ID tuning, shared by the default pipeline and by a
/// `{"derived_call_id": {}}` strategy that names no field.
const DERIVED_MIN_BASE_LEN: usize = 8;

impl Query {
    /// Parse a query document.
    pub fn from_json(v: &Value) -> R<Query> {
        let obj = v.as_object().ok_or_else(|| QueryError {
            path: "$".into(),
            msg: "a query is a JSON object".into(),
        })?;
        for k in obj.keys() {
            if !matches!(
                k.as_str(),
                "name" | "version" | "scope" | "correlate" | "select" | "project" | "neighbours"
            ) {
                return err("$", format!("unknown key {k:?}"));
            }
        }
        if let Some(ver) = obj.get("version") {
            if ver.as_u64() != Some(1) {
                return err(
                    "$.version",
                    format!("unsupported query version {ver}; this build reads 1"),
                );
            }
        }
        Ok(Query {
            name: obj.get("name").and_then(|n| n.as_str()).map(str::to_string),
            scope: match obj.get("scope") {
                Some(s) => scope(s, "$.scope")?,
                None => Scope::default(),
            },
            correlate: match obj.get("correlate") {
                Some(c) => Some(correlate(c, "$.correlate")?),
                None => None,
            },
            select: match obj.get("select") {
                Some(s) => node(s, "$.select")?,
                None => Node::Always(true),
            },
            project: match obj.get("project") {
                Some(p) => projection(p, "$.project")?,
                None => Projection::default(),
            },
            neighbours: match obj.get("neighbours") {
                Some(n) => Some(neighbours(n, "$.neighbours")?),
                None => None,
            },
        })
    }
}

fn scope(v: &Value, path: &str) -> R<Scope> {
    let obj = v
        .as_object()
        .ok_or_else(|| QueryError { path: path.into(), msg: "expected an object".into() })?;
    let mut out = Scope::default();
    for (k, val) in obj {
        match k.as_str() {
            "time" => {
                let t = val.as_object().ok_or_else(|| QueryError {
                    path: format!("{path}.time"),
                    msg: "expected an object".into(),
                })?;
                for (tk, tv) in t {
                    let us = tv.as_u64().ok_or_else(|| QueryError {
                        path: format!("{path}.time.{tk}"),
                        msg: "expected microseconds since the epoch".into(),
                    })?;
                    match tk.as_str() {
                        "from_us" => out.from_us = Some(us),
                        "to_us" => out.to_us = Some(us),
                        _ => return err(&format!("{path}.time"), format!("unknown key {tk:?}")),
                    }
                }
            }
            _ => return err(path, format!("unknown key {k:?}")),
        }
    }
    Ok(out)
}

fn correlate(v: &Value, path: &str) -> R<FlowConfig> {
    let obj = v
        .as_object()
        .ok_or_else(|| QueryError { path: path.into(), msg: "expected an object".into() })?;
    let mut cfg = FlowConfig { strategies: Vec::new(), dedup_window_us: DEFAULT_DEDUP_WINDOW_US };
    let mut saw_strategies = false;
    for (k, val) in obj {
        match k.as_str() {
            "dedup_window_us" => {
                cfg.dedup_window_us = val.as_u64().ok_or_else(|| QueryError {
                    path: format!("{path}.dedup_window_us"),
                    msg: "expected microseconds".into(),
                })?
            }
            "strategies" => {
                saw_strategies = true;
                let arr = val.as_array().ok_or_else(|| QueryError {
                    path: format!("{path}.strategies"),
                    msg: "expected an array".into(),
                })?;
                for (i, s) in arr.iter().enumerate() {
                    cfg.strategies.push(strategy(s, &format!("{path}.strategies[{i}]"))?);
                }
            }
            _ => return err(path, format!("unknown key {k:?}")),
        }
    }
    if !saw_strategies {
        cfg.strategies = FlowConfig::default().strategies;
    }
    Ok(cfg)
}

fn strategy(v: &Value, path: &str) -> R<CorrelateStrategy> {
    let (kind, body) = single_key(v, path)?;
    let obj = body.as_object().ok_or_else(|| QueryError {
        path: format!("{path}.{kind}"),
        msg: "expected an object of strategy settings".into(),
    })?;
    let get_u64 = |name: &str, default: u64| -> R<u64> {
        match obj.get(name) {
            None => Ok(default),
            Some(x) => x.as_u64().ok_or(QueryError {
                path: format!("{path}.{kind}.{name}"),
                msg: "expected a number".into(),
            }),
        }
    };
    let get_bool = |name: &str, default: bool| -> R<bool> {
        match obj.get(name) {
            None => Ok(default),
            Some(x) => x.as_bool().ok_or(QueryError {
                path: format!("{path}.{kind}.{name}"),
                msg: "expected a boolean".into(),
            }),
        }
    };
    match kind {
        "header_token" => {
            let arr = obj.get("headers").and_then(|h| h.as_array()).ok_or(QueryError {
                path: format!("{path}.header_token.headers"),
                msg: "expected an array of header names".into(),
            })?;
            Ok(CorrelateStrategy::HeaderToken {
                headers: arr.iter().filter_map(|h| h.as_str().map(str::to_string)).collect(),
            })
        }
        "header_param" => {
            let s = |name: &str| -> R<String> {
                obj.get(name).and_then(|x| x.as_str()).map(str::to_string).ok_or(QueryError {
                    path: format!("{path}.header_param.{name}"),
                    msg: "expected a string".into(),
                })
            };
            Ok(CorrelateStrategy::HeaderParam { header: s("header")?, param: s("param")? })
        }
        "derived_call_id" => Ok(CorrelateStrategy::DerivedCallId {
            window_us: get_u64("window_us", DEFAULT_PAIR_WINDOW_US)?,
            require_shared_hop: get_bool("require_shared_hop", true)?,
            min_base_len: get_u64("min_base_len", DERIVED_MIN_BASE_LEN as u64)? as usize,
        }),
        "identity_adjacency" => Ok(CorrelateStrategy::IdentityAdjacency {
            window_us: get_u64("window_us", DEFAULT_PAIR_WINDOW_US)?,
        }),
        other => err(
            path,
            format!(
                "unknown strategy {other:?}; expected header_token, header_param, \
                 derived_call_id or identity_adjacency"
            ),
        ),
    }
}

fn projection(v: &Value, path: &str) -> R<Projection> {
    let obj = v
        .as_object()
        .ok_or_else(|| QueryError { path: path.into(), msg: "expected an object".into() })?;
    for k in obj.keys() {
        if !matches!(k.as_str(), "mode" | "fields") {
            return err(path, format!("unknown key {k:?}"));
        }
    }
    let mode = obj.get("mode").and_then(|m| m.as_str()).unwrap_or("summary");
    match mode {
        "count" => Ok(Projection::Count),
        "full" => Ok(Projection::Full),
        "summary" => {
            let Some(fields) = obj.get("fields") else {
                return Ok(Projection::default());
            };
            let arr = fields.as_array().ok_or_else(|| QueryError {
                path: format!("{path}.fields"),
                msg: "expected an array of field names".into(),
            })?;
            let mut out = Vec::new();
            for (i, f) in arr.iter().enumerate() {
                let name = f.as_str().ok_or_else(|| QueryError {
                    path: format!("{path}.fields[{i}]"),
                    msg: "expected a field name".into(),
                })?;
                out.push(KeyField::parse(name).ok_or_else(|| QueryError {
                    path: format!("{path}.fields[{i}]"),
                    msg: format!("unknown field {name:?}; known: {}", KeyField::names()),
                })?);
            }
            Ok(Projection::Summary { fields: out })
        }
        other => err(
            &format!("{path}.mode"),
            format!("unknown mode {other:?}; expected count, summary or full"),
        ),
    }
}

fn neighbours(v: &Value, path: &str) -> R<Neighbours> {
    let obj = v
        .as_object()
        .ok_or_else(|| QueryError { path: path.into(), msg: "expected an object".into() })?;
    for k in obj.keys() {
        if !matches!(k.as_str(), "key" | "window_us" | "max") {
            return err(path, format!("unknown key {k:?}"));
        }
    }
    let arr = obj.get("key").and_then(|k| k.as_array()).ok_or(QueryError {
        path: format!("{path}.key"),
        msg: "expected an array of field names defining sameness".into(),
    })?;
    let mut key = Vec::new();
    for (i, f) in arr.iter().enumerate() {
        let name = f.as_str().unwrap_or_default();
        key.push(KeyField::parse(name).ok_or_else(|| QueryError {
            path: format!("{path}.key[{i}]"),
            msg: format!("unknown field {name:?}; known: {}", KeyField::names()),
        })?);
    }
    if key.is_empty() {
        return err(&format!("{path}.key"), "at least one field is required");
    }
    Ok(Neighbours {
        key,
        window_us: obj.get("window_us").and_then(|w| w.as_u64()).ok_or(QueryError {
            path: format!("{path}.window_us"),
            msg: "expected how far either side of a hit to look, in microseconds".into(),
        })?,
        max: obj.get("max").and_then(|m| m.as_u64()).unwrap_or(0) as usize,
    })
}

/// The one-key-object convention every tagged node uses.
fn single_key<'a>(v: &'a Value, path: &str) -> R<(&'a str, &'a Value)> {
    let obj = v.as_object().ok_or_else(|| QueryError {
        path: path.into(),
        msg: "expected an object with exactly one key".into(),
    })?;
    let mut it = obj.iter();
    match (it.next(), it.next()) {
        (Some((k, val)), None) => Ok((k.as_str(), val)),
        (None, _) => err(path, "expected an object with exactly one key, got {}"),
        (Some(_), Some(_)) => err(
            path,
            format!(
                "expected exactly one key, got {} — wrap them in {{\"all\": [...]}}",
                obj.keys().cloned().collect::<Vec<_>>().join(", ")
            ),
        ),
    }
}

fn nodes(v: &Value, path: &str) -> R<Vec<Node>> {
    let arr = v.as_array().ok_or_else(|| QueryError {
        path: path.into(),
        msg: "expected an array of predicates".into(),
    })?;
    arr.iter().enumerate().map(|(i, n)| node(n, &format!("{path}[{i}]"))).collect()
}

/// Parse one predicate node.
pub fn node(v: &Value, path: &str) -> R<Node> {
    if let Some(b) = v.as_bool() {
        return Ok(Node::Always(b));
    }
    let (kind, body) = single_key(v, path)?;
    let sub = format!("{path}.{kind}");
    match kind {
        "all" => Ok(Node::All(nodes(body, &sub)?)),
        "any" => Ok(Node::Any(nodes(body, &sub)?)),
        "not" => Ok(Node::Not(Box::new(node(body, &sub)?))),
        "any_leg" => Ok(Node::AnyLeg(Box::new(node(body, &sub)?))),
        "any_txn" => Ok(Node::AnyTxn(Box::new(node(body, &sub)?))),
        "any_msg" => Ok(Node::AnyMsg(Box::new(node(body, &sub)?))),
        "request" => Ok(Node::Request(Box::new(node(body, &sub)?))),
        "any_response" => Ok(Node::AnyResponse(Box::new(node(body, &sub)?))),
        "count_leg" => Ok(Node::CountLeg(num_cmp(body, &sub)?)),
        "count_txn" => {
            let obj = body.as_object().ok_or_else(|| QueryError {
                path: sub.clone(),
                msg: "expected an object with an optional \"where\" and a comparison".into(),
            })?;
            let filter = match obj.get("where") {
                Some(w) => Some(Box::new(node(w, &format!("{sub}.where"))?)),
                None => None,
            };
            let mut rest = obj.clone();
            rest.remove("where");
            Ok(Node::CountTxn { filter, count: num_cmp(&Value::Object(rest), &sub)? })
        }

        "evidence_kind" => Ok(Node::EvidenceKind(string(body, &sub)?)),
        "as_socket" => Ok(Node::AsSocket(str_match(body, &sub)?)),
        "call_id" => Ok(Node::CallId(str_match(body, &sub)?)),
        "ruri" => Ok(Node::Ruri(str_match(body, &sub)?)),
        "from" => Ok(Node::FromUri(str_match(body, &sub)?)),
        "to" => Ok(Node::ToUri(str_match(body, &sub)?)),
        "final_status" => Ok(Node::FinalStatus(status_match(body, &sub)?)),
        "status" => Ok(Node::Status(status_match(body, &sub)?)),
        "saw_180" => Ok(Node::Saw180(boolean(body, &sub)?)),
        "terminated_by" => Ok(Node::TerminatedBy(str_match(body, &sub)?)),
        "duration_us" => Ok(Node::DurationUs(num_cmp(body, &sub)?)),
        "latency_us" => Ok(Node::LatencyUs(num_cmp(body, &sub)?)),
        "kind" => {
            let s = string(body, &sub)?;
            Ok(Node::TxnKindIs(TxnKind::parse(&s).ok_or_else(|| QueryError {
                path: sub.clone(),
                msg: format!(
                    "unknown transaction kind {s:?}; expected initial_invite, reinvite, \
                     in_dialog or out_of_dialog"
                ),
            })?))
        }
        "method" => Ok(Node::MethodIs(string(body, &sub)?.to_ascii_uppercase())),
        "is_request" => Ok(Node::IsRequest(boolean(body, &sub)?)),
        "retx" => Ok(Node::Retx(boolean(body, &sub)?)),
        "body" => Ok(Node::Body(str_match(body, &sub)?)),
        "src" => Ok(Node::Src(str_match(body, &sub)?)),
        "dst" => Ok(Node::Dst(str_match(body, &sub)?)),
        "reason_cause" => Ok(Node::ReasonCause(num_cmp(body, &sub)?)),
        "header" => {
            let obj = body.as_object().ok_or_else(|| QueryError {
                path: sub.clone(),
                msg: "expected {\"name\": …, plus a string match}".into(),
            })?;
            let name = obj.get("name").and_then(|n| n.as_str()).ok_or(QueryError {
                path: format!("{sub}.name"),
                msg: "expected the header name".into(),
            })?;
            let mut rest = obj.clone();
            rest.remove("name");
            Ok(Node::Header {
                name: name.to_string(),
                value: if rest.is_empty() {
                    // Naming a header with no match asks whether it is present.
                    StrMatch::Contains(String::new())
                } else {
                    str_match(&Value::Object(rest), &sub)?
                },
            })
        }
        other => err(path, format!("unknown predicate {other:?}")),
    }
}

fn string(v: &Value, path: &str) -> R<String> {
    v.as_str()
        .map(str::to_string)
        .ok_or(QueryError { path: path.into(), msg: "expected a string".into() })
}

fn boolean(v: &Value, path: &str) -> R<bool> {
    v.as_bool().ok_or(QueryError { path: path.into(), msg: "expected a boolean".into() })
}

fn str_match(v: &Value, path: &str) -> R<StrMatch> {
    if let Some(s) = v.as_str() {
        return Ok(StrMatch::Equals(s.to_string()));
    }
    let (kind, body) = single_key(v, path)?;
    let sub = format!("{path}.{kind}");
    match kind {
        "equals" => Ok(StrMatch::Equals(string(body, &sub)?)),
        "contains" => Ok(StrMatch::Contains(string(body, &sub)?)),
        "prefix" => Ok(StrMatch::Prefix(string(body, &sub)?)),
        "suffix" => Ok(StrMatch::Suffix(string(body, &sub)?)),
        "absent" => match boolean(body, &sub)? {
            true => Ok(StrMatch::Absent),
            false => err(&sub, "\"absent\": false says nothing; use a positive match"),
        },
        other => err(
            path,
            format!("unknown string match {other:?}; expected equals, contains, prefix, suffix or absent"),
        ),
    }
}

fn num_cmp(v: &Value, path: &str) -> R<NumCmp> {
    if let Some(n) = v.as_u64() {
        return Ok(NumCmp { eq: Some(n), ..Default::default() });
    }
    let obj = v.as_object().ok_or_else(|| QueryError {
        path: path.into(),
        msg: "expected a number or an object of bounds (eq, ne, ge, le, gt, lt)".into(),
    })?;
    let mut out = NumCmp::default();
    for (k, val) in obj {
        let n = val.as_u64().ok_or_else(|| QueryError {
            path: format!("{path}.{k}"),
            msg: "expected a number".into(),
        })?;
        match k.as_str() {
            "eq" => out.eq = Some(n),
            "ne" => out.ne = Some(n),
            "ge" => out.ge = Some(n),
            "le" => out.le = Some(n),
            "gt" => out.gt = Some(n),
            "lt" => out.lt = Some(n),
            other => {
                return err(
                    path,
                    format!("unknown bound {other:?}; expected eq, ne, ge, le, gt or lt"),
                )
            }
        }
    }
    if out.is_empty() {
        return err(path, "expected at least one bound");
    }
    Ok(out)
}

fn status_match(v: &Value, path: &str) -> R<StatusMatch> {
    if let Some(s) = v.as_str() {
        if s == "none" {
            return Ok(StatusMatch::NoneSeen);
        }
        if let Some(class) = s.strip_suffix("xx").and_then(|c| c.parse::<u16>().ok()) {
            return Ok(StatusMatch::Class(class));
        }
        return err(
            path,
            format!("unknown status {s:?}; expected a code, a class like \"4xx\", or \"none\""),
        );
    }
    Ok(StatusMatch::Cmp(num_cmp(v, path)?))
}
