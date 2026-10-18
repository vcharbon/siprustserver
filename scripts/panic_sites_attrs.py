"""The lint attributes of Rust sources the panic-sites ratchet reads.

The pass force-warns the counted lints, so no source attribute hides a site
from it. A site is exempt only when one `#[expect(lint, reason = "…")]` on an
item or statement covers it and no other site of that lint (`expects`).
Attributes that would hide sites from a plain clippy run are refused all the
same, and so is a `cfg` that keeps code out of the pass (`refusals`).
Comments and the contents of string and char literals are blanked before any
attribute is read.
"""
import re

from panic_sites_lints import COUNTED, DENIED, HIDING_GROUPS

OPEN = re.compile(r"#\s*(!)?\s*\[")
META = re.compile(r"([A-Za-z_][\w:\s]*?)\s*(?:\((.*)\))?\s*$", re.S)
RAW = re.compile(r"[bc]?r(#*)\"")
CHAR = re.compile(r"'(\\(?:x[0-9A-Fa-f]{2}|u\{[0-9A-Fa-f]{1,6}\}|.)|[^\\'\n])'")
QUALIFIERS = re.compile(r"\s*(?:#\s*\[|pub\s*\(|pub\b|unsafe\b|auto\b|default\b)")
CONTAINERS = ("mod", "impl", "trait")
ZERO = {f"clippy::{lint}" for lint in DENIED}
ZERO_TEXT = "#![cfg_attr(not(test), deny(" + ", ".join(sorted(ZERO)) + "))]"


def blank(text):
    """`text` with comments and literal contents as spaces, newlines kept, so
    every offset still names the same line."""
    out, i, n = list(text), 0, len(text)

    def fill(a, b):
        for k in range(a, b):
            if out[k] != "\n":
                out[k] = " "

    while i < n:
        if text.startswith("//", i):
            j = text.find("\n", i)
            j = n if j < 0 else j
            fill(i, j)
            i = j
        elif text.startswith("/*", i):
            depth, j = 0, i
            while j < n:
                if text.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif text.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                    if depth == 0:
                        break
                else:
                    j += 1
            fill(i, j)
            i = j
        elif (m := RAW.match(text, i)) and (i == 0 or not (text[i - 1].isalnum() or
                                                          text[i - 1] == "_")):
            end = text.find('"' + m.group(1), m.end())
            end = n if end < 0 else end
            fill(m.end(), end)
            i = end + 1 + len(m.group(1))
        elif text[i] == '"':
            j = i + 1
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            fill(i + 1, min(j, n))
            i = j + 1
        elif text[i] == "'" and (m := CHAR.match(text, i)):
            fill(i + 1, m.end() - 1)
            i = m.end()
        else:
            i += 1
    return "".join(out)


def closing(text, start):
    """The offset of the bracket closing the one at `start`."""
    depth = 0
    for k in range(start, len(text)):
        if text[k] in "[(":
            depth += 1
        elif text[k] in "])":
            depth -= 1
            if depth == 0:
                return k
    return len(text)


def split_top(args):
    """`args` split at its top-level commas."""
    parts, depth, cur = [], 0, []
    for ch in args:
        if ch in "([":
            depth += 1
        elif ch in ")]":
            depth -= 1
        if ch == "," and depth == 0:
            parts.append("".join(cur))
            cur = []
        else:
            cur.append(ch)
    parts.append("".join(cur))
    return [p.strip() for p in parts if p.strip()]


def metas(body, in_cfg=False):
    """`(kind, lints, has_reason, in_cfg)` of each allow or expect `body`
    holds, a `cfg_attr` looked through."""
    m = META.match(body.strip())
    if not m:
        return []
    name, args = re.sub(r"\s+", "", m.group(1)), m.group(2) or ""
    if name == "cfg_attr":
        return [x for part in split_top(args)[1:] for x in metas(part, True)]
    if name not in ("allow", "expect"):
        return []
    items = [re.sub(r"\s+", "", x) for x in split_top(args)]
    lints = [x for x in items if not x.startswith("reason=")]
    return [(name, lints, len(lints) < len(items), in_cfg)]


def item_after(text, end):
    """The keyword of the item an outer attribute ending at `end` applies
    to, past further attributes and qualifiers."""
    i = end + 1
    while True:
        m = QUALIFIERS.match(text, i)
        if not m:
            break
        tail = m.group(0).rstrip()
        if tail.endswith("["):
            i = closing(text, m.end() - 1) + 1
        elif tail.endswith("("):
            i = closing(text, m.end() - 1) + 1
        else:
            i = m.end()
    word = re.match(r"\s*([A-Za-z_]\w*)", text[i:])
    return word.group(1) if word else ""


def predicates(body):
    """The `cfg` predicates of an attribute `body`: a `cfg`'s, a
    `cfg_attr`'s and those of the attributes it holds."""
    m = META.match(body.strip())
    if not m:
        return []
    name, args = re.sub(r"\s+", "", m.group(1)), m.group(2) or ""
    if name == "cfg":
        return [re.sub(r"\s+", "", args)]
    if name == "cfg_attr":
        parts = split_top(args)
        return [re.sub(r"\s+", "", parts[0])] + [p for x in parts[1:] for p in predicates(x)] \
            if parts else []
    return []


def unlinted(predicate):
    """Whether code under `predicate` is compiled out of the clippy pass,
    which builds with `clippy` and `debug_assertions` set."""
    return re.search(r"\bclippy\b", predicate) is not None or \
        "not(debug_assertions)" in predicate


def attributes(code):
    """`(start, bracket, end, inner, line)` of each attribute of the blanked
    `code`."""
    for m in OPEN.finditer(code):
        bracket = m.end() - 1
        yield m.start(), bracket, closing(code, bracket), m.group(1) is not None, \
            code.count("\n", 0, m.start()) + 1


def item_end(code, start):
    """The offset closing the item that begins at `start`: its first `;` at
    depth 0, or the `}` of its first top-level block."""
    depth = 0
    for k in range(start, len(code)):
        ch = code[k]
        if ch in "([{":
            depth += 1
        elif ch in ")]}":
            depth -= 1
            if ch == "}" and depth == 0:
                return k
        elif ch == ";" and depth == 0:
            return k
    return len(code)


def expects(text):
    """`(lints, first line, last line)` of each `#[expect(…, reason = "…")]`
    on one item or statement: the item's lines are where it may exempt its one
    site of each lint."""
    code, found = blank(text), []
    for _, bracket, end, inner, _ in attributes(code):
        for kind, lints, has_reason, in_cfg in metas(code[bracket + 1:end]):
            counted = {x for x in lints if x in COUNTED}
            if kind != "expect" or not counted or not has_reason or in_cfg or inner:
                continue
            if item_after(code, end) in CONTAINERS:
                continue
            stop = item_end(code, end + 1)
            found.append((counted, code.count("\n", 0, end) + 1, code.count("\n", 0, stop) + 1))
    return found


def refusals(text):
    """`(line, why)` of each attribute that silences a counted lint other
    than by a per-site `#[expect(…, reason = "…")]`, or keeps code out of the
    clippy pass by its `cfg`."""
    code, bad = blank(text), []
    for _, bracket, end, inner, line in attributes(code):
        body = code[bracket + 1:end]
        for predicate in predicates(body):
            if unlinted(predicate):
                bad.append((line, f"`cfg({predicate})` keeps code out of the clippy pass"))
        for kind, lints, has_reason, in_cfg in metas(body):
            groups = [x for x in lints if x in HIDING_GROUPS]
            counted = [x for x in lints if x in COUNTED]
            if groups:
                bad.append((line, f"`{kind}` of `{groups[0]}` silences panic-set lints"))
            if not counted:
                continue
            if kind == "allow":
                bad.append((line, f"`allow` of `{counted[0]}`; a provably safe site takes "
                                  "`#[expect(…, reason = \"…\")]`"))
            elif in_cfg:
                bad.append((line, f"`expect` of `{counted[0]}` under `cfg_attr`"))
            elif inner:
                bad.append((line, f"inner `#![expect({counted[0]})]` covers a whole module; "
                                  "mark the site"))
            elif (item := item_after(code, end)) in CONTAINERS:
                bad.append((line, f"`expect` of `{counted[0]}` on a whole `{item}`; "
                                  "mark the site"))
            elif not has_reason:
                bad.append((line, f"`expect` of `{counted[0]}` has no reason"))
    return bad


def zero_lints(text):
    """The lints a `#![cfg_attr(not(test), deny(…))]` of `text` denies, or
    None when it carries none."""
    code = blank(text)
    for _, bracket, end, inner, _ in attributes(code):
        if not inner:
            continue
        body = re.sub(r"\s+", "", code[bracket + 1:end])
        found = re.fullmatch(r"cfg_attr\(not\(test\),deny\(([^()]*)\)\)", body)
        if found:
            return {x for x in found.group(1).split(",") if x}
    return None
