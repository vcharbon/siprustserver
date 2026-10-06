//! A contract read back from the catalogues' JSON
//! (`metric_catalogue::export`): what the checker needs of each family.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

/// One family as a reader may name it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FamilyRecord {
    /// `counter`, `gauge` or `histogram`.
    pub kind: String,
    /// Every label name of every block, with its values when they are a
    /// closed list (a fixed family's), `None` when a value nobody declared
    /// may appear.
    pub labels: BTreeMap<String, Option<BTreeSet<String>>>,
}

/// Every family of every catalogue, and the name prefixes they own.
#[derive(Debug, Clone, Default)]
pub struct Contract {
    /// The families, by name.
    pub families: BTreeMap<String, FamilyRecord>,
    /// The name prefixes the families own, longest first: per first name
    /// segment, the longest prefix every family of that segment shares, cut
    /// at a `_`; a lone family's first two segments.
    pub namespaces: Vec<String>,
}

impl Contract {
    /// The contract stated by `json`, the catalogues' export.
    pub fn from_json(json: &str) -> Result<Self, String> {
        let root: Value = serde_json::from_str(json).map_err(|e| format!("catalogue JSON: {e}"))?;
        let catalogues = root["catalogues"].as_array().ok_or("catalogue JSON: no catalogues")?;
        let mut contract = Contract::default();
        for c in catalogues {
            for f in c["families"].as_array().ok_or("catalogue JSON: no families")? {
                let name = f["name"].as_str().ok_or("catalogue JSON: a family name")?;
                let kind = f["kind"].as_str().ok_or("catalogue JSON: a family kind")?;
                let closed = f["semi_open"].as_bool() == Some(false);
                let mut labels: BTreeMap<String, Option<BTreeSet<String>>> = BTreeMap::new();
                for block in f["labels"].as_array().into_iter().flatten() {
                    for dim in block.as_array().into_iter().flatten() {
                        let label = dim["name"].as_str().ok_or("catalogue JSON: a label name")?;
                        let values = dim["values"].as_array().into_iter().flatten();
                        let values: BTreeSet<String> =
                            values.filter_map(|v| v.as_str().map(str::to_owned)).collect();
                        let entry = labels
                            .entry(label.to_owned())
                            .or_insert_with(|| closed.then(BTreeSet::new));
                        if let Some(set) = entry {
                            set.extend(values);
                        }
                    }
                }
                let record = FamilyRecord { kind: kind.to_owned(), labels };
                contract.families.insert(name.to_owned(), record);
            }
        }
        contract.namespaces = namespaces(contract.families.keys().map(String::as_str));
        Ok(contract)
    }

    /// The family a sample name belongs to: the family of that name, or the
    /// histogram whose `_bucket`, `_sum` or `_count` it is; with whether it
    /// is a histogram's `_bucket` (which carries `le`).
    pub(crate) fn family_of(&self, name: &str) -> Option<(&FamilyRecord, bool)> {
        if let Some(f) = self.families.get(name) {
            return Some((f, false));
        }
        ["_bucket", "_sum", "_count"].into_iter().find_map(|suffix| {
            let f = self.families.get(name.strip_suffix(suffix)?)?;
            (f.kind == "histogram").then_some((f, suffix == "_bucket"))
        })
    }

    /// Whether some family's name starts with `prefix`.
    pub(crate) fn has_prefix(&self, prefix: &str) -> bool {
        self.families.range(prefix.to_owned()..).next().is_some_and(|(n, _)| n.starts_with(prefix))
    }
}

/// The name prefixes `names` own, longest first (see [`Contract::namespaces`]).
pub(crate) fn namespaces<'a>(names: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut groups: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for name in names {
        let first = name.split('_').next().unwrap_or(name);
        groups.entry(first).or_default().push(name);
    }
    let mut out: Vec<String> = groups
        .values()
        .map(|names| {
            if let [one] = names[..] {
                let two: Vec<&str> = one.splitn(3, '_').take(2).collect();
                return format!("{}_", two.join("_"));
            }
            let mut common = names[0].to_owned();
            for n in &names[1..] {
                let len = common.bytes().zip(n.bytes()).take_while(|(a, b)| a == b).count();
                common.truncate(len);
            }
            let cut = common.rfind('_').unwrap_or(common.len());
            format!("{}_", &common[..cut])
        })
        .collect();
    out.sort_by_key(|ns| std::cmp::Reverse(ns.len()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_group_owns_its_common_prefix_and_a_lone_family_its_first_two_segments() {
        let ns = namespaces(
            [
                "b2bua_a_total",
                "b2bua_b",
                "http_request_failures_total",
                "http_request_failures_overflow_total",
                "log_lines_dropped_total",
                "trace_admitted_total",
                "trace_denied_rate_total",
            ]
            .into_iter(),
        );
        assert_eq!(ns, ["http_request_failures_", "log_lines_", "b2bua_", "trace_"]);
    }
}
