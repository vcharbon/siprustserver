"""Checks of scripts/panic-sites.py on throwaway cargo workspaces.

The pass is read from recorded clippy JSON (`--json`), except in `RealPass`,
which runs clippy once on a one-crate workspace under the real clippy.toml
(`PANIC_SITES_CLIPPY_TOML`, by default this repository's).
"""
import os
import shutil
import subprocess
import tempfile
import unittest

from workspace import HERE, Workspace, debug_assert_expansion, message

SCRIPT = "panic-sites.py"
REAL_CLIPPY_TOML = os.environ.get(
    "PANIC_SITES_CLIPPY_TOML", os.path.join(HERE, "..", "..", "clippy.toml"))
ZERO = ("#![cfg_attr(not(test), deny(clippy::cast_possible_truncation, clippy::cast_possible_wrap,"
        " clippy::cast_sign_loss, clippy::expect_used, clippy::indexing_slicing, clippy::panic,"
        " clippy::string_slice, clippy::todo, clippy::unimplemented, clippy::unreachable,"
        " clippy::unwrap_used))]\n")
ZERO_PURE = ("#![cfg_attr(not(test), deny(clippy::cast_possible_truncation,"
             " clippy::cast_possible_wrap, clippy::cast_sign_loss, clippy::expect_used,"
             " clippy::indexing_slicing, clippy::iter_over_hash_type, clippy::panic,"
             " clippy::string_slice, clippy::todo, clippy::unimplemented, clippy::unreachable,"
             " clippy::unwrap_used))]\n")
HEADER = "# budget\n"
LIB = "pub fn f() {}\n" * 20


class Case(unittest.TestCase):
    def workspace(self, allow=HEADER, crates=(("a", "shell"),)):
        ws = Workspace()
        self.addCleanup(ws.tmp.cleanup)
        for name, tier in crates:
            ws.crate(name, tier, lib=LIB)
        ws.write("panic-sites.allow", allow)
        ws.commit()
        return ws

    def check(self, ws, records, *flags):
        ws.write("pass.json", "\n".join(records) + "\n")
        ws.git("add", "pass.json")
        return ws.run(SCRIPT, "--json", os.path.join(ws.root, "pass.json"), *flags)


class Budget(Case):
    def test_listed_file_one_site_over_its_count_fails(self):
        ws = self.workspace(HEADER + "crates/a/src/lib.rs 2\n")
        a = ws.ids()["a"]
        two = [message(a, "clippy::unwrap_used", "crates/a/src/lib.rs", n) for n in (3, 4)]
        self.assertEqual(self.check(ws, two)[0], 0)
        rc, out = self.check(ws, two + [message(a, "clippy::panic", "crates/a/src/lib.rs", 5)])
        self.assertEqual(rc, 1)
        self.assertIn("crates/a/src/lib.rs has 3 panic-class sites, budget 2", out)

    def test_unlisted_file_with_one_site_fails(self):
        ws = self.workspace(HEADER + "crates/a/src/lib.rs 1\n")
        a = ws.ids()["a"]
        rc, out = self.check(ws, [
            message(a, "clippy::unwrap_used", "crates/a/src/lib.rs", 3),
            message(a, "clippy::indexing_slicing", "crates/a/src/new.rs", 7)])
        self.assertEqual(rc, 1)
        self.assertIn("crates/a/src/new.rs has 1 panic-class sites, budget 0 (unlisted)", out)
        self.assertIn("crates/a/src/new.rs:7 clippy::indexing_slicing", out)

    def test_untiered_crate_is_not_counted(self):
        ws = self.workspace(HEADER + "crates/a/src/lib.rs 1\n", crates=(("a", "shell"),))
        ws.crate("harness", None, lib=LIB)
        ws.commit()
        ids = ws.ids()
        rc, out = self.check(ws, [
            message(ids["a"], "clippy::unwrap_used", "crates/a/src/lib.rs", 3),
            message(ids["harness"], "clippy::unwrap_used", "crates/harness/src/lib.rs", 3)])
        self.assertEqual(rc, 0, out)

    def test_named_crates_restrict_the_pass_and_skip_the_untiered(self):
        ws = self.workspace(HEADER + "crates/a/src/lib.rs 1\ncrates/b/src/lib.rs 1\n",
                            crates=(("a", "shell"), ("b", "shell")))
        ws.crate("harness", None, lib=LIB)
        ws.commit()
        a = ws.ids()["a"]
        rc, out = self.check(ws, [message(a, "clippy::panic", "crates/a/src/lib.rs", 2)],
                             "-p", "a", "-p", "harness")
        self.assertEqual(rc, 0, out)
        rc, out = self.check(ws, [], "-p", "harness")
        self.assertEqual(rc, 0, out)
        self.assertIn("no tiered crate among the named ones", out)
        rc, out = self.check(ws, [], "-p", "nope")
        self.assertEqual(rc, 2)


class Hunks(Case):
    def test_site_on_a_touched_line_fails_within_budget(self):
        ws = self.workspace(HEADER + "crates/a/src/lib.rs 1\n")
        a = ws.ids()["a"]
        site = [message(a, "clippy::unwrap_used", "crates/a/src/lib.rs", 4)]
        self.assertEqual(self.check(ws, site)[0], 0)
        lines = LIB.splitlines(keepends=True)
        lines[3] = "pub fn g() {}\n"
        ws.write("crates/a/src/lib.rs", "".join(lines))
        rc, out = self.check(ws, site)
        self.assertEqual(rc, 1)
        self.assertIn("new site in a touched hunk", out)
        self.assertIn("crates/a/src/lib.rs:4", out)

    def test_a_span_ending_on_a_touched_line_is_in_the_hunk(self):
        ws = self.workspace(HEADER + "crates/a/src/lib.rs 1\n")
        a = ws.ids()["a"]
        site = [message(a, "clippy::expect_used", "crates/a/src/lib.rs", 3, end=5)]
        lines = LIB.splitlines(keepends=True)
        lines[4] = "pub fn g() {}\n"
        ws.write("crates/a/src/lib.rs", "".join(lines))
        rc, out = self.check(ws, site)
        self.assertEqual(rc, 1)
        self.assertIn("new site in a touched hunk", out)

    def test_untracked_file_is_one_hunk(self):
        ws = self.workspace(HEADER + "crates/a/src/new.rs 1\n")
        a = ws.ids()["a"]
        ws.write("crates/a/src/new.rs", LIB)
        rc, out = self.check(ws, [message(a, "clippy::expect_used", "crates/a/src/new.rs", 9)])
        self.assertEqual(rc, 1)
        self.assertIn("new site in a touched hunk", out)


class Tightens(Case):
    def test_raised_entry_fails_against_the_base(self):
        ws = self.workspace(HEADER + "crates/a/src/lib.rs 1\n")
        a = ws.ids()["a"]
        ws.write("panic-sites.allow", HEADER + "crates/a/src/lib.rs 2\n")
        rc, out = self.check(ws, [message(a, "clippy::panic", "crates/a/src/lib.rs", n)
                                  for n in (2, 3)])
        self.assertEqual(rc, 1)
        self.assertIn("lists crates/a/src/lib.rs at 2, above its 1 at", out)

    def test_added_entry_fails_against_the_base(self):
        ws = self.workspace(HEADER + "crates/a/src/lib.rs 1\n")
        a = ws.ids()["a"]
        ws.write("panic-sites.allow", HEADER + "crates/a/src/lib.rs 1\ncrates/a/src/b.rs 1\n")
        ws.write("crates/a/src/b.rs", "\n")
        ws.git("add", "-A")
        ws.git("-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "b")
        rc, out = self.check(ws, [message(a, "clippy::panic", "crates/a/src/lib.rs", 2),
                                  message(a, "clippy::panic", "crates/a/src/b.rs", 1)])
        self.assertEqual(rc, 1)
        self.assertIn("adds crates/a/src/b.rs at 1, which", out)

    def test_renamed_file_keeps_its_entry(self):
        ws = self.workspace(HEADER + "crates/a/src/old.rs 1\n")
        ws.write("crates/a/src/old.rs", LIB)
        ws.commit()
        a = ws.ids()["a"]
        ws.git("mv", "crates/a/src/old.rs", "crates/a/src/new.rs")
        ws.write("panic-sites.allow", HEADER + "crates/a/src/new.rs 1\n")
        rc, out = self.check(ws, [message(a, "clippy::panic", "crates/a/src/new.rs", 30)])
        self.assertEqual(rc, 0, out)

    def test_staged_raise_fails_against_head(self):
        ws = self.workspace(HEADER + "crates/a/src/lib.rs 1\n")
        ws.write("panic-sites.allow", HEADER + "crates/a/src/lib.rs 3\n")
        ws.git("add", "panic-sites.allow")
        rc, out = ws.run(SCRIPT, "--staged")
        self.assertEqual(rc, 1)
        self.assertIn("at 3, above its 1 at", out)

    def test_staged_entries_are_held_to_the_base_not_head(self):
        ws = self.workspace(HEADER + "crates/a/src/lib.rs 2\n")
        ws.write("panic-sites.allow", HEADER + "crates/a/src/lib.rs 1\n")
        ws.commit(branch_develop=False)
        ws.write("panic-sites.allow", HEADER + "crates/a/src/lib.rs 1\ncrates/a/src/b.rs 1\n")
        ws.git("add", "panic-sites.allow")
        rc, out = ws.run(SCRIPT, "--staged")
        self.assertEqual(rc, 0, out)


class Stale(Case):
    def test_entry_above_its_count_is_stale_and_update_lowers_it(self):
        ws = self.workspace(HEADER + "crates/a/src/lib.rs 3\ncrates/a/src/gone.rs 2\n")
        a = ws.ids()["a"]
        one = [message(a, "clippy::panic", "crates/a/src/lib.rs", 2)]
        rc, out = self.check(ws, one)
        self.assertEqual(rc, 1)
        self.assertIn("lists crates/a/src/lib.rs at 3, it has 1", out)
        self.assertIn("lists crates/a/src/gone.rs at 2, it has 0", out)
        rc, out = self.check(ws, one, "--update")
        self.assertEqual(rc, 0, out)
        self.assertEqual(ws.read("panic-sites.allow"), HEADER + "crates/a/src/lib.rs 1\n")

    def test_update_never_adds_an_entry(self):
        ws = self.workspace(HEADER + "crates/a/src/lib.rs 1\n")
        a = ws.ids()["a"]
        rc, out = self.check(ws, [message(a, "clippy::panic", "crates/a/src/lib.rs", 2),
                                  message(a, "clippy::panic", "crates/a/src/b.rs", 2)],
                             "--update")
        self.assertEqual(rc, 1)
        self.assertNotIn("crates/a/src/b.rs", ws.read("panic-sites.allow"))


class Zero(Case):
    def test_crate_at_zero_without_the_attribute_fails(self):
        ws = self.workspace()
        rc, out = self.check(ws, [])
        self.assertEqual(rc, 1)
        self.assertIn("a is at zero: crates/a/src/lib.rs carries `#![cfg_attr(not(test), deny(",
                      out)

    def test_crate_at_zero_with_the_attribute_passes(self):
        ws = self.workspace()
        ws.write("crates/a/src/lib.rs", "// root\n" + ZERO + LIB)
        ws.commit()
        rc, out = self.check(ws, [])
        self.assertEqual(rc, 0, out)

    def test_a_pure_crate_at_zero_denies_hash_iteration_too(self):
        ws = self.workspace(crates=(("a", "pure"),))
        ws.write("crates/a/src/lib.rs", ZERO + LIB)
        ws.commit()
        rc, out = self.check(ws, [])
        self.assertEqual(rc, 1)
        self.assertIn("(missing clippy::iter_over_hash_type)", out)
        ws.write("crates/a/src/lib.rs", ZERO_PURE + LIB)
        ws.commit()
        rc, out = self.check(ws, [])
        self.assertEqual(rc, 0, out)

    def test_every_root_carries_it_the_bin_too(self):
        ws = self.workspace()
        ws.crate("a", "shell", lib=ZERO + LIB, bin="fn main() {}\n")
        ws.commit()
        rc, out = self.check(ws, [])
        self.assertEqual(rc, 1)
        self.assertIn("crates/a/src/main.rs carries", out)

    def test_crate_with_entries_and_the_attribute_fails(self):
        ws = self.workspace(HEADER + "crates/a/src/lib.rs 1\n")
        ws.write("crates/a/src/lib.rs", ZERO + LIB)
        ws.commit()
        a = ws.ids()["a"]
        rc, out = self.check(ws, [message(a, "clippy::panic", "crates/a/src/lib.rs", 4)])
        self.assertEqual(rc, 1)
        self.assertIn("a has allowlist entries: crates/a/src/lib.rs carries the zero attribute",
                      out)


class Classification(Case):
    PURE = "pure tier: reads the clock"

    def test_pure_only_entry_counts_in_a_pure_crate_only(self):
        ws = self.workspace(HEADER + "crates/s/src/lib.rs 1\ncrates/p/src/lib.rs 1\n",
                            crates=(("s", "shell"), ("p", "pure")))
        ids = ws.ids()
        base = [message(ids["s"], "clippy::panic", "crates/s/src/lib.rs", 2),
                message(ids["p"], "clippy::panic", "crates/p/src/lib.rs", 2)]
        shell_clock = message(ids["s"], "clippy::disallowed_methods", "crates/s/src/lib.rs", 5,
                              reason=self.PURE)
        self.assertEqual(self.check(ws, base + [shell_clock])[0], 0)
        pure_clock = message(ids["p"], "clippy::disallowed_methods", "crates/p/src/lib.rs", 5,
                             reason=self.PURE)
        rc, out = self.check(ws, base + [pure_clock])
        self.assertEqual(rc, 1)
        self.assertIn("crates/p/src/lib.rs has 2 panic-class sites, budget 1", out)

    def test_hash_iteration_lint_counts_in_a_pure_crate_only(self):
        ws = self.workspace(HEADER + "crates/s/src/lib.rs 1\ncrates/p/src/lib.rs 1\n",
                            crates=(("s", "shell"), ("p", "pure")))
        ids = ws.ids()
        base = [message(ids["s"], "clippy::panic", "crates/s/src/lib.rs", 2),
                message(ids["p"], "clippy::panic", "crates/p/src/lib.rs", 2)]
        shell_loop = message(ids["s"], "clippy::iter_over_hash_type", "crates/s/src/lib.rs", 5)
        self.assertEqual(self.check(ws, base + [shell_loop])[0], 0)
        pure_loop = message(ids["p"], "clippy::iter_over_hash_type", "crates/p/src/lib.rs", 5)
        rc, out = self.check(ws, base + [pure_loop])
        self.assertEqual(rc, 1)
        self.assertIn("crates/p/src/lib.rs has 2 panic-class sites, budget 1", out)

    def test_both_tier_entry_counts_in_a_shell_crate(self):
        ws = self.workspace(HEADER + "crates/a/src/lib.rs 1\n")
        a = ws.ids()["a"]
        rc, out = self.check(ws, [
            message(a, "clippy::panic", "crates/a/src/lib.rs", 2),
            message(a, "clippy::disallowed_methods", "crates/a/src/lib.rs", 6,
                    reason="panics past the end; use `split_at_checked`")])
        self.assertEqual(rc, 1)
        self.assertIn("has 2 panic-class sites, budget 1", out)

    def test_assert_from_debug_assert_is_not_counted(self):
        ws = self.workspace(HEADER + "crates/a/src/lib.rs 1\n")
        a = ws.ids()["a"]
        records = [message(a, "clippy::panic", "crates/a/src/lib.rs", 2),
                   message(a, "clippy::disallowed_macros", "/rust/core/src/macros/mod.rs", 290,
                           expansion=debug_assert_expansion("crates/a/src/lib.rs", 8))]
        self.assertEqual(self.check(ws, records)[0], 0)
        rc, out = self.check(ws, records[:1] + [
            message(a, "clippy::disallowed_macros", "crates/a/src/lib.rs", 8)])
        self.assertEqual(rc, 1)

    def test_site_in_a_macro_is_counted_at_its_call_site(self):
        ws = self.workspace(HEADER + "crates/a/src/lib.rs 1\n")
        a = ws.ids()["a"]
        expansion = {"macro_decl_name": "m!", "def_site_span": None,
                     "span": {"file_name": "crates/a/src/lib.rs", "line_start": 9,
                              "line_end": 9, "column_start": 1, "column_end": 4,
                              "is_primary": False, "expansion": None}}
        rc, out = self.check(ws, [
            message(a, "clippy::panic", "crates/a/src/lib.rs", 2),
            message(a, "clippy::unwrap_used", "/dep/src/lib.rs", 3, expansion=expansion)])
        self.assertEqual(rc, 1)
        self.assertIn("crates/a/src/lib.rs has 2 panic-class sites, budget 1", out)

    def test_a_disallowed_unwrap_under_unwrap_used_is_one_site(self):
        ws = self.workspace(HEADER + "crates/a/src/lib.rs 2\n")
        a = ws.ids()["a"]
        unwrap = "use of a disallowed method `std::option::Option::unwrap`"
        records = [message(a, "clippy::panic", "crates/a/src/lib.rs", 2),
                   message(a, "clippy::unwrap_used", "crates/a/src/lib.rs", 5, col=5, width=10),
                   message(a, "clippy::disallowed_methods", "crates/a/src/lib.rs", 5, col=7,
                           text=unwrap)]
        self.assertEqual(self.check(ws, records)[0], 0)
        local_macro = message(a, "clippy::disallowed_methods", "crates/a/src/lib.rs", 9, col=12,
                              text=unwrap)
        rc, out = self.check(ws, records + [local_macro])
        self.assertEqual(rc, 1)
        self.assertIn("has 3 panic-class sites, budget 2", out)
        external = {"macro_decl_name": "json!", "def_site_span": None,
                    "span": {"file_name": "crates/a/src/lib.rs", "line_start": 12,
                             "line_end": 12, "column_start": 1, "column_end": 4,
                             "is_primary": False, "expansion": None}}
        in_dependency = message(a, "clippy::disallowed_methods", "/dep/src/macros.rs", 279,
                                text=unwrap, expansion=external)
        self.assertEqual(self.check(ws, records + [in_dependency])[0], 0)

    def test_compile_error_fails_the_pass(self):
        ws = self.workspace(HEADER + "crates/a/src/lib.rs 1\n")
        a = ws.ids()["a"]
        rc, out = self.check(ws, [message(a, "clippy::panic", "crates/a/src/lib.rs", 2),
                                  message(a, None, "crates/a/src/lib.rs", 3, level="error")])
        self.assertEqual(rc, 1)
        self.assertIn("0 site(s) at deny, 1 other error(s)", out)

    def test_site_at_deny_fails_the_pass(self):
        ws = self.workspace()
        ws.write("crates/a/src/lib.rs", ZERO + LIB)
        ws.commit()
        a = ws.ids()["a"]
        rc, out = self.check(ws, [message(a, "clippy::unwrap_used", "crates/a/src/lib.rs", 3,
                                          level="error")])
        self.assertEqual(rc, 1)
        self.assertIn("1 site(s) at deny, 0 other error(s)", out)
        self.assertIn("crates/a/src/lib.rs has 1 panic-class sites, budget 0", out)


class Attributes(Case):
    """Source attributes beat the pass's `-W`: only a per-site `#[expect]`
    with a reason may silence a counted lint."""

    def outcome(self, source):
        ws = self.workspace(HEADER + "crates/a/src/lib.rs 1\n")
        a = ws.ids()["a"]
        ws.write("crates/a/src/lib.rs", LIB + source)
        ws.commit()
        return self.check(ws, [message(a, "clippy::panic", "crates/a/src/lib.rs", 2)])

    def assert_refused(self, source, why):
        rc, out = self.outcome(source)
        self.assertEqual(rc, 1, out)
        self.assertIn(why, out)

    def test_allow_of_a_counted_lint_fails(self):
        self.assert_refused("#[allow(clippy::unwrap_used)]\nfn g() {}\n",
                            "crates/a/src/lib.rs:21: `allow` of `clippy::unwrap_used`")

    def test_allow_of_a_group_holding_a_counted_lint_fails(self):
        for group in ("warnings", "clippy::restriction", "clippy::pedantic", "clippy::all",
                      "clippy::style"):
            with self.subTest(group=group):
                self.assert_refused(f"#[allow({group})]\nfn g() {{}}\n",
                                    f"`allow` of `{group}` silences panic-set lints")

    def test_expect_of_a_group_fails_even_with_a_reason(self):
        self.assert_refused("#[expect(clippy::pedantic, reason = \"x\")]\nfn g() {}\n",
                            "`expect` of `clippy::pedantic` silences panic-set lints")

    def test_cfg_attr_allow_or_expect_fails(self):
        self.assert_refused("#[cfg_attr(all(), allow(clippy::unwrap_used))]\nfn g() {}\n",
                            "`allow` of `clippy::unwrap_used`")
        self.assert_refused(
            "#[cfg_attr(all(), expect(clippy::unwrap_used, reason = \"x\"))]\nfn g() {}\n",
            "`expect` of `clippy::unwrap_used` under `cfg_attr`")

    def test_inner_expect_fails(self):
        self.assert_refused("mod m {\n    #![expect(clippy::indexing_slicing, reason = \"x\")]\n}\n",
                            "inner `#![expect(clippy::indexing_slicing)]` covers a whole module")

    def test_expect_on_a_mod_impl_or_trait_fails(self):
        for item, keyword in (("pub(crate) mod m {}", "mod"), ("impl S {}", "impl"),
                              ("pub unsafe trait T {}", "trait")):
            with self.subTest(item=item):
                self.assert_refused(
                    "pub struct S;\n#[expect(clippy::indexing_slicing, reason = \"x\")]\n"
                    f"#[doc(hidden)]\n{item}\n",
                    f"on a whole `{keyword}`; mark the site")

    def test_cfg_that_keeps_code_out_of_the_pass_fails(self):
        for pred in ("not(clippy)", "clippy", "not(debug_assertions)",
                     "all(unix, not(debug_assertions))"):
            with self.subTest(pred=pred):
                self.assert_refused(f"#[cfg({pred})]\nfn g() {{}}\n",
                                    f"`cfg({pred.replace(' ', '')})` keeps code out of the clippy pass")
        self.assert_refused("#[cfg_attr(not(clippy), doc = \"x\")]\nfn g() {}\n",
                            "`cfg(not(clippy))` keeps code out")

    def test_a_test_file_s_cfg_is_left_alone(self):
        ws = self.workspace(HEADER + "crates/a/src/lib.rs 1\n")
        ws.write("crates/a/tests/it.rs", "#[cfg(not(debug_assertions))]\nfn g() {}\n")
        ws.commit()
        a = ws.ids()["a"]
        rc, out = self.check(ws, [message(a, "clippy::panic", "crates/a/src/lib.rs", 2)])
        self.assertEqual(rc, 0, out)

    def test_expect_needs_a_reason(self):
        self.assert_refused("#[expect(\n    clippy::indexing_slicing\n)]\nfn g() {}\n",
                            "`expect` of `clippy::indexing_slicing` has no reason")

    def test_comments_and_strings_are_not_attributes(self):
        rc, out = self.outcome("// #[allow(warnings)]\n/* #![allow(clippy::all)] */\n"
                               "/// #[allow(clippy::unwrap_used)]\n"
                               "pub const S: &str = \"#[allow(warnings)]\";\n"
                               "pub const R: &str = r#\"#[allow(warnings)]\"#;\n"
                               "pub const Q: &core::ffi::CStr = cr##\"x\"#[allow(warnings)]\"##;\n"
                               "pub const C: char = '#';\n")
        self.assertEqual(rc, 0, out)


class Expects(Case):
    """A reasoned `#[expect]` exempts the one site of its lint its item or
    statement holds; with two sites in it, both count."""

    def outcome(self, source, sites):
        ws = self.workspace(HEADER + "crates/a/src/lib.rs 1\n")
        a = ws.ids()["a"]
        ws.write("crates/a/src/lib.rs", LIB + source)
        ws.commit()
        records = [message(a, "clippy::panic", "crates/a/src/lib.rs", 2)]
        records += [message(a, code, "crates/a/src/lib.rs", line, col=col)
                    for code, line, col in sites]
        return self.check(ws, records)

    FN = ("#[expect(clippy::indexing_slicing, reason = \"a constant table\")]\n"
          "pub fn g(v: &[u8]) -> u8 {\n    v[0] + v[1]\n}\n")

    def test_one_site_under_its_expect_is_exempt(self):
        rc, out = self.outcome(self.FN, [("clippy::indexing_slicing", 23, 5)])
        self.assertEqual(rc, 0, out)

    def test_two_sites_under_one_fn_expect_both_count(self):
        rc, out = self.outcome(self.FN, [("clippy::indexing_slicing", 23, 5),
                                         ("clippy::indexing_slicing", 23, 12)])
        self.assertEqual(rc, 1)
        self.assertIn("crates/a/src/lib.rs has 3 panic-class sites, budget 1", out)

    def test_a_const_item_holding_an_impl_exempts_nothing_of_two(self):
        source = ("pub struct S;\n#[expect(clippy::unwrap_used, reason = \"x\")]\n"
                  "const _: () = {\n    impl S {\n"
                  "        pub fn a(x: Option<u8>) -> u8 { x.unwrap() }\n"
                  "        pub fn b(x: Option<u8>) -> u8 { x.unwrap() }\n    }\n};\n")
        rc, out = self.outcome(source, [("clippy::unwrap_used", 25, 41),
                                        ("clippy::unwrap_used", 26, 41)])
        self.assertEqual(rc, 1)
        self.assertIn("has 3 panic-class sites, budget 1", out)

    def test_an_expect_of_another_lint_exempts_nothing(self):
        rc, out = self.outcome(self.FN, [("clippy::unwrap_used", 23, 5)])
        self.assertEqual(rc, 1)
        self.assertIn("has 2 panic-class sites, budget 1", out)

    def test_a_statement_expect_covers_its_statement_only(self):
        source = ("pub fn g(v: &[u8]) -> u8 {\n"
                  "    #[expect(clippy::indexing_slicing, reason = \"checked above\")]\n"
                  "    let x = v[0];\n    x + v[1]\n}\n")
        rc, out = self.outcome(source, [("clippy::indexing_slicing", 23, 13),
                                        ("clippy::indexing_slicing", 24, 9)])
        self.assertEqual(rc, 1)
        self.assertIn("has 2 panic-class sites, budget 1", out)


KEEP = "pub fn keep_one(v: &[u8]) -> u8 {\n    v[0]\n}\n"
MOVED = ("pub fn moved_parser(input_text: &str) -> u32 {\n"
         "    let parsed_value = input_text.trim().parse::<u32>();\n"
         "    parsed_value.unwrap()\n"
         "}\n")


class Moves(Case):
    """Code git sees moved carries its budget into its new file; nothing
    else raises an entry."""

    def split(self, dest, dest_crate="a", extra="", add=True):
        ws = self.workspace(HEADER + "crates/a/src/lib.rs 2\n" + (
            "crates/b/src/lib.rs 1\n" if dest_crate == "b" else ""),
            crates=(("a", "shell"), ("b", "shell")) if dest_crate == "b" else (("a", "shell"),))
        ws.write("crates/a/src/lib.rs", KEEP + "\n" + MOVED)
        ws.commit()
        ws.write("crates/a/src/lib.rs", KEEP)
        ws.write(dest, MOVED + extra)
        if add:
            ws.git("add", dest)
        ids = ws.ids()
        sites = [message(ids["a"], "clippy::indexing_slicing", "crates/a/src/lib.rs", 2),
                 message(ids[dest_crate], "clippy::unwrap_used", dest, 3)]
        if dest_crate == "b":
            sites.append(message(ids["b"], "clippy::panic", "crates/b/src/lib.rs", 2))
        if extra:
            sites.append(message(ids[dest_crate], "clippy::indexing_slicing", dest, 7))
        return ws, sites

    def test_a_split_of_a_listed_file_passes_after_update(self):
        ws, sites = self.split("crates/a/src/parse.rs")
        rc, out = self.check(ws, sites)
        self.assertEqual(rc, 1)
        self.assertIn("crates/a/src/parse.rs has 1 panic-class sites, budget 0", out)
        self.assertNotIn("touched hunk", out)
        rc, out = self.check(ws, sites, "--update")
        self.assertEqual(rc, 0, out)
        self.assertIn("added crates/a/src/parse.rs 0 -> 1", out)
        self.assertEqual(ws.read("panic-sites.allow"),
                         HEADER + "crates/a/src/lib.rs 1\ncrates/a/src/parse.rs 1\n")
        self.assertEqual(self.check(ws, sites)[0], 0)

    def test_a_split_into_an_untracked_file_passes_after_update(self):
        ws, sites = self.split("crates/a/src/parse.rs", add=False)
        self.assertNotIn("touched hunk", self.check(ws, sites)[1])
        self.assertEqual(self.check(ws, sites, "--update")[0], 0)
        self.assertEqual(self.check(ws, sites)[0], 0)
        self.assertIn("crates/a/src/parse.rs 1\n", ws.read("panic-sites.allow"))
        status = subprocess.run(["git", "-C", ws.root, "status", "--porcelain"],
                                capture_output=True, text=True).stdout
        self.assertIn("?? crates/a/src/parse.rs", status)

    def test_a_split_of_a_deleted_file_into_three_passes_after_update(self):
        parts = [MOVED.replace("moved_parser", name).replace("u32", ty)
                 for name, ty in (("first_parser", "u32"), ("second_parser", "u16"),
                                  ("third_parser", "u64"))]
        ws = self.workspace(HEADER + "crates/a/src/old.rs 3\n")
        ws.write("crates/a/src/old.rs", "\n".join(parts))
        ws.commit()
        a = ws.ids()["a"]
        ws.git("rm", "-q", "crates/a/src/old.rs")
        dests = ["crates/a/src/one.rs", "crates/a/src/two.rs", "crates/a/src/three.rs"]
        for dest, part in zip(dests, parts):
            ws.write(dest, part)
            ws.git("add", dest)
        sites = [message(a, "clippy::unwrap_used", dest, 3) for dest in dests]
        self.assertNotIn("touched hunk", self.check(ws, sites)[1])
        self.assertEqual(self.check(ws, sites, "--update")[0], 0)
        self.assertEqual(ws.read("panic-sites.allow"), HEADER + "".join(
            f"{dest} 1\n" for dest in sorted(dests)))
        self.assertEqual(self.check(ws, sites)[0], 0)

    def test_a_test_line_moved_into_production_carries_no_budget(self):
        tests = ("\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn parses() {\n"
                 "        let parsed_value_for_check = crate::parse(b\"abc\").unwrap();\n"
                 "        assert_eq!(parsed_value_for_check, b'a');\n    }\n}\n")
        cast = "pub fn first_byte(input: &[u8]) -> u8 {\n    input.len() as u8\n}\n"
        ws = self.workspace(HEADER + "crates/a/src/b.rs 1\ncrates/a/src/lib.rs 1\n")
        ws.write("crates/a/src/lib.rs", KEEP)
        ws.write("crates/a/src/b.rs", cast + tests)
        ws.commit()
        a = ws.ids()["a"]
        ws.write("crates/a/src/lib.rs", "pub fn keep_one(v: &[u8]) -> Option<u8> {\n"
                                        "    v.first().copied()\n}\n")
        ws.write("crates/a/src/b.rs", cast.replace(
            "    input.len()",
            "        let parsed_value_for_check = crate::parse(b\"abc\").unwrap();\n"
            "    let _ = parsed_value_for_check;\n    input.len()") +
            tests.replace("        let parsed_value_for_check = crate::parse(b\"abc\").unwrap();\n"
                          "        assert_eq!(parsed_value_for_check, b'a');\n",
                          "        assert_eq!(crate::parse(b\"abc\"), Some(b'a'));\n"))
        ws.write("panic-sites.allow", HEADER + "crates/a/src/b.rs 2\n")
        sites = [message(a, "clippy::unwrap_used", "crates/a/src/b.rs", 2),
                 message(a, "clippy::cast_possible_truncation", "crates/a/src/b.rs", 4)]
        rc, out = self.check(ws, sites)
        self.assertEqual(rc, 1)
        self.assertIn("new site in a touched hunk", out)
        self.assertIn("lists crates/a/src/b.rs at 2, above its 1 at", out)

    def test_a_move_across_crates_passes_after_update(self):
        ws, sites = self.split("crates/b/src/parse.rs", dest_crate="b")
        self.assertEqual(self.check(ws, sites, "--update")[0], 0)
        self.assertIn("crates/b/src/parse.rs 1\n", ws.read("panic-sites.allow"))
        self.assertEqual(self.check(ws, sites)[0], 0)

    def test_a_move_plus_one_new_site_fails(self):
        ws, sites = self.split("crates/a/src/parse.rs",
                               extra="\npub fn fresh(v: &[u8]) -> u8 {\n    v[3]\n}\n")
        rc, out = self.check(ws, sites, "--update")
        self.assertEqual(rc, 1)
        self.assertIn("crates/a/src/parse.rs 1\n", ws.read("panic-sites.allow"))
        rc, out = self.check(ws, sites)
        self.assertEqual(rc, 1)
        self.assertIn("crates/a/src/parse.rs has 2 panic-class sites, budget 1", out)
        self.assertIn("new site in a touched hunk", out)
        ws.write("panic-sites.allow", HEADER + "crates/a/src/lib.rs 1\ncrates/a/src/parse.rs 2\n")
        rc, out = self.check(ws, sites)
        self.assertEqual(rc, 1)
        self.assertIn("adds crates/a/src/parse.rs at 2, which", out)

    def test_a_hand_raised_entry_without_a_move_fails(self):
        ws = self.workspace(HEADER + "crates/a/src/lib.rs 2\ncrates/a/src/b.rs 2\n")
        a = ws.ids()["a"]
        ws.write("panic-sites.allow", HEADER + "crates/a/src/lib.rs 3\ncrates/a/src/b.rs 1\n")
        rc, out = self.check(ws, [message(a, "clippy::panic", "crates/a/src/lib.rs", n)
                                  for n in (2, 3, 4)] +
                             [message(a, "clippy::panic", "crates/a/src/b.rs", 1)])
        self.assertEqual(rc, 1)
        self.assertIn("lists crates/a/src/lib.rs at 3, above its 2 at", out)
        self.assertIn("1 above it, 0 on moved lines", out)


LISTS = """disallowed-methods = [
  { path = "std::vec::Vec::remove", reason = "panics past the end" },
  { path = "std::env::var", reason = "pure tier: reads the environment" },
]
"""


class Lists(Case):
    """The root clippy.toml's lists only tighten against the base."""

    def lists(self, now, *flags):
        ws = self.workspace(HEADER + "crates/a/src/lib.rs 1\n")
        ws.write("clippy.toml", LISTS)
        ws.commit()
        ws.write("clippy.toml", now)
        a = ws.ids()["a"]
        return self.check(ws, [message(a, "clippy::panic", "crates/a/src/lib.rs", 2)], *flags)

    def test_a_dropped_entry_fails(self):
        rc, out = self.lists(LISTS.replace(
            '  { path = "std::vec::Vec::remove", reason = "panics past the end" },\n', ""))
        self.assertEqual(rc, 1)
        self.assertIn("drops `std::vec::Vec::remove` from disallowed-methods", out)

    def test_a_both_tier_entry_narrowed_to_pure_fails(self):
        rc, out = self.lists(LISTS.replace('"panics past the end"', '"pure tier: panics"'))
        self.assertEqual(rc, 1)
        self.assertIn("narrows `std::vec::Vec::remove` in disallowed-methods to the pure tier",
                      out)

    def test_a_pure_entry_widened_to_both_passes(self):
        rc, out = self.lists(LISTS.replace('"pure tier: reads the environment"', '"env"'))
        self.assertEqual(rc, 0, out)

    def test_a_staged_narrowing_fails_against_head(self):
        ws = self.workspace(HEADER + "crates/a/src/lib.rs 1\n")
        ws.write("clippy.toml", LISTS)
        ws.commit()
        ws.write("clippy.toml", LISTS.replace('"panics past the end"', '"pure tier: x"'))
        ws.git("add", "clippy.toml")
        rc, out = ws.run(SCRIPT, "--staged")
        self.assertEqual(rc, 1)
        self.assertIn("narrows `std::vec::Vec::remove`", out)


class Report(Case):
    def test_report_prints_the_allowlist_form(self):
        ws = self.workspace()
        a = ws.ids()["a"]
        rc, out = self.check(ws, [message(a, "clippy::panic", "crates/a/src/lib.rs", n)
                                  for n in (2, 3)], "--report")
        self.assertEqual(rc, 0)
        self.assertIn("# a (shell): 2 sites in 1 files", out)
        self.assertIn("crates/a/src/lib.rs 2\n", out)


@unittest.skipUnless(shutil.which("cargo"), "no cargo")
class RealPass(Case):
    def test_clippy_counts_the_set_over_the_workspace_allow_and_skips_debug_assert(self):
        ws = Workspace()
        self.addCleanup(ws.tmp.cleanup)
        ws.crate("a", "pure", lib=(
            "pub fn f(v: &[u8], s: &str) -> u8 {\n"
            "    debug_assert!(v.len() < 9);\n"
            "    assert!(!v.is_empty());\n"
            "    let _ = std::time::Instant::now();\n"
            "    let (x, _) = v.split_at(1);\n"
            "    let _ = std::path::Path::new(s).exists();\n"
            "    s.parse::<u8>().unwrap() + x[0]\n"
            "}\n"
            "pub fn g(m: &std::collections::HashMap<u8, u8>) -> usize {\n"
            "    let mut n = m.keys().len();\n"
            "    for _ in m {\n"
            "        n += 1;\n"
            "    }\n"
            "    n\n"
            "}\n"
            "#[cfg(test)]\n"
            "mod tests {\n"
            "    #[test]\n"
            "    fn t() { assert_eq!(super::f(&[1], \"1\"), 2); Some(1).unwrap(); }\n"
            "}\n"))
        with open(REAL_CLIPPY_TOML) as f:
            ws.write("clippy.toml", f.read())
        ws.write("panic-sites.allow", HEADER)
        ws.commit()
        rc, out = ws.run(SCRIPT, "--report")
        self.assertEqual(rc, 0, out)
        self.assertIn("crates/a/src/lib.rs 8\n", out)
        self.assertNotIn("does not refer to", out)

    HIDDEN = (
        "#[allow(clippy::option_unwrap_used)]\n"
        "pub fn renamed(x: Option<u8>) -> u8 {\n    x.unwrap()\n}\n"
        "#[allow(clippy::r#unwrap_used)]\n"
        "pub fn raw(x: Option<u8>) -> u8 {\n    x.unwrap()\n}\n"
        "macro_rules! u {\n    ($e:expr) => {\n        $e.unwrap()\n    };\n}\n"
        "pub fn local_macro(x: Option<u8>) -> u8 {\n    u!(x)\n}\n"
        "#[expect(clippy::indexing_slicing, reason = \"a constant table\")]\n"
        "pub fn two_sites(v: &[u8], i: usize) -> u8 {\n    v[0] + v[i]\n}\n"
        "#[expect(clippy::indexing_slicing, reason = \"one site\")]\n"
        "pub fn one_site(v: &[u8]) -> u8 {\n    v[0]\n}\n"
        "pub fn cell() -> u8 {\n    std::cell::UnsafeCell::new(0u8).into_inner()\n}\n")
    INCLUDED = "#[allow(clippy::unwrap_used)]\npub fn included(x: Option<u8>) -> u8 {\n" \
               "    x.unwrap()\n}\n"

    def test_no_source_attribute_hides_a_site_from_the_pass(self):
        ws = Workspace()
        self.addCleanup(ws.tmp.cleanup)
        ws.crate("a", "pure", lib="mod hidden;\ninclude!(\"included.in\");\n")
        ws.write("crates/a/src/hidden.rs", self.HIDDEN)
        ws.write("crates/a/src/included.in", self.INCLUDED)
        with open(REAL_CLIPPY_TOML) as f:
            ws.write("clippy.toml", f.read())
        ws.write("panic-sites.allow", HEADER)
        ws.commit()
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        elsewhere = tmp.name
        with open(os.path.join(elsewhere, "clippy.toml"), "w") as f:
            f.write("")
        rc, out = ws.run(SCRIPT, "--report", env={**os.environ, "CLIPPY_CONF_DIR": elsewhere})
        self.assertEqual(rc, 0, out)
        self.assertIn("crates/a/src/hidden.rs 6\n", out)
        self.assertIn("crates/a/src/included.in 1\n", out)


if __name__ == "__main__":
    unittest.main()
