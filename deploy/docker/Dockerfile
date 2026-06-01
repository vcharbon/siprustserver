# Multi-stage build for the containerized Rust SIP SUT.
# Produces ONE image carrying all runner binaries; the k8s manifests pick which
# one to run via the container `command`:
#   - sip-proxy-runner     (front proxy + load balancer)
#   - b2bua-runner         (B2BUA worker)
#   - call-limiter-runner  (shared sliding-window call limiter)
#
# Build context is the repo root:
#   docker build -f deploy/docker/Dockerfile -t siprustserver:dev .
#
# Only the runner packages are compiled (`-p ...`), so the workspace's
# dev-only parity-oracle path dep is never pulled.

FROM rust:1-slim-bookworm AS builder
WORKDIR /src
# Copy the whole workspace (cheap on loopback; .dockerignore drops target/).
COPY . .
RUN cargo build --release -p sip-proxy-runner -p b2bua-runner -p call-limiter-runner

FROM debian:bookworm-slim AS runtime
# ca-certificates only; the runners need no extra runtime libs beyond glibc.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/sip-proxy-runner    /usr/local/bin/sip-proxy-runner
COPY --from=builder /src/target/release/b2bua-runner        /usr/local/bin/b2bua-runner
COPY --from=builder /src/target/release/call-limiter-runner /usr/local/bin/call-limiter-runner
# Default to the worker; manifests override `command` for the proxy.
ENTRYPOINT []
CMD ["/usr/local/bin/b2bua-runner"]
