# syntax=docker/dockerfile:1
#
# A pack's image, `<name>-rig:<tag>`, FROM the base image `siprig:<tag>`.
# Built by `kit pack` (README.md) from the staging directory it fills: `pack/`
# (the pack's tree as its checkout holds it, ignored files left out),
# `binaries/` (the directory `[image] binaries` names, or empty), `stamp/`
# (the pack's provenance, `<component>.env`) and `wrapper` (the wrapper the
# kit wrote for this image). The kit hands the manifest's facts in as
# build arguments, each checked before the build (`ts/siprig/src/pack-image.ts`).
ARG BASE=siprig
FROM ${BASE}

ARG PACKAGES=""
RUN rm -f /etc/apt/apt.conf.d/docker-clean
RUN --mount=type=cache,id=siprig-pack-apt,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,id=siprig-pack-apt-lists,target=/var/lib/apt,sharing=locked \
    if [ -n "$PACKAGES" ]; then apt-get update && apt-get install -y --no-install-recommends $PACKAGES; fi

COPY pack/ /opt/siprig/pack/
COPY binaries/ /opt/siprig/bin/
COPY stamp/ /opt/siprig/image/stamp/
COPY wrapper /opt/siprig/image/wrapper

# The pack's setup, run once in its tree with the packages in place;
# SIPRIG_BUILD_CACHE is a directory kept from one build to the next.
ARG SETUP=""
RUN --mount=type=cache,id=siprig-pack-setup,target=/var/cache/siprig-setup \
    if [ -n "$SETUP" ]; then cd /opt/siprig/pack && SIPRIG_BUILD_CACHE=/var/cache/siprig-setup "./$SETUP"; fi

ARG NAME
ARG WRAPPER
ARG COMPONENT
LABEL org.siprig.pack=${NAME} org.siprig.wrapper=${WRAPPER}
ENV SIPRIG_PACK=/opt/siprig/pack \
    SIPRIG_WRAPPER=${WRAPPER} \
    SIPRIG_COMPONENT=${COMPONENT}
