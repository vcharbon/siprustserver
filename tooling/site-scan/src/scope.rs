//! The names one scope declares and imports, and the resolution of a path
//! written in it to a module, a type or the functions of the crate it may
//! call. A scope is a module, or a function body holding items of its own
//! (a path segment `<fn name>` under its module), which sees its module's
//! names behind its own. Names of other crates resolve to nothing.

use std::collections::{BTreeMap, BTreeSet};

use syn::{Item, UseTree};

/// A type or trait of the crate: its scope and its name. A type of another
/// crate an `impl` names is keyed under the scope `<extern>`.
pub type TypeKey = (Vec<String>, String);

/// A function: a scope's free function, or a type's method with the trait
/// of the impl that holds it (`None` in an inherent impl, and for a trait's
/// own default method, keyed under the trait).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FnKey {
    Free(Vec<String>, String),
    Method(TypeKey, Option<TypeKey>, String),
}

/// What a path names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Res {
    Module(Vec<String>),
    Type(TypeKey),
    /// The functions a call through the path may reach.
    Fns(Vec<FnKey>),
}

/// The segment of a function body's own scope.
pub fn body_segment(owner: Option<&str>, name: &str) -> String {
    match owner {
        Some(ty) => format!("<fn {ty}::{name}>"),
        None => format!("<fn {name}>"),
    }
}

fn is_body(segment: &str) -> bool {
    segment.starts_with("<fn ")
}

/// The module `scope` lies in: its path without its body segments.
fn module_of(scope: &[String]) -> Vec<String> {
    let real = scope.iter().position(|s| is_body(s)).unwrap_or(scope.len());
    scope.get(..real).unwrap_or(scope).to_vec()
}

/// The names of one scope.
#[derive(Default)]
pub struct Scope {
    pub fns: BTreeSet<String>,
    pub types: BTreeSet<String>,
    pub mods: BTreeSet<String>,
    /// A local name and the path it imports, as written.
    pub uses: BTreeMap<String, Vec<String>>,
    pub globs: Vec<Vec<String>>,
}

impl Scope {
    /// Records what `item` declares or imports.
    pub fn add(&mut self, item: &Item) {
        let name = match item {
            Item::Fn(f) => {
                self.fns.insert(f.sig.ident.to_string());
                return;
            }
            Item::Mod(m) => {
                self.mods.insert(m.ident.to_string());
                return;
            }
            Item::Use(u) => return self.add_use(Vec::new(), &u.tree),
            Item::Struct(s) => &s.ident,
            Item::Enum(e) => &e.ident,
            Item::Union(u) => &u.ident,
            Item::Trait(t) => &t.ident,
            Item::Type(t) => &t.ident,
            _ => return,
        };
        self.types.insert(name.to_string());
    }

    fn add_use(&mut self, prefix: Vec<String>, tree: &UseTree) {
        let mut work = vec![(prefix, tree)];
        while let Some((prefix, tree)) = work.pop() {
            match tree {
                UseTree::Path(p) => {
                    let mut next = prefix;
                    next.push(p.ident.to_string());
                    work.push((next, &p.tree));
                }
                UseTree::Name(n) if n.ident == "self" => {
                    if let Some(last) = prefix.last() {
                        self.uses.insert(last.clone(), prefix.clone());
                    }
                }
                UseTree::Name(n) => {
                    let mut path = prefix;
                    path.push(n.ident.to_string());
                    self.uses.insert(n.ident.to_string(), path);
                }
                UseTree::Rename(r) if r.ident == "self" => {
                    self.uses.insert(r.rename.to_string(), prefix);
                }
                UseTree::Rename(r) => {
                    let mut path = prefix;
                    path.push(r.ident.to_string());
                    self.uses.insert(r.rename.to_string(), path);
                }
                UseTree::Glob(_) => self.globs.push(prefix),
                UseTree::Group(g) => work.extend(g.items.iter().map(|t| (prefix.clone(), t))),
            }
        }
    }
}

/// The scopes of a crate, its traits and the methods of its types.
#[derive(Default)]
pub struct Crate {
    pub scopes: BTreeMap<Vec<String>, Scope>,
    /// Each type's (or trait's) methods: name, and the trait of the impl
    /// holding it.
    pub methods: BTreeMap<TypeKey, BTreeSet<(String, Option<TypeKey>)>>,
    pub traits: BTreeSet<TypeKey>,
    /// The traits each type implements.
    pub traits_of: BTreeMap<TypeKey, BTreeSet<TypeKey>>,
    /// The types implementing each trait.
    pub implementors: BTreeMap<TypeKey, BTreeSet<TypeKey>>,
}

/// How many use and glob hops one resolution follows: a cycle of imports
/// ends there.
const HOPS: usize = 32;

impl Crate {
    /// The methods `name` of `ty` a path or a call on `self` may reach. On a
    /// type: the inherent one, as rustc picks it first, else its trait
    /// impls', else the defaults of the traits it implements. On a trait: its
    /// default and every implementor's own.
    pub fn methods_named(&self, ty: &TypeKey, name: &str) -> Vec<FnKey> {
        let has = |t: &TypeKey, via: Option<&TypeKey>| {
            self.methods.get(t).is_some_and(|m| m.contains(&(name.to_string(), via.cloned())))
        };
        let key = |t: &TypeKey, via: Option<&TypeKey>| {
            FnKey::Method(t.clone(), via.cloned(), name.to_string())
        };
        if self.traits.contains(ty) {
            let own = has(ty, None).then(|| key(ty, None));
            let impls = self.implementors.get(ty).into_iter().flatten();
            return own
                .into_iter()
                .chain(impls.filter(|x| has(x, Some(ty))).map(|x| key(x, Some(ty))))
                .collect();
        }
        if has(ty, None) {
            return vec![key(ty, None)];
        }
        let mut out: Vec<FnKey> = self
            .methods
            .get(ty)
            .into_iter()
            .flatten()
            .filter(|(n, via)| n == name && via.is_some())
            .map(|(_, via)| key(ty, via.as_ref()))
            .collect();
        if out.is_empty() {
            let traits = self.traits_of.get(ty).into_iter().flatten();
            out = traits.filter(|t| has(t, None)).map(|t| key(t, None)).collect();
        }
        out
    }

    /// The import `name` names in `scope`, or in the scopes it sees.
    pub fn import(&self, scope: &[String], name: &str) -> Option<&Vec<String>> {
        let mut at = scope.to_vec();
        loop {
            if let Some(path) = self.scopes.get(&at).and_then(|s| s.uses.get(name)) {
                return Some(path);
            }
            if !at.last().is_some_and(|s| is_body(s)) {
                return None;
            }
            at.pop();
        }
    }

    /// What `path`, written in `scope` (in an `impl` of `self_ty`), names.
    pub fn resolve(
        &self,
        scope: &[String],
        path: &[String],
        self_ty: Option<&TypeKey>,
    ) -> Option<Res> {
        self.resolve_in(scope, path, self_ty, true, HOPS)
    }

    fn resolve_in(
        &self,
        scope: &[String],
        path: &[String],
        self_ty: Option<&TypeKey>,
        value: bool,
        hops: usize,
    ) -> Option<Res> {
        let (first, mut rest) = path.split_first()?;
        let mut at = match first.as_str() {
            "crate" => Res::Module(Vec::new()),
            "self" => Res::Module(module_of(scope)),
            "Self" => Res::Type(self_ty?.clone()),
            "super" => {
                let mut m = module_of(scope);
                m.pop()?;
                while rest.first().is_some_and(|s| s == "super") {
                    m.pop()?;
                    rest = rest.get(1..)?;
                }
                Res::Module(m)
            }
            name => self.lookup(scope, name, value && rest.is_empty(), hops)?,
        };
        for (i, seg) in rest.iter().enumerate() {
            let last = i + 1 == rest.len();
            at = match at {
                Res::Module(m) => self.lookup(&m, seg, value && last, hops)?,
                Res::Type(t) => {
                    let fns = self.methods_named(&t, seg);
                    if fns.is_empty() {
                        return None;
                    }
                    Res::Fns(fns)
                }
                Res::Fns(_) => return None,
            };
        }
        Some(at)
    }

    /// What `name` names in `scope`: in the value namespace first when it
    /// ends the path (`value`), else in the type namespace; its own items,
    /// then its imports, then the scope it lies in when it is a body.
    fn lookup(&self, scope: &[String], name: &str, value: bool, hops: usize) -> Option<Res> {
        let hops = hops.checked_sub(1)?;
        let parent = || {
            let mut up = scope.to_vec();
            up.pop();
            up
        };
        let Some(s) = self.scopes.get(scope) else {
            return match scope.last() {
                Some(seg) if is_body(seg) => self.lookup(&parent(), name, value, hops),
                _ => None,
            };
        };
        let item = || {
            let free = || {
                s.fns
                    .contains(name)
                    .then(|| Res::Fns(vec![FnKey::Free(scope.to_vec(), name.to_string())]))
            };
            let ty =
                || s.types.contains(name).then(|| Res::Type((scope.to_vec(), name.to_string())));
            let module = || {
                s.mods.contains(name).then(|| {
                    let mut m = scope.to_vec();
                    m.push(name.to_string());
                    Res::Module(m)
                })
            };
            if value {
                free().or_else(ty).or_else(module)
            } else {
                module().or_else(ty).or_else(free)
            }
        };
        if let Some(found) = item() {
            return Some(found);
        }
        if let Some(path) = s.uses.get(name) {
            if path.len() == 1 && path.first().is_some_and(|p| p == name) {
                return None;
            }
            return self.resolve_in(scope, path, None, value, hops);
        }
        let globbed = s.globs.iter().find_map(|glob| {
            match self.resolve_in(scope, glob, None, false, hops)? {
                Res::Module(m) => self.lookup(&m, name, value, hops),
                _ => None,
            }
        });
        if globbed.is_some() {
            return globbed;
        }
        match scope.last() {
            Some(seg) if is_body(seg) => self.lookup(&parent(), name, value, hops),
            _ => None,
        }
    }
}
