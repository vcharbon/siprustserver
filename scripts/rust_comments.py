"""The comments of a Rust source file, found by a lexer that knows literals.

`scan(text)` returns every comment as a `Comment`: its kind, its 1-based first
and last line, its text, and whether code shares its first line. String, raw
string, byte string, char and lifetime tokens are skipped, so a `//` inside a
literal is never a comment; block comments nest as Rust's do.

Kinds: `outer-doc` (`///`, `/** */`), `inner-doc` (`//!`, `/*! */`), `line`
(any other `//`) and `block` (any other `/* */`). `////` and `/***` are plain
comments, as rustdoc reads them.

A comment is **allowed in a body** when it is one `line` comment alone on its
line, with no comment line above or below it, that is a `// SAFETY: ...` note
whose next code line holds `unsafe`, or a bare ADR pointer (`// ADR-0051`,
`// ADR-0051 §3`, `// ADR-0009 §X7`, nothing after it). `banned(comments, text)`
returns the plain comments that are not.

`strip(text)` returns the source without its comments: a line that held only
comment text is dropped, a trailing comment and the whitespace before it are cut.
"""
import re
from dataclasses import dataclass

ADR_POINTER = re.compile(r"^// ADR-\d{4}( §[A-Z]?\d+(\.\d+)*)?\.?$")
SAFETY_NOTE = re.compile(r"^// SAFETY: \S")
DOC_KINDS = ("outer-doc", "inner-doc")


@dataclass
class Comment:
    kind: str
    start: int
    end: int
    text: str
    trailing: bool
    span: tuple

    @property
    def lines(self):
        return self.end - self.start + 1


def _kind_of_line(body):
    if body.startswith("///") and not body.startswith("////"):
        return "outer-doc"
    if body.startswith("//!"):
        return "inner-doc"
    return "line"


def _kind_of_block(body):
    if body.startswith("/**") and not body.startswith("/***") and body != "/**/":
        return "outer-doc"
    if body.startswith("/*!"):
        return "inner-doc"
    return "block"


def _skip_raw_string(text, i):
    """`i` at the `r` of `r#*"`; returns the index past the literal, or None."""
    j = i + 1
    hashes = 0
    while j < len(text) and text[j] == "#":
        hashes += 1
        j += 1
    if j >= len(text) or text[j] != '"':
        return None
    close = '"' + "#" * hashes
    end = text.find(close, j + 1)
    return len(text) if end < 0 else end + len(close)


def _skip_string(text, i):
    """`i` at the opening `"`; returns the index past the closing one."""
    j = i + 1
    while j < len(text):
        c = text[j]
        if c == "\\":
            j += 2
            continue
        if c == '"':
            return j + 1
        j += 1
    return j


_CHAR = re.compile(r"'(?:\\(?:x[0-9a-fA-F]{2}|u\{[0-9a-fA-F_]+\}|.)|[^\\'\n])'")


def _ident_before(text, i):
    return i > 0 and (text[i - 1].isalnum() or text[i - 1] == "_")


def literal_end(text, i):
    """The index past the string, raw string or char literal starting at `i`,
    or None when none starts there (a `b`/`c` prefix: the literal starts at
    the quote after it; a lifetime or label: no literal)."""
    c = text[i]
    if c == '"':
        return _skip_string(text, i)
    if c in "rbc" and not _ident_before(text, i):
        at = i + 1 if c in "bc" and text[i + 1 : i + 2] == "r" else i
        return _skip_raw_string(text, at) if text[at] == "r" else None
    if c == "'":
        m = _CHAR.match(text, i)
        return m.end() if m else None
    return None


def scan(text):
    comments = []
    line_starts = [0]
    for m in re.finditer("\n", text):
        line_starts.append(m.end())

    def line_of(pos):
        lo, hi = 0, len(line_starts) - 1
        while lo < hi:
            mid = (lo + hi + 1) // 2
            if line_starts[mid] <= pos:
                lo = mid
            else:
                hi = mid - 1
        return lo + 1

    def code_before(pos):
        start = line_starts[line_of(pos) - 1]
        return text[start:pos].strip() != ""

    i = 0
    n = len(text)
    while i < n:
        c = text[i]
        nxt = text[i + 1] if i + 1 < n else ""
        if c == "/" and nxt == "/":
            end = text.find("\n", i)
            end = n if end < 0 else end
            body = text[i:end]
            ln = line_of(i)
            comments.append(Comment(_kind_of_line(body), ln, ln, body, code_before(i), (i, end)))
            i = end
        elif c == "/" and nxt == "*":
            depth = 1
            j = i + 2
            while j < n and depth:
                if text.startswith("/*", j):
                    depth += 1
                    j += 2
                elif text.startswith("*/", j):
                    depth -= 1
                    j += 2
                else:
                    j += 1
            body = text[i:j]
            comments.append(
                Comment(_kind_of_block(body), line_of(i), line_of(max(i, j - 1)), body, code_before(i), (i, j))
            )
            i = j
        else:
            end = literal_end(text, i)
            i = end if end is not None else i + 1
    return comments


def blocks(comments):
    """Runs of whole-line comments of one kind on consecutive lines, as lists."""
    out = []
    for c in comments:
        prev = out[-1][-1] if out else None
        if (
            prev is not None
            and not c.trailing
            and not prev.trailing
            and c.kind == prev.kind
            and c.kind != "block"
            and c.start == prev.end + 1
        ):
            out[-1].append(c)
        else:
            out.append([c])
    return out


def _next_code_line(text, pos):
    for line in text[pos:].split("\n"):
        if line.strip():
            return line
    return ""


def _allowed(c, text):
    body = c.text.rstrip("\r")
    if c.kind != "line" or c.trailing:
        return False
    if ADR_POINTER.match(body):
        return True
    return bool(SAFETY_NOTE.match(body)) and re.search(r"\bunsafe\b", _next_code_line(text, c.span[1]))


def banned(comments, text):
    """The plain comments of `text` (scanned as `comments`) the rule bans."""
    out = []
    for blk in blocks(comments):
        if blk[0].kind in DOC_KINDS:
            continue
        if len(blk) == 1 and _allowed(blk[0], text):
            continue
        out.extend(blk)
    return out


def strip(text, keep=lambda c: False):
    """The source without the comments `keep` refuses (by default: all)."""
    chars = list(text)
    touched = set()
    for c in scan(text):
        if keep(c):
            continue
        a, b = c.span
        for k in range(a, b):
            if chars[k] != "\n":
                chars[k] = ""
        touched.update(range(c.start, c.end + 1))
    lines = "".join(chars).split("\n")
    kept = []
    for idx, line in enumerate(lines, 1):
        if idx in touched:
            if line.strip() == "":
                continue
            line = line.rstrip()
        kept.append(line)
    return "\n".join(kept)
