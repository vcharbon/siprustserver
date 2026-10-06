#!/usr/bin/env bash
# One integration test binary per crate (ADR-0030):
#
#   check-test-layout.sh [<workspace root> [<allowlist>]]
#
# Every test target of every workspace member is the crate's `tests/it/main.rs`,
# or a file the allowlist names with its X2 reason, one per line:
# `<path from the root> <reason>`, `#` for comments. Exits 1 on:
#   - any other test target (a `tests/*.rs` file, a `tests/<dir>/main.rs`, a `[[test]]`);
#   - two test targets built from one file;
#   - a `tests/it/<name>.rs` or `tests/it/<name>/mod.rs` that no `mod <name>;` line
#     of `tests/it/main.rs` declares, or a `tests/it/<dir>/` holding Rust sources
#     that is neither: it would never compile nor run;
#   - a manifest with `autotests = false`, under which a stray `tests/*.rs` is
#     silently no target at all;
#   - an allowlist entry that names no test target.
# The root defaults to this repository, the allowlist to `<root>/test-binaries.allow`.
# Needs `cargo` and `jq`.
set -euo pipefail

command -v jq >/dev/null || { echo "check-test-layout needs jq (see \`just doctor\`)" >&2; exit 2; }

here="$(cd "${BASH_SOURCE[0]%/*}/.." && pwd -P)"
root="$(cd "${1:-$here}" && pwd -P)"
allow="${2:-$root/test-binaries.allow}"
[ -f "$allow" ] || { echo "check-test-layout: no allowlist at $allow" >&2; exit 2; }

# A path as the reader typed it: relative to the working directory when under it.
shown() { case "$1" in "$PWD"/*) echo "${1#"$PWD"/}" ;; *) echo "$1" ;; esac; }
allow_shown="$(shown "$(cd "$(dirname "$allow")" && pwd -P)/$(basename "$allow")")"

bad=0
fail() { echo "check-test-layout: $*" >&2; bad=1; }

metadata="$(cd "$root" && cargo metadata --no-deps --format-version 1 --offline)"
targets="$(jq -r '.packages[].targets[] | select(.kind | index("test")) | .src_path' <<<"$metadata" \
  | sed "s#^$root/##" | sort)"
manifests="$(jq -r '.packages[].manifest_path' <<<"$metadata")"
allowed="$(sed -e 's/#.*//' -e '/^[[:space:]]*$/d' "$allow" | awk '{ print $1 }' | sort -u)"

while IFS= read -r t; do
  [ -n "$t" ] || continue
  fail "$t is the source of more than one test target"
done < <(uniq -d <<<"$targets")

while IFS= read -r t; do
  [ -n "$t" ] || continue
  case "$t" in */tests/it/main.rs) continue ;; esac
  grep -qxF -- "$t" <<<"$allowed" && continue
  fail "$t is a test binary of its own: move it to tests/it/ as a module," \
    "or name it in $allow_shown with the X2 reason it needs its own process"
done <<<"$targets"

while IFS= read -r a; do
  [ -n "$a" ] || continue
  grep -qxF -- "$a" <<<"$targets" && continue
  fail "$allow_shown names $a, which is no test target (stale entry)"
done <<<"$allowed"

while IFS= read -r m; do
  [ -n "$m" ] || continue
  grep -Eq '^[[:space:]]*autotests[[:space:]]*=[[:space:]]*false' "$m" \
    && fail "$(shown "$m") sets autotests = false: a tests/*.rs file there is silently no target"
done <<<"$manifests"

# Every module source of a tests/it/ target is declared by its main.rs.
while IFS= read -r t; do
  case "$t" in */tests/it/main.rs) ;; *) continue ;; esac
  dir="$root/${t%/main.rs}"
  declared="$(sed -nE 's/^[[:space:]]*(pub(\([^)]*\))?[[:space:]]+)?mod[[:space:]]+([A-Za-z0-9_]+)[[:space:]]*;.*/\3/p' \
    "$dir/main.rs")"
  for f in "$dir"/*.rs; do
    [ -e "$f" ] || continue
    name="${f##*/}"; name="${name%.rs}"
    [ "$name" = main ] && continue
    grep -qxF -- "$name" <<<"$declared" \
      || fail "$(shown "$f") has no \`mod $name;\` in $(shown "$dir/main.rs"): it never compiles nor runs"
  done
  for d in "$dir"/*/; do
    [ -d "$d" ] || continue
    d="${d%/}"; name="${d##*/}"
    if [ -f "$d/mod.rs" ]; then
      grep -qxF -- "$name" <<<"$declared" \
        || fail "$(shown "$d")/mod.rs has no \`mod $name;\` in $(shown "$dir/main.rs"): it never compiles nor runs"
    elif [ ! -f "$dir/$name.rs" ] && [ -n "$(find "$d" -name '*.rs' -print -quit)" ]; then
      fail "$(shown "$d")/ holds Rust sources but no mod.rs nor $name.rs: they never compile nor run"
    fi
  done
done <<<"$targets"

if [ "$bad" -ne 0 ]; then
  echo "check-test-layout: see $(shown "$here/docs/adr/0030-one-integration-test-binary-per-crate.md")" >&2
  exit 1
fi
