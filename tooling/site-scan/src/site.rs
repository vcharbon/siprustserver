//! A counted site and its JSON line.

use proc_macro2::Span;
use serde_json::{json, Value};

/// The set a site belongs to.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Set {
    Loop,
    Recursion,
}

impl Set {
    pub fn name(self) -> &'static str {
        match self {
            Set::Loop => "loop",
            Set::Recursion => "recursion",
        }
    }
}

/// One site: the file, the line and columns of the token that names it, and
/// what it is.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Site {
    pub file: String,
    pub line: usize,
    pub column: usize,
    pub column_end: usize,
    pub set: Set,
    pub message: String,
}

impl Site {
    /// The site at `span` of `file`.
    pub fn at(file: &str, span: Span, set: Set, message: String) -> Site {
        let (start, end) = (span.start(), span.end());
        let column_end = if end.line == start.line { end.column + 1 } else { start.column + 2 };
        Site {
            file: file.to_string(),
            line: start.line,
            column: start.column + 1,
            column_end,
            set,
            message,
        }
    }

    /// The JSON line of the site in crate `krate`.
    pub fn json(&self, krate: &str) -> Value {
        json!({
            "reason": "site-scan",
            "crate": krate,
            "set": self.set.name(),
            "file_name": self.file,
            "line_start": self.line,
            "line_end": self.line,
            "column_start": self.column,
            "column_end": self.column_end,
            "message": self.message,
        })
    }
}
