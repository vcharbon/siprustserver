//! The call graph of one crate target, resolved by path: a path expression
//! naming a function of the crate (`f`, `m::f`, `Type::f`, `Self::f`,
//! `Trait::f`, `<T as Trait>::f`, `f` passed as a value) and a method called
//! on `self` are its edges; a call through a trait reaches its default and
//! every implementor's own. A name a local binding shadows is no call. A
//! method called on any other receiver, a call through `dyn` or a function
//! pointer, and a macro's own body are not read. A function guards its
//! recursion when it takes a `bounds::Depth` and calls `descend` on it.

use std::collections::{BTreeMap, BTreeSet};

use proc_macro2::Span;
use syn::visit::{self, Visit};
use syn::{Block, Expr, FnArg, ImplItem, Item, Pat, Signature, TraitItem, Type};

use crate::cfg::test_only;
use crate::macros;
use crate::modules::{attrs_of, Module};
use crate::scope::{body_segment, Crate, FnKey, Res, Scope, TypeKey};

/// The crate whose `Depth` guards a recursion (ADR-0057 §5).
pub const GUARD_CRATE: &str = "bounds";

/// One function of the graph.
pub struct Node {
    pub file: String,
    pub span: Span,
    /// It takes a `bounds::Depth` and calls `descend` on it.
    pub guarded: bool,
    pub calls: BTreeSet<FnKey>,
}

/// A scope of the target: its path, the file holding it, and its items.
struct Region<'a> {
    path: Vec<String>,
    file: &'a str,
    items: Vec<&'a Item>,
    children: &'a [String],
}

/// Who owns a function body.
enum Owner<'a> {
    Free,
    Impl { self_ty: &'a Type, trait_: Option<&'a syn::Path> },
    Trait { name: String },
}

/// One function body: its owner, signature and block.
struct Body<'a> {
    owner: Owner<'a>,
    sig: &'a Signature,
    block: &'a Block,
}

impl Body<'_> {
    fn segment(&self) -> String {
        let name = self.sig.ident.to_string();
        match &self.owner {
            Owner::Free => body_segment(None, &name),
            Owner::Impl { self_ty, .. } => body_segment(Some(&type_name(self_ty)), &name),
            Owner::Trait { name: t } => body_segment(Some(t), &name),
        }
    }
}

/// The functions of the target made of `modules`, by key.
pub fn build(modules: &[Module]) -> BTreeMap<FnKey, Node> {
    let regions = regions(modules);
    let mut krate = Crate::default();
    for r in &regions {
        let scope = krate.scopes.entry(r.path.clone()).or_insert_with(Scope::default);
        scope.mods.extend(r.children.iter().cloned());
        r.items.iter().for_each(|i| scope.add(i));
    }
    for r in &regions {
        for item in &r.items {
            match item {
                Item::Trait(t) => {
                    krate.traits.insert((r.path.clone(), t.ident.to_string()));
                }
                Item::Impl(imp) => {
                    if let Some((_, path, _)) = &imp.trait_ {
                        let ty = type_key(&krate, &r.path, &imp.self_ty);
                        let tr = path_key(&krate, &r.path, path);
                        krate.traits_of.entry(ty.clone()).or_default().insert(tr.clone());
                        krate.implementors.entry(tr).or_default().insert(ty);
                    }
                }
                _ => {}
            }
        }
    }
    let mut keyed: Vec<(&Region, Body, FnKey, Option<TypeKey>)> = Vec::new();
    for r in &regions {
        for b in r.items.iter().copied().flat_map(bodies) {
            let (key, self_ty) = key_of(&krate, &r.path, &b);
            keyed.push((r, b, key, self_ty));
        }
    }
    for (_, _, key, _) in &keyed {
        if let FnKey::Method(ty, via, name) = key {
            krate.methods.entry(ty.clone()).or_default().insert((name.clone(), via.clone()));
        }
    }
    let mut nodes = BTreeMap::new();
    for (r, b, key, self_ty) in &keyed {
        let mut scope = r.path.clone();
        scope.push(b.segment());
        nodes.insert(key.clone(), node(&krate, r.file, &scope, b, self_ty.as_ref()));
    }
    nodes
}

/// The modules of the target and the bodies holding items of their own.
fn regions(modules: &[Module]) -> Vec<Region<'_>> {
    let mut out: Vec<Region> = modules
        .iter()
        .map(|m| Region {
            path: m.path.clone(),
            file: &m.file,
            items: m.items.iter().filter(|i| !test_only(attrs_of(i))).collect(),
            children: &m.children,
        })
        .collect();
    let mut at = 0;
    while let Some(r) = out.get(at) {
        let mut nested = Vec::new();
        for item in r.items.iter().copied() {
            for b in bodies(item) {
                let items = items_in(b.block);
                if !items.is_empty() {
                    let mut path = r.path.clone();
                    path.push(b.segment());
                    nested.push(Region { path, file: r.file, items, children: &[] });
                }
            }
        }
        out.extend(nested);
        at += 1;
    }
    out
}

/// The items `block` declares, nested blocks and closures included, the
/// bodies of the functions among them left to their own scope.
fn items_in(block: &Block) -> Vec<&Item> {
    struct Nested<'a> {
        found: Vec<&'a Item>,
    }
    impl<'a> Visit<'a> for Nested<'a> {
        fn visit_item(&mut self, item: &'a Item) {
            if !test_only(attrs_of(item)) {
                self.found.push(item);
            }
        }
    }
    let mut v = Nested { found: Vec::new() };
    v.visit_block(block);
    v.found
}

/// The function bodies `item` holds.
fn bodies(item: &Item) -> Vec<Body<'_>> {
    match item {
        Item::Fn(f) => vec![Body { owner: Owner::Free, sig: &f.sig, block: &f.block }],
        Item::Impl(imp) => imp
            .items
            .iter()
            .filter_map(|i| match i {
                ImplItem::Fn(f) if !test_only(&f.attrs) => Some(Body {
                    owner: Owner::Impl {
                        self_ty: &imp.self_ty,
                        trait_: imp.trait_.as_ref().map(|(_, p, _)| p),
                    },
                    sig: &f.sig,
                    block: &f.block,
                }),
                _ => None,
            })
            .collect(),
        Item::Trait(t) => t
            .items
            .iter()
            .filter_map(|i| match i {
                TraitItem::Fn(f) if !test_only(&f.attrs) => Some(Body {
                    owner: Owner::Trait { name: t.ident.to_string() },
                    sig: &f.sig,
                    block: f.default.as_ref()?,
                }),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The key of `b`, declared in `scope`, and the type `Self` names in it.
fn key_of(krate: &Crate, scope: &[String], b: &Body) -> (FnKey, Option<TypeKey>) {
    let name = b.sig.ident.to_string();
    match &b.owner {
        Owner::Free => (FnKey::Free(scope.to_vec(), name), None),
        Owner::Impl { self_ty, trait_ } => {
            let ty = type_key(krate, scope, self_ty);
            let via = trait_.map(|p| path_key(krate, scope, p));
            (FnKey::Method(ty.clone(), via, name), Some(ty))
        }
        Owner::Trait { name: t } => {
            let ty = (scope.to_vec(), t.clone());
            (FnKey::Method(ty.clone(), None, name), Some(ty))
        }
    }
}

fn segments(path: &syn::Path) -> Vec<String> {
    path.segments.iter().map(|s| s.ident.to_string()).collect()
}

fn type_name(ty: &Type) -> String {
    match peel_type(ty) {
        Type::Path(p) => segments(&p.path).join("::"),
        other => {
            use quote::ToTokens;
            other.to_token_stream().to_string()
        }
    }
}

fn peel_type(ty: &Type) -> &Type {
    let mut t = ty;
    loop {
        t = match t {
            Type::Reference(r) => &r.elem,
            Type::Paren(p) => &p.elem,
            Type::Group(g) => &g.elem,
            _ => return t,
        };
    }
}

/// The crate's type `ty` names in `scope`, or its `<extern>` key.
fn type_key(krate: &Crate, scope: &[String], ty: &Type) -> TypeKey {
    match peel_type(ty) {
        Type::Path(p) if p.qself.is_none() => path_key(krate, scope, &p.path),
        other => (vec!["<extern>".to_string()], type_name(other)),
    }
}

fn path_key(krate: &Crate, scope: &[String], path: &syn::Path) -> TypeKey {
    let segs = segments(path);
    match krate.resolve(scope, &segs, None) {
        Some(Res::Type(key)) => key,
        _ => (vec!["<extern>".to_string()], segs.join("::")),
    }
}

fn node(krate: &Crate, file: &str, scope: &[String], b: &Body, self_ty: Option<&TypeKey>) -> Node {
    let mut params = BTreeSet::new();
    let mut depths = BTreeSet::new();
    for arg in &b.sig.inputs {
        if let FnArg::Typed(pt) = arg {
            let names = bound_by(&pt.pat);
            if takes_depth(krate, scope, &pt.ty) {
                depths.extend(names.iter().cloned());
            }
            params.extend(names);
        }
    }
    let mut calls = Calls {
        krate,
        scope,
        self_ty,
        bound: vec![params],
        depths,
        found: BTreeSet::new(),
        descends: false,
    };
    calls.visit_block(b.block);
    Node {
        file: file.to_string(),
        span: b.sig.ident.span(),
        guarded: calls.descends,
        calls: calls.found,
    }
}

/// Whether `ty` is a `bounds::Depth`, by path or by import.
fn takes_depth(krate: &Crate, scope: &[String], ty: &Type) -> bool {
    let Type::Path(p) = peel_type(ty) else {
        return false;
    };
    match segments(&p.path).as_slice() {
        [c, name] => c == GUARD_CRATE && name == "Depth",
        [name] => {
            name == "Depth"
                && krate
                    .import(scope, name)
                    .is_some_and(|p| p.first().is_some_and(|c| c == GUARD_CRATE))
        }
        _ => false,
    }
}

/// The names `pat` binds.
fn bound_by(pat: &Pat) -> Vec<String> {
    struct Names(Vec<String>);
    impl<'a> Visit<'a> for Names {
        fn visit_pat_ident(&mut self, p: &'a syn::PatIdent) {
            self.0.push(p.ident.to_string());
            visit::visit_pat_ident(self, p);
        }
        fn visit_expr(&mut self, _: &'a Expr) {}
    }
    let mut v = Names(Vec::new());
    v.visit_pat(pat);
    v.0
}

/// `expr` without its parentheses and references, and how many derefs it
/// went through.
fn peel(expr: &Expr) -> (&Expr, usize) {
    let (mut e, mut derefs) = (expr, 0);
    loop {
        e = match e {
            Expr::Paren(p) => &p.expr,
            Expr::Group(g) => &g.expr,
            Expr::Reference(r) => &r.expr,
            Expr::Unary(u) if matches!(u.op, syn::UnOp::Deref(_)) => {
                derefs += 1;
                &u.expr
            }
            _ => return (e, derefs),
        };
    }
}

struct Calls<'a> {
    krate: &'a Crate,
    scope: &'a [String],
    self_ty: Option<&'a TypeKey>,
    /// The names bound in each enclosing lexical scope, innermost last.
    bound: Vec<BTreeSet<String>>,
    /// The parameters that are a `bounds::Depth`.
    depths: BTreeSet<String>,
    found: BTreeSet<FnKey>,
    descends: bool,
}

impl Calls<'_> {
    fn local(&self, name: &str) -> bool {
        self.bound.iter().any(|s| s.contains(name))
    }

    fn bind(&mut self, pat: &Pat) {
        if let Some(top) = self.bound.last_mut() {
            top.extend(bound_by(pat));
        }
    }

    fn scoped(&mut self, f: impl FnOnce(&mut Self)) {
        self.bound.push(BTreeSet::new());
        f(self);
        self.bound.pop();
    }

    /// The edges of `<qself as Trait>::m` (`position` segments name the trait).
    fn qualified(&mut self, qself: &syn::QSelf, path: &syn::Path) {
        let segs = segments(path);
        let Some(method) = segs.last() else { return };
        let ty = match peel_type(&qself.ty) {
            Type::Path(p) if p.path.is_ident("Self") => self.self_ty.cloned(),
            other => Some(type_key(self.krate, self.scope, other)),
        };
        let trait_path = segs.get(..qself.position).unwrap_or_default();
        let tr = match self.krate.resolve(self.scope, trait_path, self.self_ty) {
            Some(Res::Type(t)) => Some(t),
            _ => None,
        };
        let mut fns = match (&ty, &tr) {
            (Some(ty), Some(tr)) => self
                .krate
                .methods_named(ty, method)
                .into_iter()
                .filter(|k| matches!(k, FnKey::Method(t, via, _) if via.as_ref() == Some(tr) || t == tr))
                .collect(),
            (Some(ty), None) if qself.position == 0 => self.krate.methods_named(ty, method),
            _ => Vec::new(),
        };
        if fns.is_empty() {
            if let Some(tr) = &tr {
                fns = self.krate.methods_named(tr, method);
            }
        }
        self.found.extend(fns);
    }
}

impl<'ast> Visit<'ast> for Calls<'_> {
    fn visit_item(&mut self, _: &'ast Item) {}

    fn visit_block(&mut self, block: &'ast Block) {
        self.scoped(|v| visit::visit_block(v, block));
    }

    fn visit_local(&mut self, local: &'ast syn::Local) {
        if let Some(init) = &local.init {
            self.visit_expr(&init.expr);
            if let Some((_, diverge)) = &init.diverge {
                self.visit_expr(diverge);
            }
        }
        self.bind(&local.pat);
    }

    fn visit_expr_closure(&mut self, c: &'ast syn::ExprClosure) {
        self.scoped(|v| {
            c.inputs.iter().for_each(|p| v.bind(p));
            v.visit_expr(&c.body);
        });
    }

    fn visit_arm(&mut self, arm: &'ast syn::Arm) {
        self.scoped(|v| {
            v.bind(&arm.pat);
            if let Some((_, guard)) = &arm.guard {
                v.visit_expr(guard);
            }
            v.visit_expr(&arm.body);
        });
    }

    fn visit_expr_for_loop(&mut self, f: &'ast syn::ExprForLoop) {
        self.visit_expr(&f.expr);
        self.scoped(|v| {
            v.bind(&f.pat);
            v.visit_block(&f.body);
        });
    }

    fn visit_expr_if(&mut self, e: &'ast syn::ExprIf) {
        self.scoped(|v| {
            v.visit_expr(&e.cond);
            v.visit_block(&e.then_branch);
        });
        if let Some((_, other)) = &e.else_branch {
            self.visit_expr(other);
        }
    }

    fn visit_expr_while(&mut self, e: &'ast syn::ExprWhile) {
        self.scoped(|v| {
            v.visit_expr(&e.cond);
            v.visit_block(&e.body);
        });
    }

    fn visit_expr_let(&mut self, e: &'ast syn::ExprLet) {
        self.visit_expr(&e.expr);
        self.bind(&e.pat);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        match expr {
            Expr::Path(p) => {
                if let Some(q) = &p.qself {
                    self.qualified(q, &p.path);
                } else {
                    let segs = segments(&p.path);
                    let shadowed = matches!(segs.as_slice(), [one] if self.local(one));
                    let resolved = self.krate.resolve(self.scope, &segs, self.self_ty);
                    if let (false, Some(Res::Fns(keys))) = (shadowed, resolved) {
                        self.found.extend(keys);
                    }
                }
            }
            Expr::MethodCall(m) => {
                // `self` and `*self` are the value; `**self` is what it points to.
                let (receiver, derefs) = peel(&m.receiver);
                let named = |name: &str| matches!(receiver, Expr::Path(p) if p.path.is_ident(name));
                if m.method == "descend" && self.depths.iter().any(|d| named(d)) {
                    self.descends = true;
                }
                if let (true, true, Some(ty)) = (named("self"), derefs <= 1, self.self_ty) {
                    self.found.extend(self.krate.methods_named(ty, &m.method.to_string()));
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
