# syntax=docker/dockerfile:1
#
# The siprig base image, `siprig:<tag>` (README.md beside this file): the
# rig's binaries, node with this repository's TypeScript packages and the
# viewer app's build, a postgres for the store, and the s6 services that run
# a workspace (`s6-rc.d/`). No Python. Built by `kit base` from the
# repository root with `Dockerfile.dockerignore` and one named context,
# `stamp`: this checkout's provenance (`siprig.env`), read off git on the
# host since the context carries no `.git`.
#
# Layout: /opt/siprig mirrors this checkout for everything that runs (`ts/`
# as source under tsx), the binaries in /opt/siprig/bin (SIPRIG_BIN_DIR),
# first on PATH; the base's commands in /opt/siprig/image/bin after it, so a
# pack's binary of the same name replaces a base command.

ARG NODE_IMAGE=node:26.8.1-trixie-slim
ARG RUST_IMAGE=rust:1.98.1-slim-trixie

# ── Rust release binaries ──────────────────────────────────────────────────
# One cargo per binary, at most 4 rustc jobs; the caller caps the build's
# memory. The target dir and the registry are cache mounts, so a rebuild
# compiles only what moved. The plain replay binary ships as `replay`.
FROM ${RUST_IMAGE} AS rust
ENV CARGO_BUILD_JOBS=4 RUSTUP_TOOLCHAIN=1.98.1
RUN rm -f /etc/apt/apt.conf.d/docker-clean
RUN --mount=type=cache,id=siprig-apt-rust,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,id=siprig-apt-lists-rust,target=/var/lib/apt,sharing=locked \
    apt-get update && apt-get install -y --no-install-recommends make mold
WORKDIR /src
COPY . /src
RUN --mount=type=cache,id=siprig-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=siprig-cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=siprig-target,target=/src/target \
    cargo build --release -p sip-pcap --bin sipflow \
 && cargo build --release -p pivot-schema --bin pivot-schema \
 && cargo build --release -p pivot-replay --bin pivot-replay \
 && mkdir -p /out \
 && cp target/release/sipflow target/release/pivot-schema /out/ \
 && cp target/release/pivot-replay /out/replay
# The line tables stay (file:line in panic backtraces), zlib-compressed.
RUN for bin in /out/*; do objcopy --compress-debug-sections=zlib-gabi "$bin"; done

# ── Downloaded parts: s6-overlay and the docker CLI ────────────────────────
FROM debian:trixie-slim AS fetch
ARG S6_OVERLAY_VERSION=3.2.1.0
ARG DOCKER_CLI_VERSION=29.7.2
ADD https://github.com/just-containers/s6-overlay/releases/download/v${S6_OVERLAY_VERSION}/s6-overlay-noarch.tar.xz /tmp/
ADD https://github.com/just-containers/s6-overlay/releases/download/v${S6_OVERLAY_VERSION}/s6-overlay-x86_64.tar.xz /tmp/
ADD https://download.docker.com/linux/static/stable/x86_64/docker-${DOCKER_CLI_VERSION}.tgz /tmp/docker.tgz
RUN apt-get update && apt-get install -y --no-install-recommends xz-utils \
 && mkdir -p /root-s6 /root-docker/usr/local/bin \
 && tar -C /root-s6 -Jxpf /tmp/s6-overlay-noarch.tar.xz \
 && tar -C /root-s6 -Jxpf /tmp/s6-overlay-x86_64.tar.xz \
 && tar -C /root-docker/usr/local/bin --strip-components=1 -xzf /tmp/docker.tgz docker/docker

# ── TypeScript: the pnpm workspace and the viewer app's build ──────────────
# Installed with dev dependencies: tsx, which runs here, is one.
FROM ${NODE_IMAGE} AS ts
RUN npm install -g pnpm@12.15.0
ENV CI=1
WORKDIR /opt/siprig
COPY ts /opt/siprig/ts
RUN --mount=type=cache,id=siprig-pnpm,target=/pnpm-store \
    pnpm -C ts install --frozen-lockfile --store-dir /pnpm-store --package-import-method copy
# The app's build stamp: the context ships no repository, so the kit states the commit.
ARG SIPRIG_BUILD_COMMIT
RUN pnpm -C ts/viewer-app build

# ── Runtime ────────────────────────────────────────────────────────────────
FROM ${NODE_IMAGE}
ARG PG_MAJOR=17
LABEL org.siprig.pg-major=${PG_MAJOR}
RUN rm -f /etc/apt/apt.conf.d/docker-clean
# postgresql-common makes no cluster: the workspace's pgdata/ is the only one.
RUN --mount=type=cache,id=siprig-apt,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,id=siprig-apt-lists,target=/var/lib/apt,sharing=locked \
    mkdir -p /etc/postgresql-common/createcluster.d \
 && echo "create_main_cluster = false" > /etc/postgresql-common/createcluster.d/no-main.conf \
 && apt-get update \
 && apt-get install -y --no-install-recommends \
      postgresql-${PG_MAJOR} postgresql-client-${PG_MAJOR} \
      bash ca-certificates curl git iproute2 jq libnss-wrapper procps zstd
RUN npm install -g pnpm@12.15.0
COPY --from=fetch /root-s6/ /
COPY --from=fetch /root-docker/ /

COPY --from=ts /opt/siprig/ts /opt/siprig/ts
COPY --from=rust /out/ /opt/siprig/bin/
COPY tooling/siprig/image /opt/siprig/image
COPY tooling/siprig/s6-rc.d /etc/s6-overlay/s6-rc.d
COPY --from=stamp . /opt/siprig/image/stamp/

# Every process runs as the uid/gid the wrapper passes (`--user`), with /run a
# tmpfs it owns. PGUSER names the store's superuser, the one initdb makes. No
# systemd user session runs here: the container caps every child.
ENV SIPRIG_BIN_DIR=/opt/siprig/bin \
    SIPFLOW=/opt/siprig/bin/sipflow \
    PIVOT_SCHEMA_BIN=/opt/siprig/bin/pivot-schema \
    REPLAY_BIN=/opt/siprig/bin/replay \
    SIPRIG_USER_SESSION=none \
    PGUSER=siprig \
    HOME=/tmp \
    PATH=/opt/siprig/bin:/opt/siprig/image/bin:/usr/lib/postgresql/${PG_MAJOR}/bin:/command:$PATH \
    S6_KEEP_ENV=1 \
    S6_BEHAVIOUR_IF_STAGE2_FAILS=2 \
    S6_CMD_WAIT_FOR_SERVICES_MAXTIME=0 \
    S6_SERVICES_GRACETIME=30000 \
    S6_KILL_GRACETIME=5000 \
    S6_VERBOSITY=1
ENTRYPOINT ["/init"]
