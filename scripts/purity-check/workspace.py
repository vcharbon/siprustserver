"""Throwaway git repositories holding a path-only cargo workspace, for the
checks of scripts/panic-sites.py and scripts/purity.py."""
import json
import os
import subprocess
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
# The scripts under check: `PURITY_SCRIPTS` names another copy (a red proof
# runs these tests against the scripts at an older revision).
SCRIPTS = os.environ.get("PURITY_SCRIPTS", os.path.join(HERE, ".."))

LINTS = """
[workspace.lints.rust]
unsafe_code = "forbid"

[workspace.lints.clippy]
too_many_lines = "warn"
disallowed_macros = "allow"
disallowed_methods = "allow"
disallowed_types = "allow"
"""


class Workspace:
    """A git repository whose root is a cargo workspace of `crates/<name>`."""

    def __init__(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = self.tmp.name
        self.crates = {}
        self.git("init", "-q")
        # No detached auto-maintenance writes into .git while the directory is removed.
        self.git("config", "maintenance.auto", "false")
        self.git("config", "gc.auto", "0")

    def git(self, *args):
        subprocess.run(["git", "-C", self.root, *args], check=True, capture_output=True)

    def write(self, rel, text):
        path = os.path.join(self.root, rel)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as f:
            f.write(text)

    def read(self, rel):
        with open(os.path.join(self.root, rel)) as f:
            return f.read()

    def crate(self, name, tier=None, deps=(), features=None, lib="", bin=None, lints=True,
              side="aware"):
        """A member `crates/<name>` on HA side `side` (`None`: it declares
        none); `deps` names members, a `name/feature` entry enabling that
        feature."""
        self.crates[name] = True
        meta = f'\n[package.metadata.purity]\ntier = "{tier}"\n' if tier else ""
        meta += f'\n[package.metadata.ha]\nside = "{side}"\n' if side else ""
        lint = "\n[lints]\nworkspace = true\n" if lints else ""
        feats = "" if features is None else "\n[features]\n" + "".join(
            f"{k} = {json.dumps(v)}\n" for k, v in features.items())
        dep_lines = []
        for d in deps:
            dep, _, feat = d.partition("/")
            extra = f', features = ["{feat}"]' if feat else ""
            dep_lines.append(f'{dep} = {{ path = "../{dep}"{extra} }}')
        self.write(f"crates/{name}/Cargo.toml",
                   f'[package]\nname = "{name}"\nversion = "0.1.0"\nedition = "2021"\n'
                   f"{meta}{lint}{feats}\n[dependencies]\n" + "\n".join(dep_lines) + "\n")
        self.write(f"crates/{name}/src/lib.rs", lib)
        if bin is not None:
            self.write(f"crates/{name}/src/main.rs", bin)
        self.manifest()

    def manifest(self, lints=LINTS):
        members = ", ".join(f'"crates/{n}"' for n in sorted(self.crates))
        self.write("Cargo.toml", f'[workspace]\nresolver = "2"\nmembers = [{members}]\n{lints}')

    def commit(self, branch_develop=True):
        self.git("add", "-A")
        self.git("-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "base")
        if branch_develop:
            self.git("branch", "-f", "develop")

    def ids(self):
        out = subprocess.run(["cargo", "metadata", "--no-deps", "--format-version", "1"],
                             cwd=self.root, capture_output=True, check=True)
        return {p["name"]: p["id"] for p in json.loads(out.stdout)["packages"]}

    def run(self, script, *flags, env=None):
        r = subprocess.run(["python3", os.path.join(SCRIPTS, script), "-C", self.root, *flags],
                           capture_output=True, text=True, env=env)
        return r.returncode, r.stdout + r.stderr


def message(pkg_id, code, file, line, reason=None, level="warning", expansion=None, end=None,
            col=5, text=None, width=4):
    """One clippy JSON record as `cargo clippy --message-format=json` prints it,
    its span over lines `line..=end` from column `col`, `width` columns wide."""
    children = [{"level": "note", "message": reason, "children": [], "spans": []}] if reason else []
    span = {"file_name": file, "line_start": line, "line_end": end or line, "column_start": col,
            "column_end": col + width, "is_primary": True, "expansion": expansion}
    return json.dumps({
        "reason": "compiler-message", "package_id": pkg_id,
        "message": {"message": text or f"{code} here", "code": {"code": code} if code else None,
                    "level": level, "spans": [span], "children": children,
                    "rendered": f"{level}: {code}\n --> {file}:{line}\n"}})


def debug_assert_expansion(file, line):
    """The expansion record of an `assert!` written by `debug_assert!` at
    `file:line`."""
    return {"macro_decl_name": "debug_assert!", "def_site_span": None,
            "span": {"file_name": file, "line_start": line, "line_end": line,
                     "column_start": 5, "column_end": 30, "is_primary": False,
                     "expansion": None}}
