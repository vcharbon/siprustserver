//! The checker: every metric name a text spells under the families'
//! namespaces, and every matcher of a plain selector attached to a
//! catalogued name, against the contract.
//!
//! A name is a maximal run of `[a-z0-9_]` that starts after no other
//! identifier character and with a namespace ([`Contract::namespaces`]), and
//! is followed by neither an uppercase letter nor `::` (an identifier or a
//! code path, not a metric). Every such name is read as a metric name: a
//! reader's own identifier under a namespace is allow-listed for its file.
//! It conforms when it names a family (a histogram's `_bucket`, `_sum` or
//! `_count` included), when it is a prefix some family name continues and is
//! spelled as one (ending in `_`, or followed by a regex `.*`, `.+` or
//! `\w`), or when the allow-list names it in that file.
//!
//! A selector is plain when every matcher in its braces reads
//! `label op "value"` (op `=`, `!=`, `=~`, `!~`, the quotes possibly escaped
//! as in JSON). Each label must be a label of the family, `le` on a
//! `_bucket`, or allowed; a literal value (`=`, `!=`) of a fixed family's
//! label must be one of its values. No PromQL is parsed beyond that.

use std::path::{Path, PathBuf};

use crate::allow::Allow;
use crate::contract::Contract;

/// One departure from the contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The file it was found in.
    pub file: PathBuf,
    /// Its line, from 1.
    pub line: usize,
    /// The metric name spelled.
    pub name: String,
    /// What is wrong.
    pub what: String,
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}: {}: {}", self.file.display(), self.line, self.name, self.what)
    }
}

/// Directories never scanned: build output, dependencies, VCS state.
const SKIPPED_DIRS: [&str; 5] = ["node_modules", "target", ".git", "dist", "__pycache__"];

/// The allow-list's file name: an allow-list is no reader.
const ALLOW_LIST: &str = "metric-names.allow";

/// Files larger than this are not readers (captures, corpora).
const MAX_FILE_BYTES: u64 = 4 << 20;

fn ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn name_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'
}

/// Every finding in `text`, read from `file`.
pub(crate) fn check_text(
    contract: &Contract,
    allow: &Allow,
    file: &Path,
    text: &str,
) -> Vec<Finding> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let finding = |name: &str, what: String| Finding {
            file: file.to_owned(),
            line: n + 1,
            name: name.to_owned(),
            what,
        };
        let chars: Vec<(usize, char)> = line.char_indices().collect();
        let mut i = 0;
        while i < chars.len() {
            let (at, c) = chars[i];
            let starts = name_char(c) && (i == 0 || !ident(chars[i - 1].1));
            if !starts {
                i += 1;
                continue;
            }
            let mut j = i;
            while j < chars.len() && name_char(chars[j].1) {
                j += 1;
            }
            let end = chars.get(j).map_or(line.len(), |(e, _)| *e);
            let name = &line[at..end];
            let rest = &line[end..];
            i = j;
            if !contract.namespaces.iter().any(|ns| name.starts_with(ns.as_str())) {
                continue;
            }
            if rest.starts_with(|c: char| c.is_ascii_uppercase()) || rest.starts_with("::") {
                continue;
            }
            match contract.family_of(name) {
                Some((family, bucket)) => {
                    for m in parse_selector(rest).unwrap_or_default() {
                        let label = m.label.as_str();
                        match family.labels.get(label) {
                            Some(Some(values)) if m.literal && !m.value.is_empty() => {
                                if !values.contains(&m.value) && !m.value.contains('$') {
                                    out.push(finding(
                                        name,
                                        format!("{label}={:?} is none of its values", m.value),
                                    ));
                                }
                            }
                            Some(_) => {}
                            None if (bucket && label == "le") || allow.label(label) => {}
                            None => out.push(finding(
                                name,
                                format!("label {label:?} is none of its labels"),
                            )),
                        }
                    }
                }
                None if allow.name(file, name) => {}
                None if spelled_as_prefix(name, rest) => {
                    if !contract.has_prefix(name) {
                        out.push(finding(name, "no catalogued family starts with it".to_owned()));
                    }
                }
                None => out.push(finding(name, "is in no catalogue".to_owned())),
            }
        }
    }
    out
}

/// Whether a name that is no family is spelled as the start of one: it ends
/// in `_`, or a regex wildcard continues it.
fn spelled_as_prefix(name: &str, rest: &str) -> bool {
    name.ends_with('_') || [".*", ".+", "\\w"].iter().any(|p| rest.starts_with(p))
}

/// One matcher of a plain selector.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Matcher {
    label: String,
    /// `=` or `!=`: the value is a literal, not a regex.
    literal: bool,
    value: String,
}

/// The matchers of a plain selector at the start of `rest`.
fn parse_selector(rest: &str) -> Option<Vec<Matcher>> {
    let body = rest.strip_prefix('{')?;
    let close = body.find('}')?;
    let inner = body[..close].replace("\\\"", "\"");
    let inner = inner.trim();
    if inner.is_empty() {
        return Some(Vec::new());
    }
    let mut matchers = Vec::new();
    let mut s = inner;
    loop {
        let name_end = s.find(|c: char| !ident(c)).unwrap_or(s.len());
        let label = &s[..name_end];
        if label.is_empty() || !label.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
            return None;
        }
        let after = s[name_end..].trim_start();
        let (op, after) = ["=~", "!~", "!=", "="]
            .iter()
            .find_map(|op| after.strip_prefix(op).map(|rest| (*op, rest)))?;
        let after = after.trim_start();
        let quote = after.chars().next().filter(|q| *q == '"' || *q == '\'')?;
        let value_end = after[1..].find(quote)? + 1;
        matchers.push(Matcher {
            label: label.to_owned(),
            literal: op == "=" || op == "!=",
            value: after[1..value_end].to_owned(),
        });
        s = after[value_end + 1..].trim_start();
        match s.strip_prefix(',') {
            Some(more) => s = more.trim_start(),
            None if s.is_empty() => return Some(matchers),
            None => return None,
        }
        if s.is_empty() {
            return Some(matchers);
        }
    }
}

/// Every finding under `paths`: a file is read as is, a directory (a
/// trailing `/**` read as the directory) recursively, skipping build output,
/// dependencies, allow-lists, files over 4 MiB and files that are not UTF-8
/// text; then every allow entry none of them used.
pub fn check_paths(contract: &Contract, allow: &Allow, paths: &[PathBuf]) -> Vec<Finding> {
    let mut files = Vec::new();
    for p in paths {
        let p = match p.to_str().and_then(|s| s.strip_suffix("/**")) {
            Some(dir) => PathBuf::from(dir),
            None => p.clone(),
        };
        collect(&p, &mut files);
    }
    files.sort();
    let mut out = Vec::new();
    for file in files.into_iter().filter(|f| !f.ends_with(ALLOW_LIST)) {
        if let Ok(text) = std::fs::read_to_string(&file) {
            out.extend(check_text(contract, allow, &file, &text));
        }
    }
    for (file, line, name) in allow.unused() {
        out.push(Finding {
            file: file.to_owned(),
            line,
            name: name.to_owned(),
            what: "stale allow entry: no reader in its scope spells it".to_owned(),
        });
    }
    out
}

fn collect(path: &Path, files: &mut Vec<PathBuf>) {
    let Ok(meta) = std::fs::metadata(path) else { return };
    if meta.is_file() {
        if meta.len() <= MAX_FILE_BYTES {
            files.push(path.to_owned());
        }
        return;
    }
    let skipped =
        path.file_name().and_then(|n| n.to_str()).is_some_and(|n| SKIPPED_DIRS.contains(&n));
    if !meta.is_dir() || skipped {
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else { return };
    for entry in entries.flatten() {
        collect(&entry.path(), files);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::FamilyRecord;

    fn contract() -> Contract {
        let mut c = Contract::default();
        let family = |kind: &str, labels: &[(&str, Option<&[&str]>)]| FamilyRecord {
            kind: kind.to_owned(),
            labels: labels
                .iter()
                .map(|(l, v)| (l.to_string(), v.map(|v| v.iter().map(|x| x.to_string()).collect())))
                .collect(),
        };
        let reason: &[&str] = &["a", "b"];
        c.families.insert("b2bua_x_total".into(), family("counter", &[("reason", Some(reason))]));
        c.families.insert("b2bua_lat_seconds".into(), family("histogram", &[("scenario", None)]));
        c.families.insert("b2bua_repl_applied_total".into(), family("counter", &[("flow", None)]));
        c.namespaces = vec!["b2bua_".into()];
        c
    }

    fn check(text: &str) -> Vec<String> {
        let mut allow = Allow::default();
        allow
            .add(Path::new("/r/a.allow"), "f: b2bua_theirs_total\nf: b2bua_chaos_*\nlabel:pod\n")
            .unwrap();
        check_text(&contract(), &allow, Path::new("/r/f"), text)
            .into_iter()
            .map(|f| format!("{} {}", f.name, f.what))
            .collect()
    }

    /// A literal value of a closed label list is checked; a regex, a
    /// template variable, the empty value and an open label's are not.
    #[test]
    fn a_literal_value_of_a_closed_label_is_checked() {
        assert_eq!(
            check(r#"b2bua_x_total{reason!="none"}"#),
            [r#"b2bua_x_total reason="none" is none of its values"#]
        );
        assert!(check(r#"b2bua_x_total{reason="$r", reason!="", reason=~"n.*"}"#).is_empty());
        assert!(check(r#"b2bua_lat_seconds_bucket{scenario="any"}"#).is_empty());
    }

    /// An allowed name is allowed in the file its entry names, nowhere else.
    #[test]
    fn an_allowed_name_elsewhere_is_a_finding() {
        let mut allow = Allow::default();
        allow.add(Path::new("/r/a.allow"), "f: b2bua_theirs_total\n").unwrap();
        let found = check_text(&contract(), &allow, Path::new("/r/g"), "b2bua_theirs_total");
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn a_catalogued_name_or_histogram_sample_conforms() {
        assert!(check("rate(b2bua_x_total[1m]) + b2bua_lat_seconds_bucket").is_empty());
        assert!(check("histogram_quantile(0.9, b2bua_lat_seconds_count)").is_empty());
    }

    #[test]
    fn a_name_in_no_catalogue_is_found() {
        assert_eq!(check("sum(b2bua_y_total)"), ["b2bua_y_total is in no catalogue"]);
        assert_eq!(check("b2bua_active"), ["b2bua_active is in no catalogue"]);
        assert_eq!(check("b2bua_x_total_count"), ["b2bua_x_total_count is in no catalogue"]);
    }

    #[test]
    fn a_name_outside_the_namespaces_or_inside_another_word_is_not_read() {
        assert!(check("sipp_calls_total xb2bua_y_total B2BUA_Y b2bua_crate::f").is_empty());
    }

    #[test]
    fn an_allowed_name_or_prefix_conforms() {
        assert!(check("b2bua_theirs_total b2bua_chaos_window").is_empty());
    }

    #[test]
    fn a_prefix_conforms_when_a_family_continues_it() {
        assert!(check(r#"{__name__=~"b2bua_repl_.*"} b2bua_repl_applied.* b2bua_lat_"#).is_empty());
        assert_eq!(check("b2bua_zzz_.*"), ["b2bua_zzz_ no catalogued family starts with it"]);
        assert!(check("f\"b2bua_{name}_total\"").is_empty());
    }

    #[test]
    fn the_labels_of_a_plain_selector_are_checked() {
        assert!(check(r#"b2bua_x_total{reason="a",pod=~"w.*",reason!~"z.*"}"#).is_empty());
        assert!(check(r#"b2bua_lat_seconds_bucket{scenario!="a", le="0.5"}"#).is_empty());
        assert_eq!(
            check(r#"b2bua_x_total{cause="a"}"#),
            [r#"b2bua_x_total label "cause" is none of its labels"#]
        );
        assert_eq!(
            check(r#""expr": "rate(b2bua_x_total{kind=\"a\"}[1m])""#),
            [r#"b2bua_x_total label "kind" is none of its labels"#]
        );
        assert_eq!(
            check(r#"b2bua_lat_seconds_sum{le="1"}"#),
            [r#"b2bua_lat_seconds_sum label "le" is none of its labels"#]
        );
    }

    #[test]
    fn a_selector_that_is_not_plain_is_not_read() {
        assert!(check("b2bua_x_total{reason,code} b2bua_x_total{$sel}").is_empty());
    }
}
