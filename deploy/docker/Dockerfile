# syntax=docker/dockerfile:1
# (the `syntax` line is REQUIRED for `RUN --mount=type=cache` below — it pins the
# BuildKit frontend that understands cache mounts. No daemon change needed;
# modern Docker runs BuildKit by default.)
#
# Multi-stage build for the containerized Rust SIP SUT.
# Produces ONE image carrying all runner binaries; the k8s manifests pick which
# one to run via the container `command`:
#   - sip-proxy-runner     (front proxy + load balancer)
#   - b2bua-runner         (B2BUA worker)
#   - call-limiter-runner  (shared sliding-window call limiter)
#   - cdr-consumer-runner  (drains the RabbitMQ CDR queue -> Prometheus metrics)
#   - loadgen              (multiplexed SIP load generator — the endurance
#                           functional-perf stream that runs alongside SIPp)
#
# Build context is the repo root:
#   docker build -f deploy/docker/Dockerfile -t siprustserver:dev .
#
# Only the runner packages are compiled (`-p ...`), so the workspace's
# dev-only parity-oracle path dep is never pulled.
#
# BuildKit cache mounts persist the cargo registry/git caches and the `target/`
# dir across builds, so an edit triggers an INCREMENTAL recompile instead of a
# cold rebuild of the whole dependency graph. The caches are host-local BuildKit
# state (inspect: `docker buildx du`; clear: `docker buildx prune`) — they do NOT
# ship inside the image, so the produced image (and `kind load`) is unchanged.

FROM rust:1-slim-bookworm AS builder
WORKDIR /src
# tikv-jemalloc-sys (the jemalloc global allocator linked into b2bua-runner)
# builds the bundled C jemalloc with `./configure && make`; the slim Rust image
# ships cc but not make, so add build tooling. Discarded with the builder stage
# (multi-stage) — no effect on the runtime image size.
# Drop docker-clean (it wipes downloaded .debs post-install) and cache the apt
# dirs so the package index + .debs survive across builds — hence no
# `rm -rf /var/lib/apt/lists/*` here (the cache mount holds it instead).
RUN rm -f /etc/apt/apt.conf.d/docker-clean
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    apt-get update && apt-get install -y --no-install-recommends make
# Copy the whole workspace (cheap on loopback; .dockerignore drops target/).
COPY . .
# Cache the cargo registry/git + the build `target/`. GOTCHA: a cache-mounted
# `target/` is NOT part of the image filesystem, so a later
# `COPY --from=builder /src/target/...` would find nothing — copy the binaries
# OUT of the mount into a plain dir (/out) in the SAME RUN.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release \
        -p sip-proxy-runner -p b2bua-runner \
        -p call-limiter-runner -p cdr-consumer-runner \
        -p loadgen \
 && mkdir -p /out \
 && cp target/release/sip-proxy-runner \
       target/release/b2bua-runner \
       target/release/call-limiter-runner \
       target/release/cdr-consumer-runner \
       target/release/loadgen /out/

FROM debian:bookworm-slim AS runtime
# ca-certificates for TLS; iproute2 (`tc`) so the chaos suite can inject netem
# packet loss on the call-limiter pod (the limiter_netcut event execs `tc` in the
# pod). Harmless on the proxy/worker; the limiter container also gets NET_ADMIN.
RUN rm -f /etc/apt/apt.conf.d/docker-clean
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates iproute2
COPY --from=builder /out/sip-proxy-runner    /usr/local/bin/sip-proxy-runner
COPY --from=builder /out/b2bua-runner        /usr/local/bin/b2bua-runner
COPY --from=builder /out/call-limiter-runner /usr/local/bin/call-limiter-runner
COPY --from=builder /out/cdr-consumer-runner  /usr/local/bin/cdr-consumer-runner
COPY --from=builder /out/loadgen              /usr/local/bin/loadgen
# Default to the worker; manifests override `command` for the proxy.
ENTRYPOINT []
CMD ["/usr/local/bin/b2bua-runner"]
