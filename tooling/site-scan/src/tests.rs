//! The scan over throwaway crates.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::scan;
use crate::site::Set;

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Tree(PathBuf);

impl Tree {
    fn new(files: &[(&str, &str)]) -> Tree {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("site-scan-{}-{n}", std::process::id()));
        for (rel, text) in files {
            let path = dir.join(rel);
            fs::create_dir_all(path.parent().expect("a parent")).expect("a dir");
            fs::write(path, text).expect("written");
        }
        Tree(dir)
    }

    /// `(file, line, message)` of the sites of `src/lib.rs` in `set`.
    fn sites(&self, set: Set) -> Vec<(String, usize, String)> {
        let (sites, problems) = scan(&self.0, &["src/lib.rs".to_string()], &BTreeSet::from([set]));
        assert!(problems.is_empty(), "{problems:?}");
        sites.into_iter().map(|s| (s.file, s.line, s.message)).collect()
    }

    fn lines(&self, set: Set) -> Vec<usize> {
        self.sites(set).into_iter().map(|(_, line, _)| line).collect()
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn lib(text: &str) -> Tree {
    Tree::new(&[("src/lib.rs", text)])
}

#[test]
fn loop_and_while_are_sites() {
    let t = lib("fn f(mut v: Vec<u8>) {\n    loop { break; }\n    while v.pop().is_some() {}\n    while let Some(_) = v.pop() {}\n}\n");
    assert_eq!(t.lines(Set::Loop), vec![2, 3, 4]);
}

#[test]
fn a_for_over_a_collection_or_a_bounded_range_is_no_site() {
    let t = lib("const N: usize = 4;\nfn f(v: &[u8]) {\n    for _ in v {}\n    for _ in 0..N {}\n    for _ in 0..v.len() - 1 {}\n    for _ in 0..=u8::MAX {}\n    for _ in (0..v.len()).rev() {}\n    let _ = (0..v.len()).contains(&3);\n}\n");
    assert_eq!(t.lines(Set::Loop), Vec::<usize>::new());
}

#[test]
fn a_range_to_a_variable_or_open_is_a_site() {
    let t = lib("fn f(n: usize) {\n    for _ in 0..n {}\n    let _: Vec<usize> = (0..n).map(|i| i).collect();\n    for _ in 0.. { break; }\n}\n");
    assert_eq!(t.lines(Set::Loop), vec![2, 3, 4]);
}

#[test]
fn infinite_constructors_and_a_hand_written_next_are_sites() {
    let t = lib("use std::iter::{self, repeat};\nstruct S;\nimpl Iterator for S {\n    type Item = u8;\n    fn next(&mut self) -> Option<u8> { None }\n}\nfn f(v: &[u8]) {\n    let _ = repeat(1).take(2);\n    let _ = iter::from_fn(|| None::<u8>);\n    let _ = std::iter::successors(Some(1u8), |_| None);\n    let _ = v.iter().cycle();\n    let _: [u8; 2] = std::array::from_fn(|i| i as u8);\n}\n");
    assert_eq!(t.lines(Set::Loop), vec![5, 8, 9, 10, 11]);
}

#[test]
fn test_code_is_left_out() {
    let t = Tree::new(&[
        (
            "src/lib.rs",
            "mod a;\n#[cfg(test)]\nmod tests { fn f() { loop {} } }\n#[test]\nfn t() { loop {} }\n",
        ),
        ("src/a.rs", "struct S;\nimpl S {\n    #[cfg(test)]\n    fn f() { loop {} }\n}\n"),
    ]);
    assert_eq!(t.lines(Set::Loop), Vec::<usize>::new());
}

#[test]
fn module_files_are_found_as_rustc_finds_them() {
    let t = Tree::new(&[
        ("src/lib.rs", "mod a;\nmod b;\n#[path = \"other.rs\"]\nmod c;\nmod d { mod e; }\n"),
        ("src/a.rs", "mod inner;\n"),
        ("src/a/inner.rs", "fn f() { loop {} }\n"),
        ("src/b/mod.rs", "fn f() { loop {} }\n"),
        ("src/other.rs", "fn f() { loop {} }\n"),
        ("src/d/e.rs", "fn f() { loop {} }\n"),
    ]);
    let files: Vec<String> = t.sites(Set::Loop).into_iter().map(|(f, _, _)| f).collect();
    assert_eq!(files, vec!["src/a/inner.rs", "src/b/mod.rs", "src/d/e.rs", "src/other.rs"]);
}

#[test]
fn a_loop_in_a_macro_argument_is_a_site() {
    let t = lib("fn f(v: &mut Vec<u8>) -> String {\n    format!(\"{}\", loop { break 1; })\n}\n");
    assert_eq!(t.lines(Set::Loop), vec![2]);
}

#[test]
fn direct_and_mutual_recursion_are_sites() {
    let t = Tree::new(&[
        ("src/lib.rs", "mod a;\nfn f(n: u8) { if n > 0 { f(n - 1) } }\npub fn g() { a::h() }\n"),
        ("src/a.rs", "use crate::g;\npub fn h() { g() }\n"),
    ]);
    let sites = t.sites(Set::Recursion);
    let messages: Vec<&str> = sites.iter().map(|(_, _, m)| m.as_str()).collect();
    assert_eq!(
        messages,
        vec!["recursion with no depth guard: a::h -> g", "recursion with no depth guard: f"]
    );
}

#[test]
fn methods_resolve_through_self_and_the_type_path() {
    let t = lib("struct T;\nimpl T {\n    fn a(&self) { self.b() }\n    fn b(&self) { Self::c() }\n    fn c() { T::a(&T) }\n    fn d(&self, other: &T) { other.d(self) }\n}\n");
    let sites = t.sites(Set::Recursion);
    assert_eq!(sites.len(), 1, "{sites:?}");
    assert_eq!(sites[0].1, 3);
}

#[test]
fn a_function_passed_as_a_value_is_an_edge() {
    let t = lib("fn walk(v: &[Vec<u8>]) -> usize { v.iter().map(|x| x.len()).map(each).sum() }\nfn each(n: usize) -> usize { walk(&[]) + n }\n");
    assert_eq!(t.lines(Set::Recursion), vec![1]);
}

#[test]
fn a_trait_default_method_calling_itself_is_a_site() {
    let t = lib("trait Walk {\n    fn walk(&self) { self.walk() }\n}\n");
    assert_eq!(t.lines(Set::Recursion), vec![2]);
}

#[test]
fn a_depth_guarded_cycle_is_no_site() {
    let t = lib("use bounds::Depth;\nfn f(d: &Depth) -> Result<(), bounds::TooDeep> { let d = d.descend()?; g(&d) }\nfn g(d: &Depth) -> Result<(), bounds::TooDeep> { f(d) }\nfn h(d: bounds::Depth) { let _ = d; h(d) }\n");
    assert_eq!(t.lines(Set::Recursion), vec![4]);
}

#[test]
fn a_call_outside_the_crate_or_on_another_receiver_is_no_edge() {
    let t = lib("fn len(v: &[u8]) -> usize { v.len() }\nfn parse(s: &str) -> u8 { s.parse().unwrap_or(0) + other::parse(s) }\n");
    assert_eq!(t.lines(Set::Recursion), Vec::<usize>::new());
}

#[test]
fn a_local_binding_shadows_a_function_of_its_name() {
    let t = lib("fn total(all: &[u64], total: impl Fn(u64) -> u64) -> u64 { all.iter().copied().map(total).sum() }\nfn check(list: &[fn() -> bool]) -> bool { list.iter().all(|check| check()) }\n");
    assert_eq!(t.lines(Set::Recursion), Vec::<usize>::new());
}

#[test]
fn a_trait_method_delegating_to_the_inherent_one_is_no_cycle() {
    let t = lib("trait Ready { fn ready(&self) -> bool; }\nstruct S;\nimpl S { fn ready(&self) -> bool { true } }\nimpl Ready for S { fn ready(&self) -> bool { S::ready(self) && Self::ready(self) && self.ready() } }\n");
    assert_eq!(t.lines(Set::Recursion), Vec::<usize>::new());
}

#[test]
fn an_immutable_local_bound_to_a_len_bounds_a_range() {
    let t = lib("fn f(v: &[u8], n: usize) {\n    let passes = v.len() + 1;\n    for _ in 0..passes {}\n    let mut grows = v.len();\n    grows += n;\n    for _ in 0..grows {}\n    let other = n;\n    for _ in 0..other {}\n    let passes = n;\n    for _ in 0..passes {}\n}\nfn g(n: usize) {\n    for _ in 0..passes(n) {}\n}\nfn passes(n: usize) -> usize { n }\n");
    assert_eq!(t.lines(Set::Loop), vec![6, 8, 10, 13]);
}

#[test]
fn repeat_n_to_a_variable_count_is_a_site() {
    let t = lib("use std::iter;\nfn f(v: &[u8], n: usize) {\n    for _ in iter::repeat_n(0, n) {}\n    for _ in iter::repeat_n(0, v.len()) {}\n    for _ in std::iter::repeat_n(0, 4) {}\n}\n");
    assert_eq!(t.lines(Set::Loop), vec![3]);
}
