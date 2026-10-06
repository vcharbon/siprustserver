//! The Prometheus text exposition (format 0.0.4) of a family, from its
//! catalogue entry and one value per series.

use std::collections::HashMap;
use std::fmt::{Display, Write};

use crate::family::{Family, Kind};
use crate::labels::{Dim, Labels, Series};

/// One histogram series' observations: the cumulative count at or below each
/// bucket bound, ascending, then the sum of the observed values and their
/// count (the implicit `+Inf` bucket).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HistogramValue {
    /// `(le, cumulative count)` per finite bucket, `le` ascending.
    pub buckets: Vec<(f64, u64)>,
    /// The sum of every observed value.
    pub sum: f64,
    /// How many values were observed.
    pub count: u64,
}

impl Family {
    /// Append the family to `out`: its `# HELP` and `# TYPE` lines, then one
    /// sample per declared label set in exposition order, valued by `value`.
    /// Every declared label set is written, whatever its value.
    ///
    /// Panics for a histogram: render it with
    /// [`render_histogram`](Self::render_histogram).
    pub fn render<V: Display>(&self, out: &mut String, mut value: impl FnMut(&Series<'_>) -> V) {
        assert!(self.kind != Kind::Histogram, "{} is a histogram", self.name);
        self.render_header(out);
        self.labels.for_each_series(|series| {
            let labels: Vec<(&str, &str)> = series.labels().collect();
            write_sample(out, self.name, "", &labels, None, value(series));
        });
    }

    /// Append a family without labels, valued `value`.
    ///
    /// Panics when the family declares labels.
    pub fn render_value(&self, out: &mut String, value: impl Display) {
        assert!(
            matches!(self.labels, Labels::None),
            "{} declares labels: render it per series",
            self.name
        );
        let mut value = Some(value);
        self.render(out, |_| value.take().expect("one series"));
    }

    /// The text of the family alone, as [`render`](Self::render) appends it.
    pub fn text<V: Display>(&self, value: impl FnMut(&Series<'_>) -> V) -> String {
        let mut out = String::new();
        self.render(&mut out, value);
        out
    }

    /// Append a family from the label sets observed so far, each a list of
    /// label values in the dimension order of its block, with its value:
    /// every declared label set first, in exposition order, valued from its
    /// row or `V::default()` without one; then every other row, in the order
    /// given, under the labels of the block with as many dimensions.
    ///
    /// Panics for a histogram, and for an undeclared row of a fixed family.
    pub fn render_rows<R, S, V>(&self, out: &mut String, rows: impl IntoIterator<Item = (R, V)>)
    where
        R: IntoIterator<Item = S>,
        S: AsRef<str>,
        V: Display + Default,
    {
        assert!(self.kind != Kind::Histogram, "{} is a histogram", self.name);
        self.render_header(out);
        self.each_row(rows, |labels, value| write_sample(out, self.name, "", labels, None, value));
    }

    /// Append a histogram: one group of `_bucket`, `_sum` and `_count`
    /// samples per declared label set, in exposition order.
    pub fn render_histogram(
        &self,
        out: &mut String,
        mut value: impl FnMut(&Series<'_>) -> HistogramValue,
    ) {
        assert!(self.kind == Kind::Histogram, "{} is not a histogram", self.name);
        self.render_header(out);
        self.labels.for_each_series(|series| {
            let labels: Vec<(&str, &str)> = series.labels().collect();
            write_histogram(out, self.name, &labels, &value(series));
        });
    }

    /// Append a histogram from its observed label sets, as
    /// [`render_rows`](Self::render_rows) does for a counter or a gauge; a
    /// declared label set without a row has no observation.
    pub fn render_histogram_rows<R, S>(
        &self,
        out: &mut String,
        rows: impl IntoIterator<Item = (R, HistogramValue)>,
    ) where
        R: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        assert!(self.kind == Kind::Histogram, "{} is not a histogram", self.name);
        self.render_header(out);
        self.each_row(rows, |labels, value| write_histogram(out, self.name, labels, &value));
    }

    /// Visit every row to write: the declared label sets, valued from `rows`
    /// or by default, then the undeclared rows in order.
    fn each_row<R, S, V>(
        &self,
        rows: impl IntoIterator<Item = (R, V)>,
        mut write: impl FnMut(&[(&str, &str)], V),
    ) where
        R: IntoIterator<Item = S>,
        S: AsRef<str>,
        V: Default,
    {
        let mut rows: Vec<(Vec<String>, Option<V>)> = rows
            .into_iter()
            .map(|(r, v)| (r.into_iter().map(|s| s.as_ref().to_owned()).collect(), Some(v)))
            .collect();
        let index: HashMap<Vec<String>, usize> =
            rows.iter().enumerate().map(|(i, (r, _))| (r.clone(), i)).collect();
        let mut key = Vec::new();
        self.labels.for_each_series(|series| {
            key.clear();
            key.extend(series.labels().map(|(_, v)| v.to_owned()));
            let value = index.get(&key).and_then(|&i| rows[i].1.take()).unwrap_or_default();
            let labels: Vec<(&str, &str)> = series.labels().collect();
            write(&labels, value);
        });
        for (values, value) in rows.iter_mut() {
            let Some(value) = value.take() else { continue };
            assert!(self.is_semi_open(), "{} is fixed: {values:?} is not declared", self.name);
            let dims: &[Dim] = self.labels.block_of_arity(values.len());
            let labels: Vec<(&str, &str)> =
                dims.iter().map(|d| d.name).zip(values.iter().map(String::as_str)).collect();
            write(&labels, value);
        }
    }

    fn render_header(&self, out: &mut String) {
        let _ = write!(out, "# HELP {} ", self.name);
        escape_help(out, self.help);
        let _ = writeln!(out, "\n# TYPE {} {}", self.name, self.kind.as_str());
    }
}

/// One sample line: `name` + `suffix`, the labels (then `le` when given),
/// the value.
fn write_sample(
    out: &mut String,
    name: &str,
    suffix: &str,
    labels: &[(&str, &str)],
    le: Option<&str>,
    value: impl Display,
) {
    out.push_str(name);
    out.push_str(suffix);
    if !labels.is_empty() || le.is_some() {
        out.push('{');
        for (i, (name, v)) in labels.iter().copied().chain(le.map(|le| ("le", le))).enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(name);
            out.push_str("=\"");
            escape_label_value(out, v);
            out.push('"');
        }
        out.push('}');
    }
    let _ = writeln!(out, " {value}");
}

/// One histogram series: a `_bucket` per bound then `+Inf`, `_sum`, `_count`.
fn write_histogram(out: &mut String, name: &str, labels: &[(&str, &str)], value: &HistogramValue) {
    for (le, n) in &value.buckets {
        write_sample(out, name, "_bucket", labels, Some(&le.to_string()), n);
    }
    write_sample(out, name, "_bucket", labels, Some("+Inf"), value.count);
    write_sample(out, name, "_sum", labels, None, value.sum);
    write_sample(out, name, "_count", labels, None, value.count);
}

/// `# HELP` text escapes backslash and line feed.
pub(crate) fn escape_help(out: &mut String, help: &str) {
    for c in help.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
}

/// A label value escapes backslash, double quote and line feed.
fn escape_label_value(out: &mut String, v: &str) {
    for c in v.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{label_values, Dim, Family, Labels};

    #[derive(Clone, Copy)]
    enum Class {
        Normal,
        Emergency,
    }

    impl Class {
        const ALL: [Class; 2] = [Class::Normal, Class::Emergency];
        const fn label(self) -> &'static str {
            match self {
                Class::Normal => "normal",
                Class::Emergency => "emergency",
            }
        }
    }

    const CLASS_VALUES: [&str; 2] = label_values!(Class::ALL, Class::label);
    const CLASS: Dim = Dim::new("class", &CLASS_VALUES);

    #[test]
    fn a_scalar_renders_help_type_and_one_sample() {
        const F: Family = Family::counter("x_total", Labels::None, "things counted");
        let mut s = String::new();
        F.render_value(&mut s, 7u64);
        assert_eq!(s, "# HELP x_total things counted\n# TYPE x_total counter\nx_total 7\n");
    }

    #[test]
    fn a_gauge_renders_a_float_as_display_does() {
        const F: Family = Family::gauge("level", Labels::None, "a level");
        let mut s = String::new();
        F.render_value(&mut s, 0.25f64);
        assert_eq!(s, "# HELP level a level\n# TYPE level gauge\nlevel 0.25\n");
    }

    #[test]
    fn every_label_set_is_written_with_its_value() {
        const F: Family = Family::counter("c_total", Labels::Product(&[CLASS]), "by class");
        let counts = [0u64, 3];
        let text = F.text(|s| counts[s.at(0)]);
        assert_eq!(
            text,
            "# HELP c_total by class\n# TYPE c_total counter\n\
             c_total{class=\"normal\"} 0\nc_total{class=\"emergency\"} 3\n"
        );
    }

    #[test]
    fn a_union_writes_each_block_with_its_own_labels() {
        const F: Family = Family::counter(
            "n_total",
            Labels::Union(&[
                &[Dim::new("outcome", &["accepted"]), CLASS],
                &[Dim::new("outcome", &["rejected"]), Dim::new("reason", &["full"]), CLASS],
            ]),
            "outcomes",
        );
        let text = F.text(|s| s.block() * 10 + s.at(s.labels().count() - 1));
        assert_eq!(
            text,
            "# HELP n_total outcomes\n# TYPE n_total counter\n\
             n_total{outcome=\"accepted\",class=\"normal\"} 0\n\
             n_total{outcome=\"accepted\",class=\"emergency\"} 1\n\
             n_total{outcome=\"rejected\",reason=\"full\",class=\"normal\"} 10\n\
             n_total{outcome=\"rejected\",reason=\"full\",class=\"emergency\"} 11\n"
        );
    }

    #[test]
    fn help_and_label_values_are_escaped() {
        const F: Family = Family::gauge(
            "e",
            Labels::Product(&[Dim::new("v", &["a\"b\\c\nd"])]),
            "line\\one\nline two",
        );
        let text = F.text(|_| 1);
        assert_eq!(
            text,
            "# HELP e line\\\\one\\nline two\n# TYPE e gauge\ne{v=\"a\\\"b\\\\c\\nd\"} 1\n"
        );
    }

    #[test]
    #[should_panic(expected = "declares labels")]
    fn a_labelled_family_refuses_a_single_value() {
        const F: Family = Family::counter("c_total", Labels::Product(&[CLASS]), "by class");
        F.render_value(&mut String::new(), 1);
    }
}
