"""A per-file allowlist that only tightens: the machinery the ratchet lints share.

An allowlist file holds settings and entries:

  limit N                        the budget of an unlisted file
  test-limit N                   the budget of an unlisted test file
  test GLOB...                   path globs of the test files
  ext .rs                        the extensions scanned (default .rs)
  scan DIR...                    the directories scanned (default: the root)
  exclude GLOB                   a path glob left out
  PATH N                         an entry: a file over its limit, its budget

`#` starts a comment. Against its version at a base revision, an allowlist may
only lower or drop entries: a new entry, a raised number or a changed setting
is a loosening. The one new entry allowed is a file git sees renamed from a
listed one, the old entry gone and the number no higher; the changed settings
allowed are a `scan` or `exclude` path following a scan directory git sees
renamed whole, and `scan` directories added to a base that scans a list.

Each lint measures its files itself (`listed` gives the paths) and words its
own findings; a setup failure raises `SetupError`.
"""
import fnmatch
import os
import re
import subprocess

SETTING_KEYS = ("limit", "test-limit", "test", "exclude", "scan", "ext")
PLAIN_PATH = re.compile(r"^[A-Za-z0-9_][A-Za-z0-9_./-]*$")
TEST_DEFAULT = ["tests/*", "*/tests/*", "*/tests.rs", "*_tests.rs"]
EXT_DEFAULT = (".rs",)


class SetupError(Exception):
    pass


class Allow:
    def __init__(self, text, path, limit, test_limit):
        self.path = path
        self.defaults = (limit, test_limit)
        self.limit = limit
        self.test_limit = test_limit
        self.test = []
        self.ext = EXT_DEFAULT
        self.scan = []
        self.exclude = []
        self.entries = {}
        self.settings = []
        self.lines = text.splitlines()
        for n, raw in enumerate(self.lines, 1):
            words = raw.split("#", 1)[0].split()
            if not words:
                continue
            key, rest = words[0], words[1:]
            if key in SETTING_KEYS:
                self.settings.append(" ".join(words))
            if key == "limit" and len(rest) == 1 and rest[0].isdigit():
                self.limit = int(rest[0])
            elif key == "test-limit" and len(rest) == 1 and rest[0].isdigit():
                self.test_limit = int(rest[0])
            elif key == "test" and rest:
                self.test.extend(rest)
            elif key == "ext" and rest:
                self.ext = tuple(rest)
            elif key == "scan" and rest:
                odd = [r for r in rest if not PLAIN_PATH.match(r)]
                if odd:
                    raise SetupError(f"{path}:{n}: scan `{odd[0]}` is no plain directory path")
                self.scan.extend(r.rstrip("/") for r in rest)
            elif key == "exclude" and len(rest) == 1:
                self.exclude.append(rest[0])
            elif len(rest) == 1 and rest[0].isdigit():
                if key in self.entries:
                    raise SetupError(f"{path}:{n}: {key} is listed twice")
                self.entries[key] = int(rest[0])
            else:
                raise SetupError(f"{path}:{n}: unreadable line: {raw.strip()}")
        self.test = self.test or TEST_DEFAULT

    def in_scope(self, rel):
        if not rel.endswith(self.ext):
            return False
        if self.scan and not any(rel.startswith(d + "/") for d in self.scan):
            return False
        return not any(fnmatch.fnmatchcase(rel, g) for g in self.exclude)

    def is_test(self, rel):
        return any(fnmatch.fnmatchcase(rel, g) for g in self.test)

    def limit_of(self, rel):
        return self.test_limit if self.is_test(rel) else self.limit

    def budget(self, rel):
        return self.entries.get(rel, self.limit_of(rel))

    def reparse(self, text, path):
        """Another allowlist read with the same default limits."""
        return Allow(text, path, *self.defaults)


def git(root, *args, binary=False):
    out = subprocess.run(["git", "-C", root, *args], capture_output=True)
    if out.returncode != 0:
        raise SetupError(f"git {' '.join(args)}: {out.stderr.decode(errors='replace').strip()}")
    return out.stdout if binary else out.stdout.decode()


def listed(root, allow):
    """The in-scope files git sees under the scan directories, tracked or
    untracked and not ignored, that exist in the worktree."""
    out = git(root, "--literal-pathspecs", "ls-files", "-z", "--cached", "--others",
              "--exclude-standard", "--", *(allow.scan or ["."]))
    return [rel for rel in sorted(set(filter(None, out.split("\0"))))
            if allow.in_scope(rel) and os.path.isfile(os.path.join(root, rel))]


def empty_scans(measures, allow):
    """The scan directories holding no measured file: gone, renamed, or
    misspelt, each one a module the lint silently stopped holding."""
    return [f"{allow.path} scans {d}, which holds no file it checks: name the module's path"
            for d in allow.scan if not any(rel.startswith(d + "/") for rel in measures)]


def staged(root, allow):
    """The in-scope files the index adds, copies, modifies or renames."""
    out = git(root, "diff", "--cached", "--name-only", "-z", "--diff-filter=ACMR")
    return [rel for rel in filter(None, out.split("\0")) if allow.in_scope(rel)]


def rev_exists(root, rev):
    out = subprocess.run(["git", "-C", root, "rev-parse", "--verify", "-q", f"{rev}^{{commit}}"],
                         capture_output=True)
    return out.returncode == 0


def default_base(root):
    if not rev_exists(root, "HEAD"):
        return None
    if rev_exists(root, "develop"):
        return git(root, "merge-base", "HEAD", "develop").strip()
    return "HEAD"


def load_allow(root, allow_rel, limit, test_limit):
    path = os.path.join(root, allow_rel)
    if not os.path.isfile(path):
        raise SetupError(f"no allowlist at {path}")
    with open(path) as f:
        return Allow(f.read(), allow_rel, limit, test_limit)


def staged_allow(root, allow_rel, limit, test_limit):
    """The index's allowlist, else the worktree's."""
    blob = subprocess.run(["git", "-C", root, "show", f":{allow_rel}"], capture_output=True)
    if blob.returncode == 0:
        return Allow(blob.stdout.decode(), allow_rel, limit, test_limit)
    return load_allow(root, allow_rel, limit, test_limit)


def base_allow(root, base, allow):
    """The allowlist at `base`, or None when there is none to hold it to."""
    if base is None or not rev_exists(root, base):
        return None
    out = subprocess.run(["git", "-C", root, "show", f"{base}:{allow.path}"], capture_output=True)
    if out.returncode != 0:
        return None
    return allow.reparse(out.stdout.decode(), f"{base}:{allow.path}")


def renames(root, base, cached):
    """`{new path: old path}` of the renames git sees between `base` and the
    worktree (the index when `cached`)."""
    if base is None:
        return {}
    args = ["diff", "-M", "--name-status", "-z"] + (["--cached"] if cached else []) + [base]
    words = git(root, *args).split("\0")
    found, i = {}, 0
    while i < len(words) and words[i]:
        status = words[i]
        if status.startswith(("R", "C")):
            if status.startswith("R"):
                found[words[i + 2]] = words[i + 1]
            i += 3
        else:
            i += 2
    return found


def moved_dirs(root, dirs, renamed, cached):
    """`{old: new}` of the `dirs` git sees renamed whole: `new` is the one
    directory below which renamed files keep their path under `old`, every file
    renamed from under `old` lands under `new`, and no file is left under `old`
    (in the index when `cached`, else tracked or untracked)."""
    moved = {}
    for d in dirs:
        pre = d + "/"
        moves = [(new, old[len(pre):]) for new, old in renamed.items() if old.startswith(pre)]
        news = {new[:-len(rest) - 1] for new, rest in moves if new.endswith("/" + rest)}
        if len(news) != 1:
            continue
        target = news.pop()
        if any(not new.startswith(target + "/") for new, _ in moves):
            continue
        extra = [] if cached else ["--others", "--exclude-standard"]
        if not git(root, "ls-files", "-z", "--cached", *extra, "--", d).strip("\0"):
            moved[d] = target
    return moved


def follow_moves(setting, moved):
    """`setting` with each `scan` or `exclude` path under a moved directory
    named under its new one."""
    key, *words = setting.split()
    if key not in ("scan", "exclude"):
        return setting

    def follow(word):
        for old, new in moved.items():
            if word.rstrip("/") == old or word.startswith(old + "/"):
                return new + word[len(old):]
        return word

    return " ".join([key] + [follow(w) for w in words])


def scan_grows(base_settings, settings):
    """Whether `settings` are `base_settings` with only `scan` directories
    added, the base already scanning a list (no `scan` scans the whole root,
    so a first `scan` line narrows it)."""
    def split(lines):
        scans = [w for x in lines if x.split()[0] == "scan" for w in x.split()[1:]]
        return scans, [x for x in lines if x.split()[0] != "scan"]

    base_scans, base_rest = split(base_settings)
    scans, rest = split(settings)
    return bool(base_scans) and rest == base_rest and set(base_scans) <= set(scans)


def against_base(allow, base, renamed, moved):
    """What `allow` loosens against its `base` version, whose settings `allow`
    keeps as they are, with their `scan` and `exclude` paths following the
    directories git sees `moved`, or with `scan` directories added."""
    if base is None:
        return []
    bad = []
    base_settings = [follow_moves(x, moved) for x in base.settings]
    if not (allow.settings == base.settings or allow.settings == base_settings
            or scan_grows(base_settings, allow.settings)):
        gone = [x for x in base_settings if x not in allow.settings]
        new = [x for x in allow.settings if x not in base_settings]
        change = "; ".join([f"was `{x}`" for x in gone] + [f"now `{x}`" for x in new])
        bad.append(f"{allow.path} changes its settings against {base.path} ({change or 'order'})")
    for rel, n in sorted(allow.entries.items()):
        old = base.entries.get(rel)
        if old is not None:
            if n > old:
                bad.append(f"{allow.path} lists {rel} at {n}, above its {old} at {base.path}")
            continue
        src = renamed.get(rel)
        if src in base.entries and src not in allow.entries and n <= base.entries[src]:
            continue
        bad.append(f"{allow.path} adds {rel} at {n}, which {base.path} does not list")
    return bad


def loosened(root, allow, base, since, cached):
    """What `allow` loosens against its version at `base`, following the
    renames git sees from `since` to the worktree (the index when `cached`)."""
    held = base_allow(root, base, allow)
    if held is None:
        return []
    renamed = renames(root, since, cached)
    return against_base(allow, held, renamed, moved_dirs(root, held.scan, renamed, cached))


def stale(measures, allow, unit):
    """The entries above their file's measure, naming a file at or under the
    limit, or naming a file gone or out of scope."""
    bad = []
    for rel, budget in sorted(allow.entries.items()):
        n = measures.get(rel)
        if n is None:
            bad.append(f"{allow.path} lists {rel}, which is gone or not scanned")
        elif n <= allow.limit_of(rel):
            bad.append(f"{allow.path} lists {rel} at {budget}, it is under the limit ({n} {unit})")
        elif n < budget:
            bad.append(f"{allow.path} lists {rel} at {budget}, it has {n} {unit}")
    return bad


def updated(allow, measures, renamed, unit):
    """The allowlist's lines with each entry lowered to its file's measure,
    moved to a renamed file's new path, or dropped at or under the limit or
    gone; never raised nor added. Returns `(lines, changes)`."""
    moved_to = {old: new for new, old in renamed.items()}
    out, changed = [], []
    for raw in allow.lines:
        words = raw.split("#", 1)[0].split()
        if len(words) == 2 and words[0] in allow.entries:
            rel, budget = words[0], int(words[1])
            n = measures.get(rel)
            new = moved_to.get(rel)
            if n is None and new in measures and new not in allow.entries:
                rel, n = new, measures[new]
                changed.append(f"moved {words[0]} -> {rel}")
                if n > allow.limit_of(rel):
                    raw = f"{rel} {min(n, budget)}"
            if n is None or n <= allow.limit_of(rel):
                changed.append(f"removed {rel} ({'gone' if n is None else f'{n} {unit}'})")
                continue
            if n < budget:
                changed.append(f"lowered {rel} {budget} -> {n}")
                note = raw.split("#", 1)[1] if "#" in raw else None
                raw = f"{rel} {n}" + (f"  #{note}" if note is not None else "")
        out.append(raw)
    return out, changed
