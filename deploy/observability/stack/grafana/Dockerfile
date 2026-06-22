# Custom Grafana image: bakes the VictoriaLogs datasource plugin INTO the image at
# BUILD time, so the running container never needs to reach grafana.com.
#
# WHY: the stock image installed the plugin at STARTUP via GF_INSTALL_PLUGINS. On a
# PROXIFIED / air-gapped host the container could not resolve or reach grafana.com,
# so startup failed and Grafana crash-looped — no listener on :3333 (issue1). Moving
# the install to build time removes that runtime dependency entirely.
#
# PLUGIN PATH — CRITICAL: docker-compose bind-mounts host ./data/grafana over the
# container's /var/lib/grafana (for the Grafana DB + dashboards). The stock plugins
# dir is /var/lib/grafana/plugins, so a plugin baked THERE would be SHADOWED by that
# bind mount at runtime (the empty host dir masks it) and silently vanish — the exact
# breakage we are fixing. So we install into /opt/grafana-plugins (OUTSIDE the mount)
# and point Grafana at it via GF_PATHS_PLUGINS, which survives the bind mount.
#
# PROXY: the plugin fetch (grafana.com) is NON-LOCAL, so it must go THROUGH the host
# proxy when one is set. We honour the proxy ONLY here (build time). The build args
# are forwarded by docker-compose from the install.sh process env; on a non-proxied
# host they arrive empty and the install reaches grafana.com directly. Runtime proxy
# env is BLANKED below (and in docker-compose.yml) so datasource traffic to the
# internal victoriametrics/victorialogs/victoria-traces services never hits the proxy.
FROM grafana/grafana:11.4.0

# Proxy build args (upper- AND lower-case) — `grafana cli plugins install` honours
# HTTP(S)_PROXY/NO_PROXY from the environment, so we promote the build args into env
# for the install layer. Empty defaults => non-proxied build works unchanged.
ARG HTTP_PROXY=
ARG HTTPS_PROXY=
ARG NO_PROXY=
ARG http_proxy=
ARG https_proxy=
ARG no_proxy=

ENV HTTP_PROXY=${HTTP_PROXY} \
    HTTPS_PROXY=${HTTPS_PROXY} \
    NO_PROXY=${NO_PROXY} \
    http_proxy=${http_proxy} \
    https_proxy=${https_proxy} \
    no_proxy=${no_proxy}

# Persisted-plugins dir OUTSIDE the /var/lib/grafana bind mount (see header). The
# stock image runs the build as uid 472 (grafana), which cannot mkdir under root-owned
# /opt — so create + install as root, then chown to the runtime uid 472 / gid 0 and
# drop back. Install the logs datasource at build time (latest compatible with 11.4.0;
# pin a version here if one is ever required). --pluginsDir is explicit so the install
# never falls back to the default (mounted) /var/lib/grafana/plugins.
USER root
RUN mkdir -p /opt/grafana-plugins \
    && grafana cli --pluginsDir /opt/grafana-plugins plugins install victoriametrics-logs-datasource \
    && chown -R 472:0 /opt/grafana-plugins \
    && chmod -R g+rX /opt/grafana-plugins

# Point Grafana at the out-of-mount plugins dir at RUNTIME (image ENV survives the
# bind mount; compose does not override it).
ENV GF_PATHS_PLUGINS=/opt/grafana-plugins

# Do NOT leave the proxy in the final image env — the running container only talks to
# internal datasources and must bypass the proxy. Blank it back out (docker-compose
# also blanks the runtime env as the authoritative belt-and-braces).
ENV HTTP_PROXY= \
    HTTPS_PROXY= \
    NO_PROXY= \
    http_proxy= \
    https_proxy= \
    no_proxy=

# Drop back to the unprivileged runtime user the base image uses.
USER grafana
