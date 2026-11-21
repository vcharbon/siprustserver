#!/usr/bin/env python3
"""The comments of Rust sources: an inventory, a stripped copy, a code-only
diff, and the lines a deletion would shed.

  comments.py list PATH...              one JSON line per comment block
  comments.py strip FILE                the file without its comments
  comments.py apply [-C DIR] CLASSIFIED...          edit the files as classified
  comments.py same-code -C DIR --base REV PATH...
  comments.py estimate PATH...

PATH is a `.rs` file or a directory searched for them. `list` prints, per
block of consecutive whole-line comments of one kind (a block comment and a
trailing comment are blocks alone): `id` (`<file>:<start>`), `file`, `start`,
`end`, `kind` (`outer-doc`, `inner-doc`, `line`, `block`), `trailing`,
`banned` (by the in-body rule, scripts/rust_comments.py) and `text`.

`apply` reads classification lines (CLASSIFIED: `.jsonl` files, or directories
of them; DIR the root the ids are relative to, by default the current
directory): `id` and `action` (`keep`, `delete`, `rewrite`, `adr-pointer`,
`to-doc`), with `text` for the last three and `insert_before` for `to-doc`.
Each id must name a block `list` finds now, so the edits land on the lines
classified; an id it does not find refuses the whole run before any edit.
`delete` cuts the block (a trailing one off its line), `rewrite` and
`adr-pointer` replace its lines by `text`, `to-doc` inserts `text` before line
`insert_before` and cuts the block. Run `cargo fmt` after.

`same-code` compares each file at REV with the worktree, both without comments,
whitespace, and the trailing commas rustfmt adds when a comment's removal lets
it reflow a list: a diff that only edits comments (and the formatting rustfmt
then picks) exits 0; otherwise it names each file whose code changed and exits 1.

`estimate` counts, per PATH, the lines of its files now and after rustfmt of
the copy without (a) its plain comments, (b) its doc comments, (c) both, split
into test files (under `tests/`, a `tests.rs` or `*_tests.rs`) and the rest.
"""
import argparse
import fnmatch
import json
import os
import re
import subprocess
import sys

REPO = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", ".."))
sys.path.insert(0, os.path.join(REPO, "scripts"))
import rust_comments  # noqa: E402  (the lexer the comments lint reads, scripts/rust_comments.py)

TEST_GLOBS = ["tests/*", "*/tests/*", "*/tests.rs", "*_tests.rs"]


def rust_files(paths):
    out = []
    for p in paths:
        if os.path.isdir(p):
            for d, dirs, files in os.walk(p):
                dirs[:] = sorted(x for x in dirs if x != "target")
                out.extend(os.path.join(d, f) for f in sorted(files) if f.endswith(".rs"))
        elif p.endswith(".rs"):
            out.append(p)
    return out


def read(path):
    with open(path, encoding="utf-8") as f:
        return f.read()


def cmd_list(paths):
    for path in rust_files(paths):
        found = rust_comments.scan(read(path))
        banned = {id(c) for c in rust_comments.banned(found)}
        for blk in rust_comments.blocks(found):
            first, last = blk[0], blk[-1]
            print(json.dumps({
                "id": f"{path}:{first.start}",
                "file": path,
                "start": first.start,
                "end": last.end,
                "kind": first.kind,
                "trailing": first.trailing,
                "banned": id(first) in banned,
                "text": "\n".join(c.text for c in blk),
            }, ensure_ascii=False))
    return 0


ACTIONS = ("keep", "delete", "rewrite", "adr-pointer", "to-doc")


def classified(paths):
    out = []
    for p in paths:
        files = sorted(os.path.join(p, f) for f in os.listdir(p) if f.endswith(".jsonl")) \
            if os.path.isdir(p) else [p]
        for f in files:
            with open(f, encoding="utf-8") as fh:
                out.extend(json.loads(line) for line in fh if line.strip())
    return out


def blocks_of(root, rel):
    found = rust_comments.scan(read(os.path.join(root, rel)))
    return {f"{rel}:{b[0].start}": b for b in rust_comments.blocks(found)}


def edits_of(blk, row):
    """`[(line, remove_count, new_lines, trailing_span)]` for one block, by its row."""
    first, last = blk[0], blk[-1]
    action = row["action"]
    if action == "keep":
        return []
    if first.trailing:
        if action != "delete":
            raise ValueError(f"{row['id']}: a trailing comment takes `delete` only")
        return [(first.start, 0, None, True)]
    count = last.end - first.start + 1
    if action == "delete":
        return [(first.start, count, [], False)]
    text = row.get("text")
    if not text:
        raise ValueError(f"{row['id']}: `{action}` needs `text`")
    lines = text.split("\n")
    if action in ("rewrite", "adr-pointer"):
        return [(first.start, count, lines, False)]
    at = int(row["insert_before"])
    return [(first.start, count, [], False), (at, 0, lines, False)]


def cmd_apply(root, paths):
    rows = classified(paths)
    per_file = {}
    for row in rows:
        if row.get("action") not in ACTIONS:
            print(f"apply: {row.get('id')}: unknown action {row.get('action')!r}", file=sys.stderr)
            return 2
        rel, _, _ = row["id"].rpartition(":")
        per_file.setdefault(rel, []).append(row)
    plan = {}
    for rel, frows in per_file.items():
        frows.sort(key=lambda r: int(r["id"].rpartition(":")[2]))
        found = blocks_of(root, rel)
        missing = [r["id"] for r in frows if r["id"] not in found]
        if missing:
            print(f"apply: {len(missing)} id(s) of {rel} name no block now, first {missing[0]}; "
                  "nothing edited", file=sys.stderr)
            return 2
        try:
            plan[rel] = (found, [e for r in frows for e in edits_of(found[r["id"]], r)])
        except ValueError as e:
            print(f"apply: {e}; nothing edited", file=sys.stderr)
            return 2
    changed = 0
    for rel, (found, edits) in plan.items():
        path = os.path.join(root, rel)
        lines = read(path).split("\n")
        # Bottom-up so earlier line numbers hold; at one line, a removal before an
        # insertion, and insertions last-classified first so they read in file order.
        order = sorted(range(len(edits)), key=lambda k: (edits[k][0], edits[k][1], k), reverse=True)
        for line, count, new, trailing in (edits[k] for k in order):
            if trailing:
                c = found[f"{rel}:{line}"][0]
                text = lines[line - 1]
                col = text.find(c.text)
                lines[line - 1] = text[:col].rstrip()
            else:
                lines[line - 1:line - 1 + count] = new
        with open(path, "w", encoding="utf-8") as f:
            f.write("\n".join(lines))
        changed += bool(edits)
    print(f"apply: {len(rows)} classified block(s), {changed} file(s) edited")
    return 0


def code_only(text):
    """The source's code with comments, whitespace and the trailing commas
    rustfmt adds or drops when it reflows a list removed."""
    code = re.sub(r"\s+", "", rust_comments.strip(text))
    return re.sub(r",(?=[}\])])", "", code)


def cmd_same_code(root, base, paths):
    changed = []
    files = subprocess.run(
        ["git", "-C", root, "diff", "--name-only", "--no-renames", base, "--", *paths],
        capture_output=True, text=True, check=True).stdout.split()
    for rel in files:
        if not rel.endswith(".rs"):
            continue
        old = subprocess.run(["git", "-C", root, "show", f"{base}:{rel}"], capture_output=True, text=True)
        new_path = os.path.join(root, rel)
        old_code = code_only(old.stdout) if old.returncode == 0 else None
        new_code = code_only(read(new_path)) if os.path.isfile(new_path) else None
        if old_code != new_code:
            changed.append(rel)
            print(f"same-code: {rel}: code differs from {base}")
    print(f"same-code: {len(files)} file(s) changed, {len(changed)} with code changes")
    return 1 if changed else 0


def rustfmt(text, config):
    out = subprocess.run(["rustfmt", "--edition", "2021", "--config-path", config, "--emit", "stdout"],
                         input=text, capture_output=True, text=True)
    return out.stdout if out.returncode == 0 and out.stdout else text


def count(text):
    return text.count("\n") + (0 if text.endswith("\n") or not text else 1)


def cmd_estimate(paths, config):
    for top in paths:
        rows = {"src": [0, 0, 0, 0], "tests": [0, 0, 0, 0]}
        for path in rust_files([top]):
            rel = os.path.relpath(path, top)
            part = "tests" if any(fnmatch.fnmatchcase(rel, g) for g in TEST_GLOBS) else "src"
            text = read(path)
            plain = lambda c: c.kind in rust_comments.DOC_KINDS  # keep docs, cut plain
            docs = lambda c: c.kind not in rust_comments.DOC_KINDS  # keep plain, cut docs
            row = rows[part]
            row[0] += count(text)
            row[1] += count(rustfmt(rust_comments.strip(text, plain) + "\n", config))
            row[2] += count(rustfmt(rust_comments.strip(text, docs) + "\n", config))
            row[3] += count(rustfmt(rust_comments.strip(text) + "\n", config))
        for part, (now, a, b, c) in rows.items():
            print(json.dumps({"path": top, "part": part, "lines": now,
                              "shed_plain": now - a, "shed_doc": now - b, "shed_all": now - c}))
    return 0


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = p.add_subparsers(dest="cmd", required=True)
    sub.add_parser("list").add_argument("paths", nargs="+")
    sub.add_parser("strip").add_argument("file")
    ap = sub.add_parser("apply")
    ap.add_argument("-C", dest="root", default=".")
    ap.add_argument("paths", nargs="+")
    s = sub.add_parser("same-code")
    s.add_argument("-C", dest="root", default=".")
    s.add_argument("--base", required=True)
    s.add_argument("paths", nargs="+")
    e = sub.add_parser("estimate")
    e.add_argument("--config", default=os.path.join(REPO, "rustfmt.toml"))
    e.add_argument("paths", nargs="+")
    a = p.parse_args()
    if a.cmd == "list":
        return cmd_list(a.paths)
    if a.cmd == "strip":
        sys.stdout.write(rust_comments.strip(read(a.file)))
        return 0
    if a.cmd == "apply":
        return cmd_apply(a.root, a.paths)
    if a.cmd == "same-code":
        return cmd_same_code(a.root, a.base, a.paths)
    return cmd_estimate(a.paths, a.config)


if __name__ == "__main__":
    sys.exit(main())
