//! The names one module declares and imports, and the resolution of a path
//! written in it to a module, a type or a function of the crate. Names of
//! other crates resolve to nothing.

use std::collections::{BTreeMap, BTreeSet};

use syn::{Item, UseTree};

/// A type of the crate: its module and its name. A type of another crate
/// that an `impl` names is keyed under the module `<extern>`.
pub type TypeKey = (Vec<String>, String);

/// A function: a module's free function or a type's method, with the trait
/// of the impl that holds it (`None` in an inherent impl, and for a trait's
/// own default method, keyed under the trait).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FnKey {
    Free(Vec<String>, String),
    Method(TypeKey, Option<String>, String),
}

/// What a path names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Res {
    Module(Vec<String>),
    Type(TypeKey),
    Fn(FnKey),
}

/// The names of one module.
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
        match item {
            Item::Fn(f) => {
                self.fns.insert(f.sig.ident.to_string());
            }
            Item::Struct(s) => {
                self.types.insert(s.ident.to_string());
            }
            Item::Enum(e) => {
                self.types.insert(e.ident.to_string());
            }
            Item::Union(u) => {
                self.types.insert(u.ident.to_string());
            }
            Item::Trait(t) => {
                self.types.insert(t.ident.to_string());
            }
            Item::Type(t) => {
                self.types.insert(t.ident.to_string());
            }
            Item::Mod(m) => {
                self.mods.insert(m.ident.to_string());
            }
            Item::Use(u) => self.add_use(Vec::new(), &u.tree),
            _ => {}
        }
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

/// The scopes of a crate and the methods of its types.
#[derive(Default)]
pub struct Crate {
    pub scopes: BTreeMap<Vec<String>, Scope>,
    /// Each type's methods: name and the trait of the impl holding it.
    pub methods: BTreeMap<TypeKey, BTreeSet<(String, Option<String>)>>,
}

/// How many use and glob hops one resolution follows: a cycle of imports
/// ends there.
const HOPS: usize = 32;

impl Crate {
    /// The method `name` of `ty` a path or a call on `self` reaches: the
    /// inherent one first, as rustc picks it, else a trait impl's.
    pub fn method(&self, ty: &TypeKey, name: &str) -> Option<FnKey> {
        let all = self.methods.get(ty)?;
        let inherent = (name.to_string(), None);
        if all.contains(&inherent) {
            return Some(FnKey::Method(ty.clone(), None, name.to_string()));
        }
        all.iter()
            .find(|(n, _)| n == name)
            .map(|(n, t)| FnKey::Method(ty.clone(), t.clone(), n.clone()))
    }

    /// What `path`, written in `module` (in an `impl` of `self_ty`), names.
    pub fn resolve(
        &self,
        module: &[String],
        path: &[String],
        self_ty: Option<&TypeKey>,
    ) -> Option<Res> {
        self.resolve_in(module, path, self_ty, HOPS)
    }

    fn resolve_in(
        &self,
        module: &[String],
        path: &[String],
        self_ty: Option<&TypeKey>,
        hops: usize,
    ) -> Option<Res> {
        let (first, mut rest) = path.split_first()?;
        let mut at = match first.as_str() {
            "crate" => Res::Module(Vec::new()),
            "self" => Res::Module(module.to_vec()),
            "Self" => Res::Type(self_ty?.clone()),
            "super" => {
                let mut m = module.to_vec();
                m.pop()?;
                while rest.first().is_some_and(|s| s == "super") {
                    m.pop()?;
                    rest = rest.get(1..)?;
                }
                Res::Module(m)
            }
            name => self.lookup(module, name, hops)?,
        };
        for seg in rest {
            at = match at {
                Res::Module(m) => self.lookup(&m, seg, hops)?,
                Res::Type(t) => Res::Fn(self.method(&t, seg)?),
                Res::Fn(_) => return None,
            };
        }
        Some(at)
    }

    /// What `name` names in `module`: its own items first, then its imports.
    fn lookup(&self, module: &[String], name: &str, hops: usize) -> Option<Res> {
        let scope = self.scopes.get(module)?;
        if scope.fns.contains(name) {
            return Some(Res::Fn(FnKey::Free(module.to_vec(), name.to_string())));
        }
        if scope.types.contains(name) {
            return Some(Res::Type((module.to_vec(), name.to_string())));
        }
        if scope.mods.contains(name) {
            let mut m = module.to_vec();
            m.push(name.to_string());
            return Some(Res::Module(m));
        }
        let hops = hops.checked_sub(1)?;
        if let Some(path) = scope.uses.get(name) {
            if path.len() == 1 && path.first().is_some_and(|p| p == name) {
                return None;
            }
            return self.resolve_in(module, path, None, hops);
        }
        scope.globs.iter().find_map(|glob| match self.resolve_in(module, glob, None, hops)? {
            Res::Module(m) => self.lookup(&m, name, hops),
            _ => None,
        })
    }
}
