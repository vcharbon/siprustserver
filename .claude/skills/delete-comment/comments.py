#!/usr/bin/env python3
"""The comments of Rust sources: an inventory, a stripped copy, a code-only
diff, and the lines a deletion would shed.

  comments.py list PATH...                       one JSON line per comment block
  comments.py strip FILE                         the file without its comments
  comments.py apply [-C DIR] CLASSIFIED...       edit the files as classified
  comments.py same-code -C DIR --base REV PATH...
  comments.py estimate PATH...

PATH is a `.rs` file or a directory searched for them. `list` prints, per
block of consecutive whole-line comments of one kind (a block comment and a
trailing comment are blocks alone): `id` (`<file>:<line>`, or
`<file>:<line>:<column>` when two blocks start on one line), `file`, `start`,
`end`, `kind` (`outer-doc`, `inner-doc`, `line`, `block`), `trailing`,
`banned` (by the in-body rule, scripts/rust_comments.py) and `text`.

`apply` reads classification lines (CLASSIFIED: `.jsonl` files, or directories
of them; DIR the root the ids are relative to, by default the current
directory): `id` and `action` (`keep`, `delete`, `rewrite`, `adr-pointer`,
`to-doc`), with `text` for the last three and `insert_before` for `to-doc`.
Every id must name a block `list` finds now, once; otherwise nothing is
edited. `delete` cuts a block that fills its lines with them, else its own
characters and the blanks between it and the code it shares a line with; `rewrite` and `adr-pointer` replace the lines of
a block that fills them by `text`; `to-doc` inserts `text` before line
`insert_before` and cuts the block. Run `cargo fmt` after.

`same-code` compares each changed file at REV with the worktree, both stripped
of comments and run through rustfmt (the repository's `rustfmt.toml`), blank
lines dropped: a diff that only edits comments, and the formatting rustfmt then
picks, exits 0. A file whose code differs, or that rustfmt cannot format, is
named and the run exits 1.

`estimate` counts, per PATH, the lines of its files now and after rustfmt of
the copy without (a) its plain comments, (b) its doc comments, (c) both, split
into test files (under `tests/`, a `tests.rs` or `*_tests.rs`) and the rest.
"""
import argparse
import collections
import fnmatch
import json
import os
import subprocess
import sys

REPO = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", ".."))
sys.path.insert(0, os.path.join(REPO, "scripts"))
import rust_comments  # noqa: E402  (the lexer the comments lint reads, scripts/rust_comments.py)

TEST_GLOBS = ["tests/*", "*/tests/*", "*/tests.rs", "*_tests.rs"]
EDITION = "2021"


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


def line_start(text, pos):
    return text.rfind("\n", 0, pos) + 1


def identified(rel, text):
    """`{id: block}` of a source's comment blocks, ids unique (`list`'s rule)."""
    blks = rust_comments.blocks(rust_comments.scan(text))
    per_line = collections.Counter(b[0].start for b in blks)
    out = {}
    for b in blks:
        first = b[0]
        col = first.span[0] - line_start(text, first.span[0]) + 1
        key = f"{rel}:{first.start}" if per_line[first.start] == 1 else f"{rel}:{first.start}:{col}"
        out[key] = b
    return out


def cmd_list(paths):
    for path in rust_files(paths):
        text = read(path)
        banned = {id(c) for c in rust_comments.banned(rust_comments.scan(text), text)}
        for key, blk in identified(path, text).items():
            first, last = blk[0], blk[-1]
            print(json.dumps({
                "id": key,
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


def file_of(row_id):
    """The file part of an id: `<file>:<line>` or `<file>:<line>:<column>`."""
    head, _, last = row_id.rpartition(":")
    rest, _, mid = head.rpartition(":")
    return rest if mid.isdigit() and last.isdigit() and rest else head


def fills_its_lines(text, a, b):
    """Whether only blanks share the lines of `text[a:b]`."""
    end = text.find("\n", b)
    end = len(text) if end < 0 else end
    return text[line_start(text, a):a].strip() == "" and text[b:end].strip() == ""


def edits_of(text, blk, row):
    """`[(start, end, replacement)]` character edits for one block, by its row."""
    a, b = blk[0].span[0], blk[-1].span[1]
    action = row["action"]
    if action == "keep":
        return []
    whole = fills_its_lines(text, a, b)
    if whole:
        cut_a = line_start(text, a)
        cut_b = b + 1 if text[b:b + 1] == "\n" else b
    elif text[line_start(text, a):a].strip() == "":
        cut_a, cut_b = a, b
        while cut_b < len(text) and text[cut_b] in " \t":
            cut_b += 1
    else:
        cut_a, cut_b = a, b
        while cut_a > line_start(text, a) and text[cut_a - 1] in " \t":
            cut_a -= 1
    if action == "delete":
        return [(cut_a, cut_b, "")]
    if not whole:
        raise ValueError(f"{row['id']}: code shares its lines; it takes `delete` only")
    new = row.get("text")
    if not new:
        raise ValueError(f"{row['id']}: `{action}` needs `text`")
    if action in ("rewrite", "adr-pointer"):
        return [(cut_a, cut_b, new + "\n")]
    lines = text.split("\n")
    at = int(row["insert_before"])
    pos = sum(len(l) + 1 for l in lines[:at - 1])
    return [(cut_a, cut_b, ""), (pos, pos, new + "\n")]


def cmd_apply(root, paths):
    rows = classified(paths)
    bad = [r for r in rows if r.get("action") not in ACTIONS]
    if bad:
        print(f"apply: {bad[0].get('id')}: unknown action {bad[0].get('action')!r}; nothing edited",
              file=sys.stderr)
        return 2
    twice = sorted(i for i, n in collections.Counter(r["id"] for r in rows).items() if n > 1)
    if twice:
        print(f"apply: {len(twice)} id(s) classified twice, first {twice[0]}; nothing edited",
              file=sys.stderr)
        return 2
    per_file = {}
    for row in rows:
        per_file.setdefault(file_of(row["id"]), []).append(row)
    plan = {}
    for rel, frows in per_file.items():
        text = read(os.path.join(root, rel))
        found = identified(rel, text)
        missing = [r["id"] for r in frows if r["id"] not in found]
        if missing:
            print(f"apply: {len(missing)} id(s) of {rel} name no block now, first {missing[0]}; "
                  "nothing edited", file=sys.stderr)
            return 2
        frows.sort(key=lambda r: found[r["id"]][0].span[0])
        try:
            plan[rel] = (text, [e for r in frows for e in edits_of(text, found[r["id"]], r)])
        except ValueError as e:
            print(f"apply: {e}; nothing edited", file=sys.stderr)
            return 2
    changed = 0
    for rel, (text, edits) in plan.items():
        # Back to front so earlier offsets hold; at one offset a removal before
        # an insertion, and insertions last-classified first so they read in order.
        order = sorted(range(len(edits)), key=lambda k: (edits[k][0], edits[k][1], k), reverse=True)
        for a, b, new in (edits[k] for k in order):
            text = text[:a] + new + text[b:]
        with open(os.path.join(root, rel), "w", encoding="utf-8") as f:
            f.write(text)
        changed += bool(edits)
    print(f"apply: {len(rows)} classified block(s), {changed} file(s) edited")
    return 0


def rustfmt(text, config):
    """`(formatted, ok)`: rustfmt of `text` under `config`."""
    out = subprocess.run(["rustfmt", "--edition", EDITION, "--config-path", config, "--emit", "stdout"],
                         input=text, capture_output=True, text=True)
    ok = out.returncode == 0 and bool(out.stdout)
    return (out.stdout if ok else text), ok


def code_of(text, config):
    formatted, ok = rustfmt(rust_comments.strip(text) + "\n", config)
    return [l for l in formatted.split("\n") if l.strip()], ok


def cmd_same_code(root, base, paths):
    config = os.path.join(root, "rustfmt.toml")
    if not os.path.isfile(config):
        config = os.path.join(REPO, "rustfmt.toml")
    changed = []
    files = subprocess.run(
        ["git", "-C", root, "diff", "--name-only", "--no-renames", base, "--", *paths],
        capture_output=True, text=True, check=True).stdout.split()
    for rel in files:
        if not rel.endswith(".rs"):
            continue
        old = subprocess.run(["git", "-C", root, "show", f"{base}:{rel}"], capture_output=True, text=True)
        new_path = os.path.join(root, rel)
        sides = [code_of(old.stdout, config) if old.returncode == 0 else (None, True),
                 code_of(read(new_path), config) if os.path.isfile(new_path) else (None, True)]
        if not all(ok for _, ok in sides):
            changed.append(rel)
            print(f"same-code: {rel}: rustfmt cannot format it; not proved")
        elif sides[0][0] != sides[1][0]:
            changed.append(rel)
            print(f"same-code: {rel}: code differs from {base}")
    print(f"same-code: {len(files)} file(s) changed, {len(changed)} with code changes")
    return 1 if changed else 0


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
            row[1] += count(rustfmt(rust_comments.strip(text, plain) + "\n", config)[0])
            row[2] += count(rustfmt(rust_comments.strip(text, docs) + "\n", config)[0])
            row[3] += count(rustfmt(rust_comments.strip(text) + "\n", config)[0])
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
