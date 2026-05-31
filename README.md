# Launch test

cargo test -p scenario-harness

cargo test -p scenario-harness --test alice_calls_bob -- --nocapture

cargo test --workspace