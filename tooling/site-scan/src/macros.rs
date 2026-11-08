//! The expressions a macro call's arguments hold, where they parse as a
//! comma-separated list of expressions (`format!`, `vec![a, b]`, `matches!`'s
//! scrutinee, `write!`); a macro whose arguments do not is not read.

use syn::punctuated::Punctuated;
use syn::{Expr, Macro, Token};

/// The argument expressions of `mac`, or none.
pub fn args(mac: &Macro) -> Vec<Expr> {
    mac.parse_body_with(Punctuated::<Expr, Token![,]>::parse_terminated)
        .map(|list| list.into_iter().collect())
        .unwrap_or_default()
}
