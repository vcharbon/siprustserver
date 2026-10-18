"""Checks of scripts/purity.py on throwaway path-only cargo workspaces (cargo
metadata and cargo tree, nothing built)."""
import unittest

from workspace import Workspace

SCRIPT = "purity.py"


def config(binaries=("bin",), untiered=(), reaches=None, tests=("testkit", "test-util"),
           pure_deps=()):
    text = (f"binaries = {list(binaries)!r}\ntest-features = {list(tests)!r}\n"
            f"pure-deps = {list(pure_deps)!r}\n[known]\nuntiered = {list(untiered)!r}\n")
    text += "[known.pure-reaches]\n" + "".join(
        f"{c} = {ds!r}\n" for c, ds in (reaches or {}).items())
    return text.replace("'", '"')


class Case(unittest.TestCase):
    def workspace(self, conf, crates):
        ws = Workspace()
        self.addCleanup(ws.tmp.cleanup)
        for name, tier, deps, features in crates:
            ws.crate(name, tier, deps=deps, features=features,
                     bin="fn main() {}\n" if name == "bin" else None)
        ws.write("purity.toml", conf)
        ws.commit()
        return ws


class Closure(Case):
    def test_binary_reaching_a_testkit_feature_fails(self):
        ws = self.workspace(config(), [
            ("clock", "shell", (), {"testkit": []}),
            ("support", None, ("clock/testkit",), None),
            ("core", "shell", ("clock",), None),
            ("bin", "shell", ("core",), None)])
        self.assertEqual(ws.run(SCRIPT)[0], 0)
        ws.crate("core", "shell", deps=("clock/testkit",))
        rc, out = ws.run(SCRIPT)
        self.assertEqual(rc, 1)
        self.assertIn('bin reaches clock feature "testkit" (via bin > core)', out)

    def test_a_test_util_feature_fails_too(self):
        ws = self.workspace(config(), [
            ("clock", "shell", (), {"test-util": []}),
            ("bin", "shell", ("clock/test-util",), None)])
        rc, out = ws.run(SCRIPT)
        self.assertEqual(rc, 1)
        self.assertIn('bin reaches clock feature "test-util"', out)

    def test_closure_crate_without_a_tier_fails(self):
        ws = self.workspace(config(), [
            ("recorder", None, (), None),
            ("bin", "shell", ("recorder",), None)])
        rc, out = ws.run(SCRIPT)
        self.assertEqual(rc, 1)
        self.assertIn("bin links recorder, which declares no tier (via bin)", out)

    def test_known_untiered_passes_and_goes_stale_once_gone(self):
        ws = self.workspace(config(untiered=["recorder"]), [
            ("recorder", None, (), None),
            ("bin", "shell", ("recorder",), None)])
        self.assertEqual(ws.run(SCRIPT)[0], 0)
        ws.crate("bin", "shell", bin="fn main() {}\n")
        rc, out = ws.run(SCRIPT)
        self.assertEqual(rc, 1)
        self.assertIn("lists recorder as untiered, which no binary links", out)

    def test_bad_tier_value_fails(self):
        ws = self.workspace(config(), [("bin", "impure", (), None)])
        rc, out = ws.run(SCRIPT)
        self.assertEqual(rc, 1)
        self.assertIn("bin declares tier `impure`", out)


class PureRule(Case):
    def test_pure_crate_gaining_a_shell_dependency_fails(self):
        ws = self.workspace(config(), [
            ("io", "shell", (), None),
            ("model", "pure", (), None),
            ("core", "pure", ("model",), None),
            ("bin", "shell", ("core", "io"), None)])
        self.assertEqual(ws.run(SCRIPT)[0], 0)
        ws.crate("core", "pure", deps=("model", "io"))
        rc, out = ws.run(SCRIPT)
        self.assertEqual(rc, 1)
        self.assertIn("pure crate core reaches io, which declares tier shell (core > io)", out)

    def test_pure_crate_depending_on_an_untiered_crate_fails(self):
        ws = self.workspace(config(untiered=["helper"]), [
            ("helper", None, (), None),
            ("core", "pure", ("helper",), None),
            ("bin", "shell", ("core",), None)])
        rc, out = ws.run(SCRIPT)
        self.assertEqual(rc, 1)
        self.assertIn("pure crate core reaches helper, which declares no tier", out)

    def test_known_breach_passes_and_goes_stale_once_fixed(self):
        crates = [("io", "shell", (), None), ("core", "pure", ("io",), None),
                  ("bin", "shell", ("core",), None)]
        ws = self.workspace(config(reaches={"core": ["io"]}), crates)
        self.assertEqual(ws.run(SCRIPT)[0], 0)
        ws.crate("core", "pure")
        rc, out = ws.run(SCRIPT)
        self.assertEqual(rc, 1)
        self.assertIn("lists core -> io under [known.pure-reaches], which no longer holds", out)

    def test_known_breach_the_base_does_not_hold_fails(self):
        ws = self.workspace(config(), [
            ("io", "shell", (), None), ("core", "pure", (), None),
            ("bin", "shell", ("core", "io"), None)])
        ws.crate("core", "pure", deps=("io",))
        ws.write("purity.toml", config(reaches={"core": ["io"]}))
        rc, out = ws.run(SCRIPT)
        self.assertEqual(rc, 1)
        self.assertIn("adds known breach `pure-reaches: core -> io`", out)
        ws.git("add", "purity.toml")
        rc, out = ws.run(SCRIPT, "--staged")
        self.assertEqual(rc, 1)
        self.assertIn(":purity.toml does not hold", out)

    def test_staged_known_entries_are_held_to_the_base_not_head(self):
        ws = self.workspace(config(), [
            ("io", "shell", (), None), ("core", "pure", ("io",), None),
            ("bin", "shell", ("core",), None)])
        ws.write("purity.toml", config(reaches={"core": ["io"]}))
        ws.commit(branch_develop=False)
        ws.write("purity.toml", config(reaches={"core": ["io"]}) + "# a later step\n")
        ws.git("add", "purity.toml")
        rc, out = ws.run(SCRIPT, "--staged")
        self.assertEqual(rc, 1)
        self.assertIn("adds known breach `pure-reaches: core -> io`", out)


class Features(Case):
    def test_a_named_crate_feature_is_a_test_feature(self):
        conf = config(tests=("testkit", "clock/authored"))
        ws = self.workspace(conf, [
            ("clock", "shell", (), {"authored": []}),
            ("other", "shell", (), {"authored": []}),
            ("bin", "shell", ("clock", "other/authored"), None)])
        self.assertEqual(ws.run(SCRIPT)[0], 0)
        ws.crate("bin", "shell", deps=("clock/authored", "other/authored"),
                 bin="fn main() {}\n")
        rc, out = ws.run(SCRIPT)
        self.assertEqual(rc, 1)
        self.assertIn('bin reaches clock feature "authored" (via bin)', out)
        self.assertNotIn("other feature", out)

    def test_a_pure_crate_under_the_binary_s_features_is_checked(self):
        ws = self.workspace(config(), [
            ("io", "shell", (), None),
            ("core", "pure", (), None),
            ("bin", "shell", ("core",), None)])
        ws.write("crates/core/Cargo.toml", ws.read("crates/core/Cargo.toml").replace(
            "[dependencies]\n",
            '[features]\nclock = ["dep:io"]\n\n[dependencies]\n'
            'io = { path = "../io", optional = true }\n'))
        self.assertEqual(ws.run(SCRIPT)[0], 0)
        ws.crate("bin", "shell", deps=("core/clock",), bin="fn main() {}\n")
        rc, out = ws.run(SCRIPT)
        self.assertEqual(rc, 1)
        self.assertIn("pure crate core reaches io, which declares tier shell (core > io)", out)


class Base(Case):
    """The configuration only tightens against the base."""

    def test_a_dropped_binary_or_test_feature_fails(self):
        ws = self.workspace(config(binaries=("bin", "tool")), [
            ("bin", "shell", (), None), ("tool", "shell", (), None)])
        ws.write("purity.toml", config(binaries=("bin",), tests=("testkit",)))
        rc, out = ws.run(SCRIPT)
        self.assertEqual(rc, 1)
        self.assertIn("purity.toml drops binary tool, which", out)
        self.assertIn("purity.toml drops test feature test-util, which", out)

    def test_an_unused_pure_dep_is_stale(self):
        ws = self.workspace(config(pure_deps=("serde",)), [
            ("core", "pure", (), None), ("bin", "shell", ("core",), None)])
        rc, out = ws.run(SCRIPT)
        self.assertEqual(rc, 1)
        self.assertIn("lists serde in pure-deps, which no pure crate reaches", out)


class Lints(Case):
    def test_member_without_lints_fails(self):
        ws = self.workspace(config(), [("bin", "shell", (), None)])
        ws.crate("loose", None, lints=False)
        rc, out = ws.run(SCRIPT)
        self.assertEqual(rc, 1)
        self.assertIn("loose has no [lints]", out)

    def test_own_table_must_restate_the_workspace_clippy_table(self):
        ws = self.workspace(config(), [("bin", "shell", (), None)])
        ws.crate("unsafe-ffi", None, lints=False)
        manifest = ws.read("crates/unsafe-ffi/Cargo.toml")
        ws.write("crates/unsafe-ffi/Cargo.toml", manifest + (
            '[lints.rust]\nunsafe_code = "deny"\n[lints.clippy]\ntoo_many_lines = "warn"\n'))
        rc, out = ws.run(SCRIPT)
        self.assertEqual(rc, 1)
        self.assertIn("unsafe-ffi's [lints] is not the workspace's table", out)
        clippy = ('[lints.clippy]\ntoo_many_lines = "warn"\ndisallowed_macros = "allow"\n'
                  'disallowed_methods = "allow"\ndisallowed_types = "allow"\n')
        ws.write("crates/unsafe-ffi/Cargo.toml",
                 manifest + '[lints.rust]\nunsafe_code = "deny"\n' + clippy)
        self.assertEqual(ws.run(SCRIPT)[0], 0)
        for rust in ('unsafe_code = "allow"\n', 'unsafe_code = "deny"\nunused = "allow"\n'):
            with self.subTest(rust=rust):
                ws.write("crates/unsafe-ffi/Cargo.toml",
                         manifest + "[lints.rust]\n" + rust + clippy)
                rc, out = ws.run(SCRIPT)
                self.assertEqual(rc, 1)
                self.assertIn("unsafe-ffi's [lints] is not the workspace's table", out)

    def test_a_root_dot_clippy_toml_fails(self):
        ws = self.workspace(config(), [("bin", "shell", (), None)])
        ws.write(".clippy.toml", "too-many-lines-threshold = 100\n")
        rc, out = ws.run(SCRIPT)
        self.assertEqual(rc, 1)
        self.assertIn(".clippy.toml: clippy reads the nearest clippy.toml", out)

    def test_a_cargo_config_that_lowers_lints_fails(self):
        for text, why in (
                ('[build]\nrustflags = ["-Aclippy::unwrap_used"]\n',
                 "rustflags `-Aclippy::unwrap_used` lowers a lint level"),
                ('[target.x86_64-unknown-linux-gnu]\nrustflags = "--cap-lints allow"\n',
                 "caps the lint levels"),
                ('[env]\nCLIPPY_CONF_DIR = "/elsewhere"\n', "sets CLIPPY_CONF_DIR")):
            with self.subTest(text=text):
                ws = self.workspace(config(), [("bin", "shell", (), None)])
                ws.write(".cargo/config.toml", text)
                rc, out = ws.run(SCRIPT)
                self.assertEqual(rc, 1)
                self.assertIn(f".cargo/config.toml: {why}", out)
        ws = self.workspace(config(), [("bin", "shell", (), None)])
        ws.write(".cargo/config.toml", '[build]\nrustflags = ["--cfg", "tokio_unstable"]\n')
        self.assertEqual(ws.run(SCRIPT)[0], 0)

    def test_crate_local_clippy_toml_fails(self):
        ws = self.workspace(config(), [("bin", "shell", (), None)])
        ws.write("crates/bin/clippy.toml", "too-many-lines-threshold = 100\n")
        rc, out = ws.run(SCRIPT)
        self.assertEqual(rc, 1)
        self.assertIn("crates/bin/clippy.toml: clippy reads the nearest clippy.toml", out)


if __name__ == "__main__":
    unittest.main()
