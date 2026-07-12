# Build SIPp from source at v3.7.7. The Debian-packaged `sip-tester` (3.6.x)
# ships a thread-race in `getmicroseconds()` that lets `clock_tick` slip
# behind `wheel_base`, tripping the timewheel assert under sustained
# endurance load (~100k+ established calls). Fixed upstream by
# https://github.com/SIPp/sipp/pull/619 (commit 2548582), released in
# 3.7.0. Not backported to 3.6.x, so we build from source.
FROM debian:bookworm-slim AS build
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      build-essential cmake git ca-certificates \
      libpcap-dev libncurses-dev libssl-dev \
 && rm -rf /var/lib/apt/lists/*
RUN git clone --depth 1 --branch v3.7.7 https://github.com/SIPp/sipp.git /src
WORKDIR /src/build
RUN cmake .. -DUSE_PCAP=1 -DUSE_SSL=1 \
 && make -j"$(nproc)"

FROM debian:bookworm-slim
# python3 (stdlib only, no pip) powers the SIPp stat -> Prometheus sidecar
# exporter (deploy/k8s/sipp/exporter/sipp_stat_exporter.py), run from this same
# image so the reporting path needs no second image.
# Full `python3` (not python3-minimal — that strips the stdlib, missing
# http.server which the exporter needs).
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates libpcap0.8 libncursesw6 libssl3 python3 \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/build/sipp /usr/local/bin/sipp
# No ENTRYPOINT: every caller names the binary explicitly — `docker run sipp:dev
# sipp <args>` for the UAC/UAS generators (run.sh sipp_uas_up, sipext-gen.sh
# sipext_sipp_uac_up) and `docker run sipp:dev python3 /exporter/...` for the
# stat->Prometheus exporter twin. A hard `ENTRYPOINT ["sipp"]` would prepend a
# second `sipp` (→ "remote_host given multiple times") to the generators AND make
# the python3 exporter unrunnable (sipp would parse "python3" as a remote host).
