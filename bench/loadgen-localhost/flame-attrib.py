#!/usr/bin/env python3
"""Self-time attribution table from inferno/pprof flamegraph SVGs.

Each SVG frame carries its *inclusive* sample count in its `<title>` and its
exact sample span in inferno's `fg:x` / `fg:w` rect attributes; self time is a
frame's count minus the counts of the frames stacked directly on it, and the
stack relation is the span containment one row up. Both are read out of the
file — nothing here is eyeballed off the picture, so a before/after pair of runs
is compared on numbers.

    ./flame-attrib.py results/baseline-pre-zerocopy/r*_loadgen.svg

Multiple SVGs of the same tier are summed: three short captures make one
statistically usable profile. Prints the upstreamneed-043 §2 frame table, the
parse and allocation bucket totals, and the overall top-20 self-time frames.
"""

import html
import re
import sys
from collections import defaultdict

# The frames upstreamneed 043 §2 tabulates, bucketed. Matched as a substring of
# the demangled symbol so a generic/inlined name still lands.
PARSE_FRAMES = [
    "split_top_level_commas",
    "memchr_naive",
    "read_header_value",
    "is_contained_in",
    "count_tag_params",
    "has_via_port_trailing_garbage",
    "run_utf8_validation",
    "next_code_point",
]
# Allocator entry points, matched on the symbol's LAST path segment so `alloc`
# means the allocator call and not every symbol with "alloc" in its generics.
# The first list is 043 §2's; the second is the rest of the family this build's
# symbol table emits (a different toolchain inlines the ladder differently, so
# the union is the honest allocation total).
ALLOC_FRAMES = [
    "try_allocate_in",
    "dealloc_nonnull",
    "do_reserve_and_handle",
    "finish_grow",
    "alloc",
]
ALLOC_FRAMES_EXTRA = [
    "dealloc",
    "alloc_impl_runtime",
    "grow_amortized",
    "grow_one",
    "with_capacity_in",
    "shrink_to_fit",
]
# Everything below the Rust allocation shim as well: the two runner tiers link
# jemalloc INTO the binary, so its internals (`cache_bin_alloc_impl`,
# `sz_index2size_lookup_impl`, `_rjem_*`, …) are attributed frames, and leaving
# them out understates the allocator by more than half. Note the asymmetry this
# creates: the loadgen links glibc malloc, and pprof blocklists `libc`, so its
# allocator internals are dropped from the profile — its allocator bucket is a
# floor, the runner tiers' is complete.
ALLOC_FAMILY_RE = re.compile(
    r"alloc|malloc|free|realloc|grow_|tcache|arena|_rjem|sz_(s2u|size2index|index2size)"
    r"|edata_|cache_bin|emap_|extent|slab|layout_to_flags",
    re.I,
)

FRAME_RE = re.compile(
    r"<title>(?P<name>.*?)\((?P<samples>\d+) samples?,\s*[\d.]+%\)</title>\s*"
    r'<rect[^>]*?\sy="(?P<y>[\d.]+)"[^>]*?fg:x="(?P<fx>\d+)"\s+fg:w="(?P<fw>\d+)"',
    re.S,
)


def parse_svg(path):
    """(name, samples, y, sample_start, sample_width) per frame box."""
    with open(path, encoding="utf-8", errors="replace") as fh:
        svg = fh.read()
    return [
        (
            m.group("name").strip(),
            int(m.group("samples")),
            float(m.group("y")),
            int(m.group("fx")),
            int(m.group("fw")),
        )
        for m in FRAME_RE.finditer(svg)
    ]


def self_times(frames):
    """Self samples per symbol, from the one-row-up span containment."""
    if not frames:
        return {}, 0
    root = max(frames, key=lambda f: f[1])
    total, root_y = root[1], root[2]

    rows = defaultdict(list)
    for f in frames:
        rows[f[2]].append(f)
    pitch = min((root_y - y for y in rows if y < root_y), default=0.0)

    self_by_name = defaultdict(int)
    for y, row in rows.items():
        above = rows.get(y - pitch, []) if pitch else []
        for name, samples, _y, fx, fw in row:
            covered = sum(c[1] for c in above if fx <= c[3] and c[3] + c[4] <= fx + fw)
            self_by_name[name] += samples - covered
    return self_by_name, total


def crate_inclusive(frames, marker="sip_message"):
    """Inclusive samples of the whole `marker` subtree.

    Sums the frames that ENTER the crate — a `marker` frame whose parent is not
    already one — so nested calls are counted once. Inclusive is the number the
    borrow-don't-own work moves: it carries the allocator time the parse drives,
    which self-time attributes to the allocator instead.
    """
    if not frames:
        return 0
    root_y = max(frames, key=lambda f: f[1])[2]
    rows = defaultdict(list)
    for f in frames:
        rows[f[2]].append(f)
    pitch = min((root_y - y for y in rows if y < root_y), default=0.0)

    total = 0
    for y, row in rows.items():
        below = rows.get(y + pitch, []) if pitch else []
        for name, samples, _y, fx, fw in row:
            if marker not in name:
                continue
            parent = next(
                (p for p in below if p[3] <= fx and fx + fw <= p[3] + p[4]), None
            )
            if parent is None or marker not in parent[0]:
                total += samples
    return total


def leaf(symbol):
    """The demangled symbol's last `::` segment, generics/trait-quals stripped."""
    name = html.unescape(symbol)
    prev = None
    while prev != name:
        prev = name
        name = re.sub(r"<[^<>]*>", "", name)
    name = name.strip()
    return name.rsplit("::", 1)[-1] if name else symbol


def main(paths):
    merged = defaultdict(int)
    total = 0
    inclusive = 0
    for p in paths:
        frames = parse_svg(p)
        st, tot = self_times(frames)
        for k, v in st.items():
            merged[k] += v
        total += tot
        inclusive += crate_inclusive(frames)
    if not total:
        print("no samples found", file=sys.stderr)
        return 1

    print(f"files: {len(paths)}   total on-CPU samples: {total}\n")
    print(f"{'frame (self-time)':<34} {'samples':>9} {'self %':>8}")
    print("-" * 53)
    by_leaf = defaultdict(int)
    for k, v in merged.items():
        by_leaf[leaf(k)] += v

    parse_total = 0
    for f in PARSE_FRAMES:
        n = by_leaf.get(f, 0)
        parse_total += n
        print(f"{f:<34} {n:>9} {100 * n / total:>7.2f}%")
    print("-" * 53)
    alloc_total = 0
    for f in ALLOC_FRAMES:
        n = by_leaf.get(f, 0)
        alloc_total += n
        print(f"{f:<34} {n:>9} {100 * n / total:>7.2f}%")
    extra_total = 0
    for f in ALLOC_FRAMES_EXTRA:
        n = by_leaf.get(f, 0)
        extra_total += n
        if n:
            print(f"{f + ' (this build)':<34} {n:>9} {100 * n / total:>7.2f}%")
    print("-" * 53)
    shim = set(ALLOC_FRAMES) | set(ALLOC_FRAMES_EXTRA)
    family_total = sum(
        v for k, v in by_leaf.items() if k in shim or ALLOC_FAMILY_RE.search(k)
    )
    print(f"{'PARSE bucket':<34} {parse_total:>9} {100 * parse_total / total:>7.2f}%")
    print(f"{'ALLOC bucket (043 frames)':<34} {alloc_total:>9} {100 * alloc_total / total:>7.2f}%")
    print(
        f"{'ALLOC bucket (Rust shim)':<34} {alloc_total + extra_total:>9} "
        f"{100 * (alloc_total + extra_total) / total:>7.2f}%"
    )
    print(f"{'ALLOC bucket (incl. allocator)':<34} {family_total:>9} {100 * family_total / total:>7.2f}%")
    print("-" * 53)
    print(f"{'sip-message subtree (INCLUSIVE)':<34} {inclusive:>9} {100 * inclusive / total:>7.2f}%")
    print()
    print("top 20 self-time frames")
    print("-" * 53)
    for name, n in sorted(by_leaf.items(), key=lambda kv: -kv[1])[:20]:
        print(f"{name[:33]:<34} {n:>9} {100 * n / total:>7.2f}%")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
