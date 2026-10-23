"""The branch's Rust lines against a base, as the panic-sites ratchet reads
them: renames, the lines it touches, the lines git sees moved, and the files
they moved out of.

A line is moved when git's `--color-moved=blocks` (whitespace ignored) finds
it in a block of at least 20 alphanumeric characters removed elsewhere in
the same diff, a deleted file's lines included; any other added line is
touched. Untracked files are diffed as new files through a temporary index
that holds them `--intent-to-add`, so a file code moved into need not be
added first.
"""
import os
import re
import shutil
import subprocess
import tempfile

ANSI = re.compile(r"\x1b\[[0-9;]*m")
COLOR = re.compile(r"\x1b\[([0-9;]*)m")
NEW_MOVED, OLD_MOVED = "35", "33"
COLORS = ["-c", "color.diff.new=green", "-c", "color.diff.old=red",
          "-c", "color.diff.meta=bold", "-c", "color.diff.frag=cyan",
          *[x for part in ("newMoved", "newMovedAlternative", "newMovedDimmed",
                           "newMovedAlternativeDimmed")
            for x in ("-c", f"color.diff.{part}=magenta")],
          *[x for part in ("oldMoved", "oldMovedAlternative", "oldMovedDimmed",
                           "oldMovedAlternativeDimmed")
            for x in ("-c", f"color.diff.{part}=yellow")]]


def run(root, *args, env=None):
    out = subprocess.run(["git", "-C", root, *args], capture_output=True,
                         env=None if env is None else {**os.environ, **env})
    if out.returncode != 0:
        raise RuntimeError(f"git {' '.join(args)}: "
                           f"{out.stderr.decode(errors='replace').strip()}")
    return out.stdout.decode(errors="replace")


def renames(root, base, cached):
    """`{new path: old path}` of the renames git sees between `base` and the
    worktree (the index when `cached`)."""
    if base is None:
        return {}
    args = ["diff", "-M", "--name-status", "-z"] + (["--cached"] if cached else []) + [base]
    words = run(root, *args).split("\0")
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


def colored_diff(root, base):
    """The colored `-U0` diff of the worktree's .rs files against `base`,
    untracked ones as new files."""
    untracked = [rel for rel in run(root, "ls-files", "-z", "--others", "--exclude-standard",
                                    "--", "*.rs").split("\0") if rel]
    args = [*COLORS, "diff", "--color=always", "--color-moved=blocks",
            "--color-moved-ws=ignore-all-space", "-U0", "--no-ext-diff", "--no-renames",
            "--src-prefix=a/", "--dst-prefix=b/", base, "--", "*.rs"]
    if not untracked:
        return run(root, *args)
    with tempfile.TemporaryDirectory() as tmp:
        index = os.path.join(tmp, "index")
        real = run(root, "rev-parse", "--path-format=absolute", "--git-path", "index").strip()
        if os.path.isfile(real):
            shutil.copyfile(real, index)
        env = {"GIT_INDEX_FILE": index}
        run(root, "add", "--intent-to-add", "--", *untracked, env=env)
        return run(root, *args, env=env)


def lines(root, base):
    """`(touched, moved, moved_from)`: `{file: set of new-side line numbers}`
    the branch touches and moves in, and the files lines moved out of."""
    touched, moved, moved_from = {}, {}, set()
    if base is None:
        return touched, moved, moved_from
    old_file = new_file = None
    line = 0
    for raw in colored_diff(root, base).splitlines():
        text = ANSI.sub("", raw)
        color = COLOR.match(raw)
        color = color.group(1) if color else None
        if text.startswith("--- "):
            old_file = text[6:].rstrip("\t") if text.startswith("--- a/") else None
        elif text.startswith("+++ "):
            new_file = text[6:].rstrip("\t") if text.startswith("+++ b/") else None
        elif text.startswith("@@ "):
            line = int(text.split()[2][1:].split(",")[0])
        elif text.startswith("+") and new_file:
            (moved if color == NEW_MOVED else touched).setdefault(new_file, set()).add(line)
            line += 1
        elif text.startswith("-") and old_file and color == OLD_MOVED:
            moved_from.add(old_file)
    return touched, moved, moved_from
