//! The allow-list: metric names a reader spells under a catalogue's
//! namespace that are not ours (another exporter's, a script's own), each
//! scoped to the files it is allowed in, and the labels a scrape adds to
//! every series.
//!
//! One entry per line, `#` starts a comment: `<path>: <name>` allows that
//! name in that file, or under that directory (with or without a trailing
//! `/`); `<path>: <prefix>*` every name starting with `<prefix>` there;
//! `label:<name>` a selector label on any family. A path is relative to the
//! allow-list file's directory, a leading `./` ignored.

use std::cell::Cell;
use std::path::{Path, PathBuf};

/// One scoped name entry.
#[derive(Debug, Clone)]
struct Entry {
    /// The allow-list file and line that state it.
    source: (PathBuf, usize),
    scope: PathBuf,
    dir: bool,
    name: String,
    used: Cell<bool>,
}

/// A parsed allow-list.
#[derive(Debug, Clone, Default)]
pub struct Allow {
    names: Vec<Entry>,
    labels: Vec<String>,
}

impl Allow {
    /// Parse the allow-list `file` of text `text`, adding to this list.
    /// `Err` names a line that is no entry.
    pub fn add(&mut self, file: &Path, text: &str) -> Result<(), String> {
        let base = file.parent().unwrap_or(Path::new(""));
        for (n, line) in text.lines().enumerate() {
            let entry = line.split('#').next().unwrap_or("").trim();
            if entry.is_empty() {
                continue;
            }
            if let Some(label) = entry.strip_prefix("label:") {
                self.labels.push(label.trim().to_owned());
            } else if let Some((path, name)) = entry.split_once(": ") {
                let path = path.trim();
                let path = path.strip_prefix("./").unwrap_or(path);
                let scope = base.join(path.trim_end_matches('/'));
                let dir = path.ends_with('/') || scope.is_dir();
                self.names.push(Entry {
                    source: (file.to_owned(), n + 1),
                    scope,
                    dir,
                    name: name.trim().to_owned(),
                    used: Cell::new(false),
                });
            } else {
                return Err(format!("line {}: {entry:?} is not `<path>: <name>`", n + 1));
            }
        }
        Ok(())
    }

    /// Whether `name` is allowed in `file` though no catalogue declares it;
    /// every entry that allows it is marked used.
    pub fn name(&self, file: &Path, name: &str) -> bool {
        let mut allowed = false;
        for e in &self.names {
            let scoped = if e.dir { file.starts_with(&e.scope) } else { file == e.scope };
            let named = match e.name.strip_suffix('*') {
                Some(prefix) => name.starts_with(prefix),
                None => name == e.name,
            };
            if scoped && named {
                e.used.set(true);
                allowed = true;
            }
        }
        allowed
    }

    /// Whether `label` is allowed on any family.
    pub fn label(&self, label: &str) -> bool {
        self.labels.iter().any(|l| l == label)
    }

    /// The name entries no check has used: their name no longer occurs in
    /// their scope. Meaningful after a check of every file the allow-lists
    /// scope; a check of a subset reports the entries outside it too.
    pub fn unused(&self) -> impl Iterator<Item = (&Path, usize, &str)> {
        self.names
            .iter()
            .filter(|e| !e.used.get())
            .map(|e| (e.source.0.as_path(), e.source.1, e.name.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_allowed_only_where_its_entry_scopes_it() {
        let mut allow = Allow::default();
        let text = "./a/f.py: x_key\nd/: y_*\nlabel:pod\n";
        allow.add(Path::new("/r/metric-names.allow"), text).unwrap();
        assert!(allow.name(Path::new("/r/a/f.py"), "x_key"));
        assert!(!allow.name(Path::new("/r/a/g.py"), "x_key"));
        assert!(allow.name(Path::new("/r/d/e/h.json"), "y_series"));
        assert!(!allow.name(Path::new("/r/a/f.py"), "y_series"));
        assert!(allow.label("pod"));
        assert!(allow.add(Path::new("/r/x.allow"), "unscoped_name\n").is_err());
    }

    #[test]
    fn an_entry_no_check_used_is_reported() {
        let mut allow = Allow::default();
        allow.add(Path::new("/r/metric-names.allow"), "a.py: x_key\na.py: y_key\n").unwrap();
        allow.name(Path::new("/r/a.py"), "x_key");
        let unused: Vec<_> = allow.unused().map(|(_, line, name)| (line, name)).collect();
        assert_eq!(unused, [(2, "y_key")]);
    }

    /// An entry inside a wider one's scope is used too: overlap reports no
    /// stale entry.
    #[test]
    fn every_matching_entry_is_marked_used() {
        let mut allow = Allow::default();
        allow.add(Path::new("/r/metric-names.allow"), "d/: x_*\nd/f.py: x_key\n").unwrap();
        allow.name(Path::new("/r/d/f.py"), "x_key");
        assert_eq!(allow.unused().count(), 0);
    }
}
