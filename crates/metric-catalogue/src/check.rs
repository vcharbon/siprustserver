//! Whether a scraped text exposition holds a family exactly as its catalogue
//! entry declares it.

use std::fmt;

use crate::family::{Family, Kind};
use crate::render::escape_help;

/// How a scraped text departs from a family's catalogue entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mismatch {
    /// The family checked.
    pub family: &'static str,
    /// What differs.
    pub what: String,
}

impl fmt::Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.family, self.what)
    }
}

impl std::error::Error for Mismatch {}

type Pairs = Vec<(String, String)>;

impl Family {
    /// `Ok` when `text` holds this family exactly as declared: one `# HELP`
    /// line with this help, the `# TYPE` line of this kind right after it,
    /// then its series: every declared label set once, in exposition order;
    /// for a semi-open family, then any label set nobody declared, each
    /// under the label names of one block, once. A histogram's series is its
    /// `_bucket` samples (`le` ending at `+Inf`), its `_sum` and its
    /// `_count`. No other line of this family appears anywhere in `text`.
    /// Values are not checked.
    pub fn check(&self, text: &str) -> Result<(), Mismatch> {
        let fail = |what: String| Err(Mismatch { family: self.name, what });
        let lines: Vec<&str> = text.lines().collect();
        let help_prefix = format!("# HELP {} ", self.name);
        let helps: Vec<usize> =
            (0..lines.len()).filter(|&i| lines[i].starts_with(&help_prefix)).collect();
        let [at] = helps[..] else {
            return fail(format!("{} HELP lines, want 1", helps.len()));
        };
        let mut want_help = help_prefix.clone();
        escape_help(&mut want_help, self.help);
        if lines[at] != want_help {
            return fail(format!("HELP is {:?}, want {want_help:?}", lines[at]));
        }
        let want_type = format!("# TYPE {} {}", self.name, self.kind.as_str());
        if lines.get(at + 1) != Some(&want_type.as_str()) {
            return fail(format!("line after HELP is {:?}, want {want_type:?}", lines.get(at + 1)));
        }
        let mut end = at + 2;
        let mut series: Vec<Pairs> = Vec::new();
        while end < lines.len() {
            match self.series_at(&lines, end) {
                Ok(Some((pairs, next))) => {
                    series.push(pairs);
                    end = next;
                }
                Ok(None) => break,
                Err(what) => return fail(what),
            }
        }
        let want: Vec<Pairs> = self
            .labels
            .label_sets()
            .into_iter()
            .map(|set| set.into_iter().map(|(n, v)| (n.to_owned(), v.to_owned())).collect())
            .collect();
        for (k, labels) in want.iter().enumerate() {
            match series.get(k) {
                Some(got) if got == labels => {}
                Some(got) => {
                    return fail(format!("series {k} has labels {got:?}, want {labels:?}"))
                }
                None => return fail(format!("text ends before series {k} {labels:?}")),
            }
        }
        let extra = &series[want.len()..];
        if !extra.is_empty() && !self.is_semi_open() {
            return fail(format!("undeclared series {:?} of a fixed family", extra[0]));
        }
        for (k, got) in extra.iter().enumerate() {
            let names: Vec<&str> = got.iter().map(|(n, _)| n.as_str()).collect();
            if self.labels.block_named(&names).is_none() {
                return fail(format!("series {got:?} matches no block's label names"));
            }
            if want.contains(got) || extra[..k].contains(got) {
                return fail(format!("series {got:?} is written twice"));
            }
        }
        let type_prefix = format!("# TYPE {} ", self.name);
        for (i, line) in lines.iter().enumerate() {
            if (at..end).contains(&i) {
                continue;
            }
            if line.starts_with(&type_prefix) || self.sample(line).is_some() {
                return fail(format!("line {i} {line:?} lies outside the family's block"));
            }
        }
        Ok(())
    }

    /// The suffixes a sample of this family carries after its name.
    fn suffixes(&self) -> &'static [&'static str] {
        match self.kind {
            Kind::Histogram => &["_bucket", "_sum", "_count"],
            Kind::Counter | Kind::Gauge => &[""],
        }
    }

    /// The suffix and label pairs of `line` when it is a sample of this family.
    fn sample(&self, line: &str) -> Option<(&'static str, Pairs)> {
        let rest = line.strip_prefix(self.name)?;
        self.suffixes().iter().find_map(|suffix| {
            let pairs = sample_of(rest.strip_prefix(suffix)?)?;
            Some((*suffix, pairs))
        })
    }

    /// The series starting at line `i`, its label pairs and the line after
    /// it; `None` when line `i` is no sample of this family.
    fn series_at(&self, lines: &[&str], i: usize) -> Result<Option<(Pairs, usize)>, String> {
        let Some((suffix, pairs)) = lines.get(i).and_then(|l| self.sample(l)) else {
            return Ok(None);
        };
        if self.kind != Kind::Histogram {
            return Ok(Some((pairs, i + 1)));
        }
        let le = |p: &Pairs| p.last().filter(|(n, _)| n == "le").map(|(_, v)| v.clone());
        if suffix != "_bucket" || le(&pairs).is_none() {
            return Err(format!("line {i} {:?} does not open a series with a bucket", lines[i]));
        }
        let labels: Pairs = pairs[..pairs.len() - 1].to_vec();
        let mut j = i;
        loop {
            match lines.get(j).and_then(|l| self.sample(l)) {
                Some(("_bucket", p)) if p[..p.len().saturating_sub(1)] == labels[..] => {
                    let last = le(&p).as_deref() == Some("+Inf");
                    j += 1;
                    if last {
                        break;
                    }
                }
                _ => return Err(format!("series {labels:?} has no +Inf bucket")),
            }
        }
        for suffix in ["_sum", "_count"] {
            match lines.get(j).and_then(|l| self.sample(l)) {
                Some((s, p)) if s == suffix && p == labels => j += 1,
                _ => return Err(format!("series {labels:?} has no {suffix} after its buckets")),
            }
        }
        Ok(Some((labels, j)))
    }
}

/// The label pairs of a sample line from just after its metric name.
fn sample_of(rest: &str) -> Option<Pairs> {
    if rest.starts_with(' ') {
        return Some(Vec::new());
    }
    let mut chars = rest.strip_prefix('{')?.chars();
    let mut pairs = Vec::new();
    loop {
        let mut label = String::new();
        loop {
            match chars.next()? {
                '}' if label.is_empty() && pairs.is_empty() => return Some(pairs),
                '=' => break,
                c => label.push(c),
            }
        }
        if chars.next()? != '"' {
            return None;
        }
        let mut value = String::new();
        loop {
            match chars.next()? {
                '"' => break,
                '\\' => match chars.next()? {
                    'n' => value.push('\n'),
                    c => value.push(c),
                },
                c => value.push(c),
            }
        }
        pairs.push((label, value));
        match chars.next()? {
            ',' => continue,
            '}' => return chars.next().filter(|&c| c == ' ').map(|_| pairs),
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{Dim, Family, HistogramValue, Labels};

    const CLASS: Dim = Dim::new("class", &["normal", "emergency"]);
    const BY_CLASS: Family = Family::counter("c_total", Labels::Product(&[CLASS]), "by \"class\"");
    const SCALAR: Family = Family::gauge("g", Labels::None, "a gauge");

    fn rendered() -> String {
        let mut s =
            String::from("# HELP other_total x\n# TYPE other_total counter\nother_total 1\n");
        BY_CLASS.render(&mut s, |series| series.at(0));
        SCALAR.render_value(&mut s, 2.5);
        s
    }

    #[test]
    fn a_rendered_family_conforms() {
        let text = rendered();
        assert_eq!(BY_CLASS.check(&text), Ok(()));
        assert_eq!(SCALAR.check(&text), Ok(()));
    }

    #[test]
    fn a_missing_family_is_named() {
        let err = Family::counter("absent_total", Labels::None, "x").check(&rendered());
        assert_eq!(err.unwrap_err().what, "0 HELP lines, want 1");
    }

    #[test]
    fn a_changed_help_or_kind_is_a_mismatch() {
        let text = rendered();
        let help = Family::counter("c_total", Labels::Product(&[CLASS]), "by class");
        assert!(help.check(&text).unwrap_err().what.starts_with("HELP is"));
        let kind = Family::gauge("c_total", Labels::Product(&[CLASS]), "by \"class\"");
        assert!(kind.check(&text).unwrap_err().what.starts_with("line after HELP"));
    }

    #[test]
    fn a_missing_or_reordered_label_set_is_a_mismatch() {
        let text = rendered().replace("c_total{class=\"emergency\"} 1\n", "");
        assert!(BY_CLASS.check(&text).is_err());
        const SWAPPED: Family = Family::counter(
            "c_total",
            Labels::Product(&[Dim::new("class", &["emergency", "normal"])]),
            "by \"class\"",
        );
        assert!(SWAPPED.check(&rendered()).unwrap_err().what.starts_with("series 0 has labels"));
    }

    #[test]
    fn a_stray_sample_elsewhere_is_a_mismatch() {
        let text = format!("{}c_total{{class=\"normal\"}} 5\n", rendered());
        assert!(BY_CLASS.check(&text).unwrap_err().what.contains("outside the family's block"));
    }

    #[test]
    fn a_name_that_prefixes_another_is_not_confused_with_it() {
        let mut text = rendered();
        Family::counter("g_total", Labels::None, "another").render_value(&mut text, 1);
        assert_eq!(SCALAR.check(&text), Ok(()));
    }

    #[test]
    fn an_undeclared_series_of_a_fixed_family_is_a_mismatch() {
        let text = rendered().replace(
            "c_total{class=\"emergency\"} 1\n",
            "c_total{class=\"emergency\"} 1\nc_total{class=\"other\"} 1\n",
        );
        assert!(BY_CLASS.check(&text).unwrap_err().what.starts_with("undeclared series"));
    }

    #[test]
    fn a_semi_open_family_takes_undeclared_series_after_the_declared_ones_once_each() {
        const OPEN: Family =
            Family::counter("o_total", Labels::Product(&[CLASS]), "open").semi_open();
        let rows = |extra: &[&str]| {
            let mut s = String::new();
            let mut rows = vec![(vec!["emergency".to_string()], 2u64)];
            rows.extend(extra.iter().map(|v| (vec![v.to_string()], 1)));
            OPEN.render_rows(&mut s, rows);
            s
        };
        assert_eq!(OPEN.check(&rows(&[])), Ok(()));
        assert_eq!(OPEN.check(&rows(&["x", "y"])), Ok(()));
        let twice = rows(&["x"]) + "o_total{class=\"x\"} 1\n";
        assert!(OPEN.check(&twice).unwrap_err().what.contains("written twice"));
        let wrong = rows(&[]) + "o_total{kind=\"x\"} 1\n";
        assert!(OPEN.check(&wrong).unwrap_err().what.contains("no block"));
        let missing = rows(&["x"]).replace("o_total{class=\"normal\"} 0\n", "");
        assert!(OPEN.check(&missing).is_err(), "a declared series is never missing");
    }

    #[test]
    fn a_histogram_is_checked_by_its_bucket_sum_and_count_samples() {
        const H: Family = Family::histogram("h_seconds", Labels::Product(&[CLASS]), "latency");
        let value = HistogramValue { buckets: vec![(0.5, 1), (1.0, 2)], sum: 1.25, count: 3 };
        let mut text = String::new();
        H.render_histogram(&mut text, |_| value.clone());
        assert!(text.contains("h_seconds_bucket{class=\"normal\",le=\"1\"} 2\n"), "{text}");
        assert!(text.contains("h_seconds_bucket{class=\"normal\",le=\"+Inf\"} 3\n"));
        assert!(text.contains("h_seconds_sum{class=\"emergency\"} 1.25\n"));
        assert_eq!(H.check(&text), Ok(()));
        let no_inf = text.replace("h_seconds_bucket{class=\"emergency\",le=\"+Inf\"} 3\n", "");
        assert!(H.check(&no_inf).unwrap_err().what.contains("no +Inf bucket"));
        let no_count = text.replace("h_seconds_count{class=\"normal\"} 3\n", "");
        assert!(H.check(&no_count).unwrap_err().what.contains("no _count"));
        let stray = format!("{text}# HELP x y\nh_seconds_sum{{class=\"normal\"}} 1\n");
        assert!(H.check(&stray).unwrap_err().what.contains("outside the family's block"));
        let trailing = format!("{text}h_seconds_sum{{class=\"normal\"}} 1\n");
        assert!(H.check(&trailing).unwrap_err().what.contains("does not open a series"));
    }
}
