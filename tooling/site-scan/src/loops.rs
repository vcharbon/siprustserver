//! The loop sites of a module (ADR-0057 §5): `loop`, `while`, a hand-written
//! `Iterator::next` or `next_back`, an infinite iterator's constructor, and a
//! range or `iter::repeat_n` whose end is neither a constant nor a
//! collection's `len()` (an immutable local bound to one of those is one too;
//! a wide integer type's `MAX` is none), where it is iterated: in a `for`, as a
//! method's receiver or as an iterator argument, itself or through the local
//! it is bound to. A macro's own body is not read.

use std::collections::{BTreeMap, BTreeSet};

use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{BinOp, Expr, ExprRange, ImplItem, Item, ItemImpl, RangeLimits};

use crate::cfg::test_only;
use crate::macros;
use crate::modules::Module;
use crate::scope::Scope;
use crate::site::{Set, Site};

const INFINITE: [&str; 4] = ["repeat", "repeat_with", "successors", "from_fn"];
/// Range methods that read the range without iterating it.
const READS: [&str; 7] = ["contains", "is_empty", "start", "end", "clone", "into_inner", "len"];
/// Methods that take an iterator argument and iterate it.
const TAKES_ITER: [&str; 3] = ["extend", "chain", "eq"];
/// The integer types whose `MAX` no loop reaches.
const WIDE: [&str; 8] = ["u32", "u64", "u128", "usize", "i32", "i64", "i128", "isize"];

/// The loop sites of `module`, whose names `scope` holds.
pub fn sites(module: &Module, scope: &Scope) -> Vec<Site> {
    let mut v = Loops {
        file: &module.file,
        scope,
        sites: Vec::new(),
        locals: BTreeSet::new(),
        ranges: BTreeMap::new(),
    };
    for item in &module.items {
        v.visit_item(item);
    }
    v.sites
}

struct Loops<'a> {
    file: &'a str,
    scope: &'a Scope,
    sites: Vec<Site>,
    /// The immutable locals of the function being read bound to a bounded
    /// end.
    locals: BTreeSet<String>,
    /// The locals bound to a range whose end is not bounded, and its span.
    ranges: BTreeMap<String, proc_macro2::Span>,
}

impl Loops<'_> {
    fn site(&mut self, span: proc_macro2::Span, message: &str) {
        self.sites.push(Site::at(self.file, span, Set::Loop, message.to_string()));
    }

    fn bounded_range(&self, range: &ExprRange) -> bool {
        match (&range.end, &range.limits) {
            (Some(end), _) => bounded(end, &self.locals),
            (None, RangeLimits::HalfOpen(_)) => false,
            (None, RangeLimits::Closed(_)) => true,
        }
    }

    /// `expr` iterated: a site when it is, or names, an unbounded range.
    fn iterated(&mut self, expr: &Expr) {
        match peel(expr) {
            Expr::Range(r) => self.range(r),
            Expr::Path(p) => {
                let held =
                    p.path.get_ident().and_then(|i| self.ranges.get(&i.to_string()).copied());
                if let Some(span) = held {
                    self.site(
                        span,
                        "a range iterated whose end is neither a constant nor a `len()`",
                    );
                }
            }
            _ => {}
        }
    }

    fn range(&mut self, range: &ExprRange) {
        if !self.bounded_range(range) {
            self.site(
                range.span(),
                "a range iterated whose end is neither a constant nor a `len()`",
            );
        }
    }

    /// Whether `path`, written here, is one of `names` of `std::iter`.
    fn from_iter(&self, path: &syn::Path, names: &[&str]) -> bool {
        let mut segs: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
        if let Some(first) = segs.first() {
            if let Some(full) = self.scope.uses.get(first) {
                segs.splice(0..1, full.iter().cloned());
            } else if segs.len() == 1
                && self.scope.globs.iter().any(|g| g.last().is_some_and(|l| l == "iter"))
            {
                segs.insert(0, "iter".to_string());
            }
        }
        match segs.as_slice() {
            [.., iter, last] => iter == "iter" && names.contains(&last.as_str()),
            _ => false,
        }
    }
}

impl<'ast> Visit<'ast> for Loops<'_> {
    fn visit_signature(&mut self, sig: &'ast syn::Signature) {
        self.locals.clear();
        self.ranges.clear();
        visit::visit_signature(self, sig);
    }

    fn visit_pat_ident(&mut self, p: &'ast syn::PatIdent) {
        self.locals.remove(&p.ident.to_string());
        self.ranges.remove(&p.ident.to_string());
        visit::visit_pat_ident(self, p);
    }

    fn visit_local(&mut self, local: &'ast syn::Local) {
        visit::visit_local(self, local);
        let held = match (&local.pat, local.init.as_ref().map(|i| peel(&i.expr))) {
            (syn::Pat::Ident(p), Some(Expr::Range(r))) if !self.bounded_range(r) => Some((p, r)),
            _ => None,
        };
        if let Some((p, r)) = held {
            self.ranges.insert(p.ident.to_string(), r.span());
        }
        if let syn::Pat::Ident(p) = &local.pat {
            let name = p.ident.to_string();
            let init = local.init.as_ref().filter(|i| i.diverge.is_none());
            if p.mutability.is_none() && init.is_some_and(|i| bounded(&i.expr, &self.locals)) {
                self.locals.insert(name);
            } else {
                self.locals.remove(&name);
            }
        }
    }

    fn visit_item(&mut self, item: &'ast Item) {
        if !test_only(crate::modules::attrs_of(item)) {
            visit::visit_item(self, item);
        }
    }

    fn visit_item_impl(&mut self, imp: &'ast ItemImpl) {
        let trait_ = imp.trait_.as_ref().and_then(|(_, path, _)| path.segments.last());
        let iterator = trait_.is_some_and(|s| s.ident == "Iterator");
        let double_ended = trait_.is_some_and(|s| s.ident == "DoubleEndedIterator");
        for item in &imp.items {
            if let ImplItem::Fn(f) = item {
                if test_only(&f.attrs) {
                    continue;
                }
                if iterator && f.sig.ident == "next" {
                    self.site(f.sig.ident.span(), "`Iterator::next` written by hand");
                }
                if double_ended && f.sig.ident == "next_back" {
                    self.site(
                        f.sig.ident.span(),
                        "`DoubleEndedIterator::next_back` written by hand",
                    );
                }
                self.visit_impl_item_fn(f);
            } else {
                self.visit_impl_item(item);
            }
        }
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        match expr {
            Expr::Loop(l) => self.site(l.loop_token.span, "`loop`"),
            Expr::While(w) => self.site(w.while_token.span, "`while`"),
            Expr::ForLoop(f) => self.iterated(&f.expr),
            Expr::MethodCall(m) => {
                if m.method == "cycle" && m.args.is_empty() {
                    self.site(m.method.span(), "`cycle()`, an infinite iterator");
                }
                let method = m.method.to_string();
                let capped = method == "take" && m.args.iter().all(|a| bounded(a, &self.locals));
                if !READS.contains(&method.as_str()) && !capped && method != "zip" {
                    self.iterated(&m.receiver);
                }
                if TAKES_ITER.contains(&method.as_str()) {
                    m.args.iter().for_each(|arg| self.iterated(arg));
                }
            }
            Expr::Call(c) => {
                if let Expr::Path(p) = &*c.func {
                    if self.from_iter(&p.path, &INFINITE) {
                        self.site(p.span(), "an infinite iterator's constructor");
                    }
                    let count = c.args.iter().nth(1);
                    let unbounded = count.is_some_and(|n| !bounded(n, &self.locals));
                    if unbounded && self.from_iter(&p.path, &["repeat_n"]) {
                        self.site(
                            p.span(),
                            "`repeat_n` to a count neither a constant nor a `len()`",
                        );
                    }
                }
            }
            _ => {}
        }
        visit::visit_expr(self, expr);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        for expr in macros::args(mac) {
            self.visit_expr(&expr);
        }
    }
}

fn peel(expr: &Expr) -> &Expr {
    let mut e = expr;
    loop {
        e = match e {
            Expr::Paren(p) => &p.expr,
            Expr::Reference(r) => &r.expr,
            Expr::Group(g) => &g.expr,
            _ => return e,
        };
    }
}

/// Whether `end` is a constant, a collection's `len()`, one of `locals`, or
/// arithmetic of those.
fn bounded(end: &Expr, locals: &BTreeSet<String>) -> bool {
    let b = |e: &Expr| bounded(e, locals);
    match peel(end) {
        Expr::Lit(_) => true,
        Expr::Path(p) if integer_max(&p.path) => false,
        Expr::Path(p) => p.path.segments.last().is_some_and(|s| {
            let name = s.ident.to_string();
            let constant = name.chars().any(|c| c.is_ascii_uppercase())
                && !name.chars().any(|c| c.is_ascii_lowercase());
            constant || (p.path.segments.len() == 1 && locals.contains(&name))
        }),
        Expr::Cast(c) => b(&c.expr),
        Expr::Binary(bin) => {
            matches!(bin.op, BinOp::Add(_) | BinOp::Sub(_) | BinOp::Mul(_) | BinOp::Div(_))
                && b(&bin.left)
                && b(&bin.right)
        }
        Expr::MethodCall(m) => match m.method.to_string().as_str() {
            "len" => m.args.is_empty(),
            "min" => b(&m.receiver) || m.args.iter().any(b),
            "saturating_sub" | "wrapping_sub" => b(&m.receiver),
            "saturating_add" | "saturating_mul" => b(&m.receiver) && m.args.iter().all(b),
            _ => false,
        },
        _ => false,
    }
}

/// Whether `path` is a wide integer type's `MAX`: a constant no loop reaches.
fn integer_max(path: &syn::Path) -> bool {
    let segs: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
    matches!(segs.as_slice(), [.., ty, max] if max == "MAX" && WIDE.contains(&ty.as_str()))
}
