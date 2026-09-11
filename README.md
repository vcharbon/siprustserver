# Launch test

cargo test -p scenario-harness

cargo test -p scenario-harness --test alice_calls_bob -- --nocapture

cargo test --workspace
# Capture tooling

`sipflow` extracts correlated calls from a pcap and, with `--rfc`, reviews them
against the RFC rules on a stated SUT side: [docs/sipflow.md](docs/sipflow.md).
