//! The module tree of one crate target, read from its root file as rustc
//! finds the files (`mod x;` as `x.rs` or `x/mod.rs`, `#[path]`), the
//! modules and items a non-test build leaves out dropped.

use std::fs;
use std::path::{Path, PathBuf};

use syn::{Item, Lit};

use crate::cfg::test_only;

/// One module: its path from the crate root, the file holding it (relative
/// to the scan root) and its own items, inline child modules excluded.
pub struct Module {
    pub path: Vec<String>,
    pub file: String,
    pub items: Vec<Item>,
    /// The names of its child modules a non-test build holds.
    pub children: Vec<String>,
}

/// What `load` could not read, as messages.
pub type Problems = Vec<String>;

struct Pending {
    path: Vec<String>,
    file: PathBuf,
    /// Where the files of `mod x;` declared here live.
    dir: PathBuf,
    /// What a `#[path]` on `mod x;` declared here is relative to.
    attr_base: PathBuf,
    items: Option<Vec<Item>>,
}

/// The modules of the target rooted at `root_file` (relative to `root`).
pub fn load(root: &Path, root_file: &str) -> (Vec<Module>, Problems) {
    let file = root.join(root_file);
    let dir = file.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut work =
        vec![Pending { path: Vec::new(), attr_base: dir.clone(), dir, file, items: None }];
    let (mut modules, mut problems) = (Vec::new(), Vec::new());
    while let Some(next) = work.pop() {
        let items = match next.items {
            Some(items) => items,
            None => match parse(&next.file) {
                Ok(items) => items,
                Err(why) => {
                    problems.push(why);
                    continue;
                }
            },
        };
        let (mut own, mut children) = (Vec::new(), Vec::new());
        for item in items {
            if test_only(attrs_of(&item)) {
                continue;
            }
            let Item::Mod(module) = item else {
                own.push(item);
                continue;
            };
            let name = module.ident.to_string();
            children.push(name.clone());
            let mut path = next.path.clone();
            path.push(name.clone());
            let attr = path_attr(&module.attrs);
            if let Some((_, content)) = module.content {
                let dir = match &attr {
                    Some(p) => next.attr_base.join(p),
                    None => next.dir.join(&name),
                };
                work.push(Pending {
                    path,
                    file: next.file.clone(),
                    attr_base: dir.clone(),
                    dir,
                    items: Some(content),
                });
                continue;
            }
            let found = match &attr {
                Some(p) => Some(next.attr_base.join(p)),
                None => [next.dir.join(format!("{name}.rs")), next.dir.join(&name).join("mod.rs")]
                    .into_iter()
                    .find(|f| f.is_file()),
            };
            let Some(file) = found else {
                problems.push(format!("{}: no file for `mod {name};`", rel(root, &next.file)));
                continue;
            };
            let parent = file.parent().map(Path::to_path_buf).unwrap_or_default();
            let mod_rs = attr.is_some() || file.file_name().is_some_and(|f| f == "mod.rs");
            let dir = if mod_rs { parent.clone() } else { parent.join(&name) };
            work.push(Pending { path, file, attr_base: parent, dir, items: None });
        }
        modules.push(Module { path: next.path, file: rel(root, &next.file), items: own, children });
    }
    (modules, problems)
}

fn parse(file: &Path) -> Result<Vec<Item>, String> {
    let text = fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let parsed = syn::parse_file(&text).map_err(|e| format!("{}: {e}", file.display()))?;
    if test_only(&parsed.attrs) {
        return Ok(Vec::new());
    }
    Ok(parsed.items)
}

fn rel(root: &Path, file: &Path) -> String {
    file.strip_prefix(root).unwrap_or(file).to_string_lossy().into_owned()
}

fn path_attr(attrs: &[syn::Attribute]) -> Option<String> {
    attrs.iter().find(|a| a.path().is_ident("path")).and_then(|a| match &a.meta {
        syn::Meta::NameValue(nv) => match &nv.value {
            syn::Expr::Lit(syn::ExprLit { lit: Lit::Str(s), .. }) => Some(s.value()),
            _ => None,
        },
        _ => None,
    })
}

/// The attributes of `item`.
pub fn attrs_of(item: &Item) -> &[syn::Attribute] {
    match item {
        Item::Const(i) => &i.attrs,
        Item::Enum(i) => &i.attrs,
        Item::ExternCrate(i) => &i.attrs,
        Item::Fn(i) => &i.attrs,
        Item::ForeignMod(i) => &i.attrs,
        Item::Impl(i) => &i.attrs,
        Item::Macro(i) => &i.attrs,
        Item::Mod(i) => &i.attrs,
        Item::Static(i) => &i.attrs,
        Item::Struct(i) => &i.attrs,
        Item::Trait(i) => &i.attrs,
        Item::TraitAlias(i) => &i.attrs,
        Item::Type(i) => &i.attrs,
        Item::Union(i) => &i.attrs,
        Item::Use(i) => &i.attrs,
        _ => &[],
    }
}
