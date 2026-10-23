"""The cargo commands an image builds with, as purity.py's closure check
needs them (ADR-0051 §1).

The closure check resolves each production binary as `cargo build -p
<binary>` alone resolves it. That holds for the shipped binary only when the
image builds it by exactly such a command: a binary built beside another
package, or with features of its own, takes a feature set no check saw. Each
`cargo build` of a listed Dockerfile is read here (continuation lines joined,
comment lines dropped, a RUN split into its commands on `&&`, `||`, `;`, `|`)
and judged against the binaries.
"""
import re
import shlex

SEPARATORS = re.compile(r"&&|\|\||;|\|")
FEATURE_FLAGS = ("--features", "-F", "--all-features", "--no-default-features")
WHOLE = ("--workspace", "--all")


class Command:
    """One `cargo build`: where it is, the packages and binaries it selects,
    whether it builds the whole workspace, and the feature flags it states."""

    def __init__(self, where, args):
        self.where = where
        self.packages, self.bins, self.features = [], [], []
        self.whole = False
        it = iter(args)
        for arg in it:
            if arg in ("-p", "--package", "--bin"):
                (self.packages if arg != "--bin" else self.bins).append(next(it, ""))
            elif arg.startswith("--package="):
                self.packages.append(arg.split("=", 1)[1])
            elif arg.startswith("--bin="):
                self.bins.append(arg.split("=", 1)[1])
            elif arg.startswith("-p") and len(arg) > 2:
                self.packages.append(arg[2:])
            elif arg in WHOLE:
                self.whole = True
            elif arg in FEATURE_FLAGS or arg.startswith(("--features=", "-F")):
                self.features.append(arg)
                if arg in ("--features", "-F"):
                    next(it, None)
        if not self.packages and not self.bins:
            self.whole = True

    def selects(self, binary):
        return self.whole or binary in self.packages or binary in self.bins

    def alone(self, binary):
        """It builds `binary` and nothing else, under no feature flag."""
        return (not self.whole and self.packages == [binary] and set(self.bins) <= {binary}
                and not self.features)


def logical_lines(text):
    """`(line number, text)` of each Dockerfile line, its continuations joined
    and comment lines dropped."""
    out, start, parts = [], None, []
    for n, line in enumerate(text.splitlines(), 1):
        if line.lstrip().startswith("#"):
            continue
        if start is None:
            start = n
        stripped = line.rstrip()
        if stripped.endswith("\\"):
            parts.append(stripped[:-1])
            continue
        parts.append(stripped)
        out.append((start, " ".join(parts)))
        start, parts = None, []
    if parts:
        out.append((start, " ".join(parts)))
    return out


def commands(rel, text):
    """Every `cargo build` of the Dockerfile `rel` holds."""
    found = []
    for n, line in logical_lines(text):
        for segment in SEPARATORS.split(line):
            try:
                words = shlex.split(segment)
            except ValueError:
                words = segment.split()
            for i, word in enumerate(words[:-1]):
                if word == "cargo" and words[i + 1] == "build":
                    found.append(Command(f"{rel}:{n}", words[i + 2:]))
                    break
    return found


def violations(binaries, files):
    """What the commands of `files` (`{rel: text}`) break: a command selecting
    a binary together with anything else or under a feature flag, and a binary
    no command builds alone."""
    cmds = [c for rel, text in sorted(files.items()) for c in commands(rel, text)]
    bad = []
    for c in cmds:
        for binary in binaries:
            if c.selects(binary) and not c.alone(binary):
                with_ = "the whole workspace" if c.whole else ", ".join(
                    [f"-p {p}" for p in c.packages if p != binary]
                    + [f"--bin {b}" for b in c.bins if b != binary] + c.features) \
                    or f"a selection other than `-p {binary}`"
                bad.append(f"{c.where} builds production binary {binary} with {with_}: "
                           "a production binary is built by a cargo command of its own")
    for binary in binaries:
        if not any(c.alone(binary) for c in cmds):
            bad.append(f"production binary {binary} is built alone by no `cargo build` of "
                       f"{', '.join(sorted(files)) or 'a listed Dockerfile'}")
    return bad
