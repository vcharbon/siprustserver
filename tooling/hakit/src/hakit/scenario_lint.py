"""The static rules every SIPp scenario of the search path follows (ADR-0049 §4.2).

SIPp ends a call without running any step of the scenario when a wait expires
without `ontimeout`, when the retransmissions of a `<send>` without
`ontimeout` run out, and when a message lands that `_unexp.main` does not
take. A UAC that ends a call that way writes no record. So a UAC scenario:

- bounds every wait: each recv group is entered on a recv with `timeout` and
  `ontimeout`, or follows a retransmitted send whose `ontimeout` ends it;
- gives `ontimeout` to every send SIPp retransmits and to every recv with a
  `timeout`;
- has an `_unexp.main` handler taking every response and every request;
- names the `next` of every optional recv, and lets no label a jump reaches
  follow a step with an unconditional `next` (the handler's resume applies the
  `next` of the step before the one it returns to);
- writes exactly one call-outcome record on every path to the end of the call.

A UAS writes no record; it follows the send, optional-recv and resume rules.
"""

from __future__ import annotations

import re
import xml.etree.ElementTree as ET
from dataclasses import dataclass, field
from pathlib import Path

STEP_TAGS = ("send", "recv", "nop", "pause", "sendCmd", "recvCmd")
HANDLER = "_unexp.main"
RECORD_MARK = "call-outcome.v1"
# Every status code a peer may send, and the methods a peer may send in a dialog.
CODES = [str(c) for c in range(100, 700)]
METHODS = ["INVITE", "ACK", "BYE", "CANCEL", "OPTIONS", "INFO", "UPDATE", "PRACK", "REFER", "NOTIFY", "MESSAGE"]
# SIPp's own keywords (src/message.cpp); any other bracketed word is a `-key`.
BUILTIN_KEYWORDS = frozenset(
    "remote_ip remote_host remote_port transport local_ip local_ip_type local_port server_ip media_ip "
    "rtpstream_audio_port rtpstream_video_port media_port auto_media_port media_ip_type call_number "
    "dynamic_id call_id cseq pid service branch msg_index next_url len peer_tag_param last_cseq_number "
    "last_message routes tdmmap clock_tick users userid timestamp date sipp_version".split()
)
_KEYWORD = re.compile(r"\[([A-Za-z_][A-Za-z0-9_.-]*)")
_DEFAULT = re.compile(r"^\s*hakit-key:\s*([a-z_][a-z0-9_]*)=(\S*)\s*$", re.MULTILINE)
_RENDERED = {
    "assignstr": ("value",),
    "log": ("message",),
    "warning": ("message",),
    "error": ("message",),
    "exec": ("command",),
    "lookup": ("file", "key"),
    "setdest": ("host", "port", "protocol"),
}
_NOT_KEYS = re.compile(r"^(field\d+|last_.*|branch-?\d*|authentication|fill|file|rtp_echo.*)$")


@dataclass
class Step:
    index: int
    el: ET.Element
    labels: list[str] = field(default_factory=lambda: [])

    @property
    def tag(self) -> str:
        return self.el.tag

    def get(self, name: str) -> str | None:
        return self.el.get(name)

    def writes_record(self) -> bool:
        return any(RECORD_MARK in (a.get("message") or "") for a in self.el.iter("log"))

    def resumes(self) -> bool:
        return any(True for _ in self.el.iter("jump"))


@dataclass
class Scenario:
    path: Path
    steps: list[Step]
    labels: dict[str, int]

    @property
    def is_uac(self) -> bool:
        first = next((s for s in self.steps if s.tag != "nop"), None)
        return first is not None and first.tag == "send"

    def target(self, label: str | None) -> int | None:
        return None if label is None else self.labels.get(label)


def parse(path: Path) -> Scenario:
    root = ET.parse(path).getroot()
    steps: list[Step] = []
    labels: dict[str, int] = {}
    pending: list[str] = []
    for el in root:
        if el.tag == "label":
            ident = el.get("id") or ""
            labels[ident] = len(steps)
            pending.append(ident)
        elif el.tag in STEP_TAGS:
            steps.append(Step(len(steps), el, pending))
            pending = []
    return Scenario(path, steps, labels)


def _substituted(root: ET.Element) -> list[str]:
    """The texts SIPp expands keywords in: message bodies and the action values
    it renders (never a regexp)."""
    out: list[str] = []
    for el in root.iter():
        if el.tag == "send" and el.text:
            out.append(el.text)
        elif el.tag in _RENDERED:
            out.extend(v for k, v in el.attrib.items() if k in _RENDERED[el.tag])
    return out


def key_defaults(path: Path) -> dict[str, str]:
    """The `-key` defaults a scenario declares in its comments, one
    `hakit-key: <name>=<value>` line each; a launcher passes them before the
    stream's own params, so a param overrides its default."""
    return dict(_DEFAULT.findall(path.read_text(encoding="latin-1")))


def scenario_keys(path: Path) -> set[str]:
    """The `-key` names a scenario reads: every keyword SIPp does not define itself."""
    keys: set[str] = set()
    for text in _substituted(ET.parse(path).getroot()):
        for name in _KEYWORD.findall(text):
            if name.startswith("$") or name in BUILTIN_KEYWORDS or _NOT_KEYS.match(name):
                continue
            keys.add(name)
    return keys


def _groups(sc: Scenario) -> list[list[Step]]:
    """Maximal runs of consecutive recv steps."""
    out: list[list[Step]] = []
    for s in sc.steps:
        joins = out and out[-1][-1].index == s.index - 1 and HANDLER not in s.labels
        if s.tag == "recv" and joins:
            out[-1].append(s)
        elif s.tag == "recv":
            out.append([s])
    return out


def _handler_group(sc: Scenario) -> list[Step]:
    start = sc.labels.get(HANDLER)
    for g in _groups(sc):
        if g[0].index == start:
            return g
    return []


def _matches(step: Step, attr: str, value: str) -> bool:
    pat = step.get(attr)
    if pat is None:
        return False
    if step.get("regexp_match") == "true":
        return re.search(pat, value) is not None
    return pat == value


def _bounded(group: list[Step], before: Step | None) -> bool:
    entries = [group[0]] + [s for s in group[1:] if s.labels]
    if all(s.get("timeout") and s.get("ontimeout") for s in entries):
        return True
    if before is None or before.tag != "send" or not before.get("retrans") or not before.get("ontimeout"):
        return False
    # A provisional stops the retransmissions of an INVITE, never those of
    # another request (RFC 3261 section 17.1.2.2).
    return not (before.el.text or "").strip().startswith("INVITE ")


def _unconditional_next(s: Step) -> bool:
    return s.get("next") is not None and s.get("test") is None


def _successors(sc: Scenario, s: Step) -> list[int]:
    """Where the call can go after step `s`; len(steps) is the end of the call."""
    if s.resumes():
        return []
    out: list[int] = []
    nxt = sc.target(s.get("next"))
    if s.tag == "recv":
        if nxt is not None:
            out.append(nxt)
        else:
            out.append(s.index + 1)
    else:
        if nxt is not None:
            out.append(nxt)
        if not _unconditional_next(s):
            out.append(s.index + 1)
    timeout = sc.target(s.get("ontimeout"))
    if timeout is not None:
        out.append(timeout)
    return out


def _group_of(sc: Scenario) -> dict[int, list[Step]]:
    return {s.index: g for g in _groups(sc) for s in g}


def _record_errors(sc: Scenario) -> list[str]:
    """Every path from the first step (and from the handler) to the end writes one record."""
    end = len(sc.steps)
    groups = _group_of(sc)
    handler = sc.labels.get(HANDLER)
    starts = [0] + ([handler] if handler is not None else [])
    seen: set[tuple[int, int]] = set()
    errors: set[str] = set()
    todo = [(i, 0, "the first step" if i == 0 else HANDLER) for i in starts]
    while todo:
        i, written, via = todo.pop()
        if (i, written) in seen:
            continue
        seen.add((i, written))
        if i >= end:
            if written == 0:
                errors.add(f"the call can end without a record (after {via})")
            continue
        s = sc.steps[i]
        if s.writes_record():
            written += 1
            if written > 1:
                errors.add(f"step {i} writes a second record on one path")
                continue
        here = f"step {i} ({s.tag}{', label ' + s.labels[0] if s.labels else ''})"
        if s.tag == "recv":
            # A wait takes any recv of its group; each continues on its own way.
            for r in groups.get(i, [s]):
                if r.index < i:
                    continue
                for n in _successors(sc, r):
                    todo.append((n, written, here))
        else:
            for n in _successors(sc, s):
                todo.append((n, written, here))
    return sorted(errors)


def lint(path: Path) -> list[str]:
    """The rule violations of one scenario file, each naming the file."""
    try:
        sc = parse(path)
    except ET.ParseError as e:
        return [f"{path}: not well-formed XML: {e}"]
    errs: list[str] = []
    for s in sc.steps:
        if s.tag == "send" and s.get("retrans") and not s.get("ontimeout"):
            errs.append(f"step {s.index}: a retransmitted send has no ontimeout")
        if s.tag == "recv" and s.get("timeout") and not s.get("ontimeout"):
            errs.append(f"step {s.index}: a recv with a timeout has no ontimeout")
        if s.tag == "recv" and s.get("optional") and not s.get("next"):
            errs.append(f"step {s.index}: an optional recv names no next")
        for label in s.labels if HANDLER in sc.labels else []:
            prev = sc.steps[s.index - 1] if s.index > 0 else None
            if label != HANDLER and prev is not None and _unconditional_next(prev):
                errs.append(f"label {label!r} follows step {prev.index}, whose unconditional next a resume would apply")
    for s in sc.steps:
        for attr in ("next", "ontimeout"):
            v = s.get(attr)
            if v is not None and v not in sc.labels:
                errs.append(f"step {s.index}: {attr} names no label {v!r}")
        if sc.target(s.get("ontimeout")) == len(sc.steps):
            errs.append(f"step {s.index}: ontimeout names the end, where SIPp counts the call failed")
    read = scenario_keys(path)
    errs.extend(f"hakit-key {k!r} has a default but no step reads it" for k in sorted(set(key_defaults(path)) - read))
    if sc.is_uac:
        errs.extend(_uac_errors(sc))
    return [f"{path}: {e}" for e in errs]


def _uac_errors(sc: Scenario) -> list[str]:
    errs: list[str] = []
    handler = _handler_group(sc)
    if not handler:
        errs.append(f"no {HANDLER} handler opening with its recvs")
    else:
        prev = sc.steps[handler[0].index - 1] if handler[0].index > 0 else None
        if prev is not None and not _unconditional_next(prev) and not prev.resumes():
            errs.append(f"step {prev.index} falls through into {HANDLER}")
        codes = [c for c in CODES if not any(_matches(r, "response", c) for r in handler)]
        if codes:
            errs.append(f"{HANDLER} takes no response {codes[0]}" + (f" (and {len(codes) - 1} more)" if len(codes) > 1 else ""))
        methods = [m for m in METHODS if not any(_matches(r, "request", m) for r in handler)]
        if methods:
            errs.append(f"{HANDLER} takes no request {', '.join(methods)}")
    hstart = handler[0].index if handler else -1
    for g in _groups(sc):
        if g[0].index == hstart:
            continue
        before = sc.steps[g[0].index - 1] if g[0].index > 0 else None
        if not _bounded(g, before):
            errs.append(f"step {g[0].index}: the wait is bounded by no timeout and no retransmitted send")
    if not any(s.writes_record() for s in sc.steps):
        errs.append("no step writes a call-outcome record")
    else:
        errs.extend(_record_errors(sc))
    return errs


def lint_all(scenarios: dict[str, Path]) -> list[str]:
    errs: list[str] = []
    for name in sorted(scenarios):
        if name.endswith(".xml"):
            errs.extend(lint(scenarios[name]))
    return errs
