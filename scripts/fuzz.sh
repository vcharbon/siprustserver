#!/usr/bin/env bash
# Coverage-guided fuzzing over the fuzz/ workspace (ADR-0051 §8, discovery only;
# docs/testing/fuzzing.md says when to run it):
#
#   fuzz.sh list                                   the targets, in blast-radius order
#   fuzz.sh run <target> [--minutes M] [--jobs J]  fuzz M minutes (default 10) on J
#                                                  processes (default 1); crashes,
#                                                  timeouts and OOMs are kept under
#                                                  fuzz/artifacts/<target>/ and the
#                                                  run goes on
#   fuzz.sh min <target>                           minimise the target's corpus
#   fuzz.sh cov <target>                           line coverage the corpus reaches,
#                                                  per file, under fuzz/coverage/<target>/
#   fuzz.sh seeds                                  rewrite the lanes' synthetic seeds
#                                                  under fuzz/seeds-out
#   fuzz.sh seeds --messages <path> <out>          raw SIP messages (every file of a
#                                                  directory, or a file of NUL-separated
#                                                  messages) written as seeds under
#                                                  <out>, for FUZZ_SEEDS
#
# Where things live, each overridable from the environment:
#   FUZZ_CORPUS  the corpus each run grows, <dir>/<target> (default fuzz/corpus)
#   FUZZ_SEEDS   more seed corpora a run reads, colon-separated, <dir>/<target>
#                each, beside fuzz/seeds-out (the lanes' seeds, which `run`
#                rewrites first)
#   FUZZ_TIMEOUT seconds one input may take before it is a timeout (default 10)
#
# Every limit is set on the command line: -timeout, -rss_limit_mb=2048, and the
# targets' own allocation cap (fuzz/lanes/src/alloc_cap.rs) in place of
# -malloc_limit_mb, which needs a sanitizer's malloc hooks. The build is
# sanitizer-free (no unsafe code to check), release with debug assertions and
# overflow checks on.
set -euo pipefail

root="$(cd "${BASH_SOURCE[0]%/*}/.." && pwd -P)"
fuzz="$root/fuzz"
corpus="${FUZZ_CORPUS:-$fuzz/corpus}"
seeds="$fuzz/seeds-out${FUZZ_SEEDS:+:$FUZZ_SEEDS}"
timeout="${FUZZ_TIMEOUT:-10}"

# The workspace's own rustflags: cargo-fuzz sets RUSTFLAGS, which replaces the
# config's list, and appends what the environment held.
export RUSTFLAGS="--cfg tokio_unstable -Clink-arg=-fuse-ld=mold"

usage() {
  sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed '$d' | sed 's/^# \{0,1\}//'
  exit 2
}

die() {
  echo "fuzz: $*" >&2
  exit 2
}

# cargo in the fuzz workspace: its toolchain file and target dir are read
# from the working directory.
in_fuzz() {
  (cd "$fuzz" && "$@")
}

# In the order fuzz/Cargo.toml declares them: by blast radius.
targets() {
  sed -n '/^\[\[bin\]\]/{n;s/^name = "\(.*\)"$/\1/p}' "$fuzz/Cargo.toml"
}

known() {
  targets | grep -qx "$1" || die "no target '$1' (fuzz.sh list)"
}

write_seeds() {
  in_fuzz cargo run --quiet --release -p fuzz-seeds -- "$@"
}

cmd="${1:-}"
[ -n "$cmd" ] || usage
shift
case "$cmd" in
  list)
    targets
    ;;
  seeds)
    case "$#" in
      0) write_seeds "$fuzz/seeds-out" ;;
      3)
        [ "$1" = --messages ] || usage
        mkdir -p "$3"
        write_seeds "$(cd "$3" && pwd -P)" --messages "$(realpath "$2")"
        ;;
      *) usage ;;
    esac
    ;;
  run)
    target="${1:-}"
    [ -n "$target" ] || usage
    shift
    minutes=10
    jobs=1
    while [ "$#" -gt 0 ]; do
      case "$1" in
        --minutes) minutes="${2:?--minutes M}"; shift 2 ;;
        --jobs) jobs="${2:?--jobs J}"; shift 2 ;;
        *) usage ;;
      esac
    done
    known "$target"
    write_seeds "$fuzz/seeds-out" >/dev/null
    dirs=("$corpus/$target")
    IFS=: read -ra seed_roots <<<"$seeds"
    for s in "${seed_roots[@]}"; do
      [ -d "$s/$target" ] && dirs+=("$s/$target")
    done
    mkdir -p "$corpus/$target" "$fuzz/artifacts/$target"
    # Fork mode: J children share the corpus, and a crash, timeout or OOM is
    # kept as an artifact while the campaign goes on. 64 KiB: a datagram's
    # bound, long enough for parse work that grows faster than its input to
    # cross the timeout.
    in_fuzz cargo fuzz run -s none --fuzz-dir . "$target" "${dirs[@]}" -- \
      -fork="$jobs" -ignore_crashes=1 -ignore_timeouts=1 -ignore_ooms=1 \
      -max_total_time="$((minutes * 60))" -timeout="$timeout" -rss_limit_mb=2048 \
      -max_len=65536 -print_final_stats=1
    echo "fuzz: $target: $(find "$fuzz/artifacts/$target" -type f | wc -l) artifacts under fuzz/artifacts/$target/"
    ;;
  min)
    target="${1:-}"
    [ -n "$target" ] || usage
    known "$target"
    in_fuzz cargo fuzz cmin -s none --fuzz-dir . "$target" "$corpus/$target"
    ;;
  cov)
    target="${1:-}"
    [ -n "$target" ] || usage
    known "$target"
    out="$fuzz/coverage/$target"
    in_fuzz cargo fuzz coverage --fuzz-dir . "$target" "$corpus/$target"
    sysroot="$(in_fuzz rustc --print sysroot)"
    llvm_cov="$(find "$sysroot/lib/rustlib" -name llvm-cov -type f | head -n 1)"
    [ -n "$llvm_cov" ] || die "llvm-cov not found: rustup component add llvm-tools-preview"
    bin="$fuzz/target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/$target"
    "$llvm_cov" report "$bin" -instr-profile="$out/coverage.profdata" \
      -ignore-filename-regex='/(\.cargo|rustc|fuzz)/' >"$out/report.txt"
    tail -n 1 "$out/report.txt"
    echo "fuzz: per-file coverage in fuzz/coverage/$target/report.txt"
    ;;
  *)
    usage
    ;;
esac
