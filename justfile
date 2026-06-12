# Test lanes (see CLAUDE.md "Test-runtime policy").
#
# default lane: every test that runs on the fake clock (paused tokio) or
# finishes fast on the real clock. Anything real-clock >60 s wall is
# `#[ignore]`d out of this lane and lives in the slow lane.

# Default lane — what CI and pre-commit run.
test:
    cargo test --workspace

# Slow lane — real-clock >60 s integration tests (`#[ignore]`d in the default
# lane). Run explicitly when touching the code they cover.
test-slow:
    cargo test --workspace --release -- --ignored

# Both lanes.
test-all: test test-slow
