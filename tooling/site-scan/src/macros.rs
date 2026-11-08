//! The expressions a macro call's arguments hold: each piece between its
//! top-level commas (and semicolons) read as an expression, a `name:` or
//! `name =` in front of it left out, and a `pattern if guard` read as its
//! guard. A piece that does not read as one is skipped; a macro's own body is
//! not read.

use proc_macro2::{Spacing, TokenStream, TokenTree};
use syn::{Expr, Macro};

/// The argument expressions of `mac`.
pub fn args(mac: &Macro) -> Vec<Expr> {
    let mut pieces: Vec<Vec<TokenTree>> = vec![Vec::new()];
    for tt in mac.tokens.clone() {
        match &tt {
            TokenTree::Punct(p) if matches!(p.as_char(), ',' | ';') => pieces.push(Vec::new()),
            _ => {
                if let Some(last) = pieces.last_mut() {
                    last.push(tt);
                }
            }
        }
    }
    pieces.into_iter().flat_map(read).collect()
}

/// The expressions one piece holds.
fn read(piece: Vec<TokenTree>) -> Vec<Expr> {
    let named = matches!(piece.as_slice(), [TokenTree::Ident(_), TokenTree::Punct(p), ..]
        if matches!(p.as_char(), ':' | '=') && p.spacing() == Spacing::Alone);
    let piece = if named { piece.get(2..).unwrap_or_default().to_vec() } else { piece };
    if let Ok(expr) = syn::parse2::<Expr>(piece.iter().cloned().collect()) {
        return vec![expr];
    }
    let guard = piece.iter().position(|t| matches!(t, TokenTree::Ident(i) if i == "if"));
    guard
        .and_then(|at| piece.get(at + 1..))
        .and_then(|rest| syn::parse2::<Expr>(rest.iter().cloned().collect::<TokenStream>()).ok())
        .into_iter()
        .collect()
}
