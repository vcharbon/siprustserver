#!/usr/bin/env bash
# Rustdoc of every lib and bin of the workspace in the current directory:
#
#   doc-all.sh [cargo doc args…]     e.g. --keep-going --message-format=json
#
# Each run is `cargo doc --no-deps --workspace … --examples`: `--examples` (the
# workspace has none) makes cargo resolve dev-dependencies, so the crates it
# checks to document carry the test build's features. A bin named like its
# package's lib writes the lib's `target/doc/<name>/`, so it gets a run of its
# own after the libs: there the lib's doc is fresh from the first run (cargo
# still names the collision), so two rustdocs never write one directory at
# once, and the bin's page overwrites the lib's index.html. The first run
# documents every lib and every other bin. Runs one after the other and exits
# 1 when one failed.
set -uo pipefail

meta="$(cargo metadata --no-deps --format-version 1)" || exit 1
lib_named="$(jq -r '.packages[]
  | ([.targets[] | select(.kind | index("lib")) | .name]) as $libs
  | .targets[] | select(.kind | index("bin"))
  | select((.name | gsub("-"; "_")) as $n | $libs | index($n)) | .name' <<<"$meta")" || exit 1
others=()
while IFS= read -r bin; do
  [ -n "$bin" ] && others+=(--bin "$bin")
done < <(jq -r --arg skip "$lib_named" '($skip | split("\n")) as $s | .packages[].targets[]
  | select(.kind | index("bin")) | select(.name | IN($s[]) | not) | .name' <<<"$meta")
rc=0
cargo doc --no-deps --workspace --lib "${others[@]}" --examples "$@" || rc=1
while IFS= read -r bin; do
  [ -n "$bin" ] || continue
  cargo doc --no-deps --workspace --bin "$bin" --examples "$@" || rc=1
done <<<"$lib_named"
exit "$rc"
