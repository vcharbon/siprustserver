//! The call graph of one crate target, resolved by path: a path expression
//! naming a function of the crate (`f`, `m::f`, `Type::f`, `Self::f`,
//! `f` passed as a value) and a method called on `self` are its edges. A
//! method called on any other receiver, a call through `dyn` or a function
//! pointer, and a macro's own body are not read.

use std::collections::{BTreeMap, BTreeSet};

use proc_macro2::Span;
use syn::visit::{self, Visit};
use syn::{Block, Expr, FnArg, ImplItem, Item, Signature, TraitItem, Type};

use crate::cfg::test_only;
use crate::macros;
use crate::modules::{attrs_of, Module};
use crate::scope::{Crate, FnKey, Res, Scope, TypeKey};

/// The crate whose `Depth` guards a recursion (ADR-0057 §5).
pub const GUARD_CRATE: &str = "bounds";

/// One function of the graph.
pub struct Node {
    pub file: String,
    pub span: Span,
    /// It takes a `bounds::Depth` and calls `descend` on one.
    pub guarded: bool,
    pub calls: BTreeSet<FnKey>,
}

/// The functions of the target made of `modules`, by key.
pub fn build(modules: &[Module]) -> BTreeMap<FnKey, Node> {
    let flat: Vec<(&Module, Vec<&Item>)> =
        modules.iter().map(|m| (m, flat_items(&m.items))).collect();
    let mut krate = Crate::default();
    for (m, items) in &flat {
        let scope = krate.scopes.entry(m.path.clone()).or_insert_with(Scope::default);
        scope.mods.extend(m.children.iter().cloned());
        for item in items {
            scope.add(item);
        }
    }
    let bodies: Vec<(&Module, Vec<Body>)> = flat
        .iter()
        .map(|(m, items)| (*m, items.iter().flat_map(|i| bodies(&krate, &m.path, i)).collect()))
        .collect();
    for (_, list) in &bodies {
        for b in list {
            if let FnKey::Method(ty, via, name) = &b.key {
                krate.methods.entry(ty.clone()).or_default().insert((name.clone(), via.clone()));
            }
        }
    }
    let mut nodes = BTreeMap::new();
    for (m, list) in &bodies {
        let scope = krate.scopes.get(&m.path);
        for b in list {
            nodes.insert(b.key.clone(), node(&krate, m, scope, b.sig, b.block, b.self_ty.as_ref()));
        }
    }
    nodes
}

/// A function body of the target: its key, signature and block, and the
/// type `Self` names in it.
struct Body<'a> {
    key: FnKey,
    sig: &'a Signature,
    block: &'a Block,
    self_ty: Option<TypeKey>,
}

/// The function bodies `item`, declared in `module`, holds.
fn bodies<'a>(krate: &Crate, module: &[String], item: &'a Item) -> Vec<Body<'a>> {
    match item {
        Item::Fn(f) => vec![Body {
            key: FnKey::Free(module.to_vec(), f.sig.ident.to_string()),
            sig: &f.sig,
            block: &f.block,
            self_ty: None,
        }],
        Item::Impl(imp) => {
            let ty = type_key(krate, module, &imp.self_ty);
            let via = imp
                .trait_
                .as_ref()
                .and_then(|(_, path, _)| path.segments.last())
                .map(|s| s.ident.to_string());
            imp.items
                .iter()
                .filter_map(|i| match i {
                    ImplItem::Fn(f) if !test_only(&f.attrs) => Some(Body {
                        key: FnKey::Method(ty.clone(), via.clone(), f.sig.ident.to_string()),
                        sig: &f.sig,
                        block: &f.block,
                        self_ty: Some(ty.clone()),
                    }),
                    _ => None,
                })
                .collect()
        }
        Item::Trait(t) => {
            let ty = (module.to_vec(), t.ident.to_string());
            t.items
                .iter()
                .filter_map(|i| match i {
                    TraitItem::Fn(f) if !test_only(&f.attrs) => Some(Body {
                        key: FnKey::Method(ty.clone(), None, f.sig.ident.to_string()),
                        sig: &f.sig,
                        block: f.default.as_ref()?,
                        self_ty: Some(ty.clone()),
                    }),
                    _ => None,
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

/// `items` and the items declared inside their function bodies, the
/// test-only ones left out.
fn flat_items(items: &[Item]) -> Vec<&Item> {
    struct Nested<'a> {
        found: Vec<&'a Item>,
    }
    impl<'a> Visit<'a> for Nested<'a> {
        fn visit_item(&mut self, item: &'a Item) {
            if !test_only(attrs_of(item)) {
                self.found.push(item);
                visit::visit_item(self, item);
            }
        }
    }
    let mut v = Nested { found: Vec::new() };
    for item in items {
        v.visit_item(item);
    }
    v.found
}

fn type_key(krate: &Crate, module: &[String], ty: &Type) -> TypeKey {
    let ty = match ty {
        Type::Reference(r) => &*r.elem,
        Type::Paren(p) => &*p.elem,
        other => other,
    };
    let Type::Path(p) = ty else {
        return (vec!["<extern>".to_string()], quote_type(ty));
    };
    let segs: Vec<String> = p.path.segments.iter().map(|s| s.ident.to_string()).collect();
    match krate.resolve(module, &segs, None) {
        Some(Res::Type(key)) => key,
        _ => (vec!["<extern>".to_string()], segs.join("::")),
    }
}

fn quote_type(ty: &Type) -> String {
    use quote::ToTokens;
    ty.to_token_stream().to_string()
}

fn node(
    krate: &Crate,
    module: &Module,
    scope: Option<&Scope>,
    sig: &Signature,
    body: &Block,
    self_ty: Option<&TypeKey>,
) -> Node {
    let mut bound = Bindings(BTreeSet::new());
    sig.inputs.iter().for_each(|arg| bound.visit_fn_arg(arg));
    bound.visit_block(body);
    let mut calls = Calls {
        krate,
        module: &module.path,
        self_ty,
        bound: bound.0,
        found: BTreeSet::new(),
        descends: false,
    };
    calls.visit_block(body);
    let guarded = calls.descends && sig.inputs.iter().any(|arg| takes_depth(arg, scope));
    Node { file: module.file.clone(), span: sig.ident.span(), guarded, calls: calls.found }
}

/// Whether `arg` is a `bounds::Depth`, by path or by import.
fn takes_depth(arg: &FnArg, scope: Option<&Scope>) -> bool {
    let FnArg::Typed(pt) = arg else {
        return false;
    };
    let mut ty = &*pt.ty;
    while let Type::Reference(r) = ty {
        ty = &r.elem;
    }
    let Type::Path(p) = ty else {
        return false;
    };
    let segs: Vec<String> = p.path.segments.iter().map(|s| s.ident.to_string()).collect();
    match segs.as_slice() {
        [krate, name] => krate == GUARD_CRATE && name == "Depth",
        [name] => scope
            .and_then(|s| s.uses.get(name))
            .is_some_and(|path| path.first().is_some_and(|c| c == GUARD_CRATE) && name == "Depth"),
        _ => false,
    }
}

/// The names a function binds (parameters, `let`, closure and match
/// bindings): a one-segment path naming one of them is no call.
struct Bindings(BTreeSet<String>);

impl<'ast> Visit<'ast> for Bindings {
    fn visit_item(&mut self, _: &'ast Item) {}

    fn visit_pat_ident(&mut self, p: &'ast syn::PatIdent) {
        self.0.insert(p.ident.to_string());
        visit::visit_pat_ident(self, p);
    }
}

struct Calls<'a> {
    krate: &'a Crate,
    module: &'a [String],
    self_ty: Option<&'a TypeKey>,
    bound: BTreeSet<String>,
    found: BTreeSet<FnKey>,
    descends: bool,
}

impl<'ast> Visit<'ast> for Calls<'_> {
    fn visit_item(&mut self, _: &'ast Item) {}

    fn visit_expr(&mut self, expr: &'ast Expr) {
        match expr {
            Expr::Path(p) if p.qself.is_none() => {
                let segs: Vec<String> =
                    p.path.segments.iter().map(|s| s.ident.to_string()).collect();
                let local = matches!(segs.as_slice(), [one] if self.bound.contains(one));
                if local {
                    return visit::visit_expr(self, expr);
                }
                if let Some(Res::Fn(key)) = self.krate.resolve(self.module, &segs, self.self_ty) {
                    self.found.insert(key);
                }
            }
            Expr::MethodCall(m) => {
                if m.method == "descend" {
                    self.descends = true;
                }
                let on_self = matches!(&*m.receiver, Expr::Path(p) if p.path.is_ident("self"));
                if let (true, Some(ty)) = (on_self, self.self_ty) {
                    if let Some(key) = self.krate.method(ty, &m.method.to_string()) {
                        self.found.insert(key);
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
