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
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates libpcap0.8 libncursesw6 libssl3 \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/build/sipp /usr/local/bin/sipp
ENTRYPOINT ["sipp"]
