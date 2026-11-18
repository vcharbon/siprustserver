{{/*
Metadata labels of every object: `app: <name>` (the selector label the tooling
selects pods by) and the chart's own. Takes (dict "root" $ "app" <name>).
*/}}
{{- define "siprust.labels" -}}
app: {{ .app }}
app.kubernetes.io/name: {{ .app }}
app.kubernetes.io/part-of: siprust
app.kubernetes.io/instance: {{ .root.Release.Name }}
app.kubernetes.io/managed-by: {{ .root.Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .root.Chart.Name .root.Chart.Version }}
{{- end }}

{{/*
Pod template labels: the selector label and the component's podLabels. Takes
(dict "app" <name> "extra" <podLabels>).
*/}}
{{- define "siprust.podLabels" -}}
app: {{ .app }}
{{- with .extra }}
{{ toYaml . }}
{{- end }}
{{- end }}

{{/*
`image` and `imagePullPolicy` of a container: the component's image over the
default image, key by key. Takes (dict "root" $ "image" <component image>).
*/}}
{{- define "siprust.image" -}}
{{- $img := merge (deepCopy (.image | default dict)) .root.Values.image -}}
image: {{ printf "%s:%s" $img.repository (toString $img.tag) | quote }}
imagePullPolicy: {{ $img.pullPolicy }}
{{- end }}

{{/* The network tools image of vip-loopback and masq-exempt. */}}
{{- define "siprust.netToolsImage" -}}
image: {{ printf "%s:%s" .Values.netToolsImage.repository (toString .Values.netToolsImage.tag) | quote }}
imagePullPolicy: {{ .Values.netToolsImage.pullPolicy }}
{{- end }}

{{/*
Plain env entries of a name -> value map, sorted by name, each value through
`tpl` so it may name the release; a null value is left out. Takes
(dict "root" $ "env" <map>).
*/}}
{{- define "siprust.envMap" -}}
{{- $root := .root }}
{{- range $name := keys .env | sortAlpha }}
{{- $value := get $.env $name }}
{{- if not (kindIs "invalid" $value) }}
- name: {{ $name }}
  value: {{ tpl ($value | toString) $root | quote }}
{{- end }}
{{- end }}
{{- end }}

{{/*
Pod annotations: the chart's own (the prometheus.io scrape target), the
component's podAnnotations over them key by key. Takes (dict "base" <map>
"extra" <podAnnotations>).
*/}}
{{- define "siprust.podAnnotations" -}}
{{- $all := deepCopy .base }}
{{- range $k, $v := .extra }}
{{- $_ := set $all $k $v }}
{{- end }}
{{- toYaml $all }}
{{- end }}

{{/*
Pod-level settings of the worker and the proxy: priorityClassName and the pod
securityContext with its sysctls. Takes the component values.
*/}}
{{- define "siprust.podSecurity" -}}
{{- with .priorityClassName }}
priorityClassName: {{ . }}
{{- end }}
{{- $sc := deepCopy (.podSecurityContext | default dict) }}
{{- with .sysctls }}
{{- $_ := set $sc "sysctls" . }}
{{- end }}
{{- with $sc }}
securityContext:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- end }}

{{/*
Overlays a component's `env` map onto the derived env map (mutates it). Takes
(dict "into" <derived map> "from" <component env>).
*/}}
{{- define "siprust.overlayEnv" -}}
{{- range $name, $value := .from }}
{{- $_ := set $.into $name $value }}
{{- end }}
{{- end }}

{{/* Pod-level image pull secrets. */}}
{{- define "siprust.pullSecrets" -}}
{{- with .Values.imagePullSecrets }}
imagePullSecrets:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- end }}

{{/*
Pod scheduling of a component: nodeSelector, tolerations, affinity,
topologySpreadConstraints when set. Takes the component values.
*/}}
{{- define "siprust.scheduling" -}}
{{- with .nodeSelector }}
nodeSelector:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- with .tolerations }}
tolerations:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- with .affinity }}
affinity:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- with .topologySpreadConstraints }}
topologySpreadConstraints:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- end }}

{{/* The in-cluster DNS name of a Service of this release. Takes (list $ <name>). */}}
{{- define "siprust.svcHost" -}}
{{- printf "%s.%s.svc.cluster.local" (index . 1) (index . 0).Release.Namespace }}
{{- end }}

{{/* The broker URL producer and consumer share. */}}
{{- define "siprust.amqpUrl" -}}
{{- printf "amqp://%s:%s@%s:5672/%%2f" .Values.cdr.rabbitmq.user .Values.cdr.rabbitmq.password (include "siprust.svcHost" (list . "rabbitmq")) }}
{{- end }}

{{/* SIP_TRACE_HEADER of the tracing values. */}}
{{- define "siprust.traceHeader" -}}
{{- ternary "1" "0" .Values.tracing.traceHeader }}
{{- end }}

{{/*
A network value the proxy and masq-exempt require, named in the error. Takes
(dict "root" $ "key" <network key>).
*/}}
{{- define "siprust.net" -}}
{{- $v := index .root.Values.network .key }}
{{- required (printf "network.%s is required by the proxy and masq-exempt" .key) $v }}
{{- end }}

{{/*
An integer env value of a component's final env, or the runner default when
the env does not state it (unset or null). Takes (list <env map> <name>
<default>); a value that is not an integer fails the render naming it.
*/}}
{{- define "siprust.envInt" -}}
{{- $v := get (index . 0) (index . 1) }}
{{- if or (kindIs "invalid" $v) (eq (toString $v) "") }}
{{- index . 2 }}
{{- else if regexMatch "^[0-9]+$" (toString $v) }}
{{- toString $v }}
{{- else }}
{{- fail (printf "%s=%q is not an integer" (index . 1) (toString $v)) }}
{{- end }}
{{- end }}

{{/*
A byte quantity in bytes: an integer with an optional Ki/Mi/Gi/Ti or k/M/G/T
suffix, the forms the runners and Kubernetes share. Takes (list <name>
<value>); any other form fails the render naming it.
*/}}
{{- define "siprust.bytes" -}}
{{- $name := index . 0 }}
{{- $v := toString (index . 1) | trim }}
{{- $digits := regexFind "^[0-9]+" $v }}
{{- $unit := trimPrefix $digits $v }}
{{- $scale := dict "" 1 "Ki" 1024 "Mi" 1048576 "Gi" 1073741824 "Ti" 1099511627776 "k" 1000 "M" 1000000 "G" 1000000000 "T" 1000000000000 }}
{{- if or (not $digits) (not (hasKey $scale $unit)) }}
{{- fail (printf "%s=%q is not a byte quantity (an integer with an optional Ki, Mi, Gi, Ti, k, M, G or T suffix)" $name $v) }}
{{- end }}
{{- mul (int64 $digits) (get $scale $unit) }}
{{- end }}

{{/*
The worker's affinity: its own `affinity` and, with the proxy enabled, a
required node affinity off every node proxy.nodeSelector names, so a worker
never lands on a node that runs or ran a proxy, whose VIPs stay on its lo
(ADR-0056 §7). A node is off when it carries every label of the selector:
one term per label (terms are ORed), each ANDed into every term the worker's
own required node affinity states. Takes the root.
*/}}
{{- define "siprust.workerAffinity" -}}
{{- $aff := deepCopy (.Values.worker.affinity | default dict) }}
{{- if .Values.proxy.enabled }}
{{- $na := get $aff "nodeAffinity" | default dict }}
{{- $req := get $na "requiredDuringSchedulingIgnoredDuringExecution" | default dict }}
{{- $own := get $req "nodeSelectorTerms" | default (list dict) }}
{{- $terms := list }}
{{- range $term := $own }}
{{- range $k, $v := $.Values.proxy.nodeSelector }}
{{- $t := deepCopy $term }}
{{- $exprs := get $t "matchExpressions" | default list }}
{{- $_ := set $t "matchExpressions" (append $exprs (dict "key" $k "operator" "NotIn" "values" (list (toString $v)))) }}
{{- $terms = append $terms $t }}
{{- end }}
{{- end }}
{{- $_ := set $req "nodeSelectorTerms" $terms }}
{{- $_ := set $na "requiredDuringSchedulingIgnoredDuringExecution" $req }}
{{- $_ := set $aff "nodeAffinity" $na }}
{{- end }}
{{- toYaml $aff }}
{{- end }}

{{/*
The routers the proxy's route guard drops BFD and BGP to, comma-separated:
proxy.routeGuard.peers, else every address of proxy.announce.metallb.peers
when the chart renders MetalLB's objects; a guard with neither fails the
render.
*/}}
{{- define "siprust.routeGuardPeers" -}}
{{- $g := .Values.proxy.routeGuard }}
{{- $m := .Values.proxy.announce.metallb }}
{{- $peers := list }}
{{- range $g.peers | default list }}{{- $peers = append $peers (toString .) }}{{- end }}
{{- if and (not $peers) $m.enabled }}
{{- range $face := list "internal" "external" }}
{{- range (index $m.peers $face | default list) }}{{- $peers = append $peers (toString .address) }}{{- end }}
{{- end }}
{{- end }}
{{- if not $peers }}
{{- fail "proxy.routeGuard.peers is required when the chart does not render MetalLB's peers (proxy.announce.metallb.enabled): the routers whose BFD and BGP the guard drops; or set proxy.routeGuard.enabled=false" }}
{{- end }}
{{- range $peers }}
{{- if not (regexMatch "^[0-9]{1,3}(\\.[0-9]{1,3}){3}$" .) }}
{{- fail (printf "proxy.routeGuard: router %s is not an IPv4 address: the route guard drops BFD and BGP to IPv4 routers only" .) }}
{{- end }}
{{- end }}
{{- $peers | uniq | join "," }}
{{- end }}

{{/*
The node's address, HOST_IP, a scrape bind of the proxy pod names as
$(HOST_IP); declared before every env entry that names it.
*/}}
{{- define "siprust.hostIpEnv" -}}
- name: HOST_IP
  valueFrom: { fieldRef: { fieldPath: status.hostIP } }
{{- end }}

{{/*
The final value of a listener variable: the component env's when it states
one (null: the runner's default), else the chart's. Takes (list <component
env> <variable> <the chart's value> <the runner's default>).
*/}}
{{- define "siprust.statedBind" -}}
{{- $env := index . 0 | default dict }}
{{- if hasKey $env (index . 1) }}
{{- $v := get $env (index . 1) }}
{{- ternary (index . 3) (toString $v) (kindIs "invalid" $v) }}
{{- else }}
{{- index . 2 }}
{{- end }}
{{- end }}

{{/* PROXY_METRICS as the chart derives it: proxy.metrics.bind, else the node's address. */}}
{{- define "siprust.proxyMetricsBind" -}}
{{- .Values.proxy.metrics.bind | default "$(HOST_IP):9090" }}
{{- end }}

{{/* PROXY_METRICS as the proxy reads it: its env over the derived value. */}}
{{- define "siprust.proxyMetricsFinal" -}}
{{- include "siprust.statedBind" (list .Values.proxy.env "PROXY_METRICS" (include "siprust.proxyMetricsBind" .) "0.0.0.0:9090") }}
{{- end }}

{{/* PROXY_DEBUG: proxy.debug.bind, else loopback port 9096. */}}
{{- define "siprust.proxyDebugBind" -}}
{{- .Values.proxy.debug.bind | default "127.0.0.1:9096" }}
{{- end }}

{{/* B2BUA_METRICS as the chart derives it: worker.metrics.bind, else every address of the pod. */}}
{{- define "siprust.workerMetricsBind" -}}
{{- .Values.worker.metrics.bind | default "0.0.0.0:9091" }}
{{- end }}

{{/* B2BUA_ADMIN: worker.admin.bind, else loopback port 9094. */}}
{{- define "siprust.workerAdminBind" -}}
{{- .Values.worker.admin.bind | default "127.0.0.1:9094" }}
{{- end }}

{{/* ROUTE_GUARD_METRICS: proxy.routeGuard.metricsBind, else the node's address. */}}
{{- define "siprust.guardMetricsBind" -}}
{{- $g := .Values.proxy.routeGuard }}
{{- $g.metricsBind | default (printf "$(HOST_IP):%d" (int $g.metricsPort)) }}
{{- end }}

{{/*
The host of an `ip:port` bind, lower case: inside its brackets, else before
its last colon, an IPv6 zone dropped.
*/}}
{{- define "siprust.bindHost" -}}
{{- $host := ternary (regexReplaceAll "^\\[([^]]*)\\].*$" . "${1}") (regexReplaceAll ":[^:]*$" . "") (hasPrefix "[" .) }}
{{- regexReplaceAll "%.*$" $host "" | lower }}
{{- end }}

{{/*
Whether a bind's host is unspecified, a wildcard: "true" or empty. Every
spelling of 0.0.0.0 and :: (zero groups, an embedded 0.0.0.0) and the
IPv4-mapped ::ffff:0.0.0.0, which binds every IPv4 address.
*/}}
{{- define "siprust.bindIsUnspecified" -}}
{{- $host := include "siprust.bindHost" . }}
{{- if or (regexMatch "^[0:.]*$" $host) (regexMatch "^[0:]*:ffff:[0:.]*$" $host) }}true{{ end }}
{{- end }}

{{/*
Whether a bind's host is loopback: "true" or empty. 127.0.0.0/8, every
spelling of ::1, and an IPv4-mapped 127.0.0.0/8 address.
*/}}
{{- define "siprust.bindIsLoopback" -}}
{{- $host := include "siprust.bindHost" . }}
{{- if or (regexMatch "^127\\." $host) (regexMatch "^[0:]*:0*1$" $host) (regexMatch "^[0:]*:ffff:127\\." $host) (eq $host "localhost") }}true{{ end }}
{{- end }}

{{/*
The address the route guard reads /fitz at: the proxy's final PROXY_METRICS,
the node's address in place of a wildcard host.
*/}}
{{- define "siprust.guardFitness" -}}
{{- $bind := include "siprust.proxyMetricsFinal" . }}
{{- if include "siprust.bindIsUnspecified" $bind }}
{{- printf "$(HOST_IP):%s" (regexReplaceAll "^.*:" $bind "") }}
{{- else }}
{{- $bind }}
{{- end }}
{{- end }}

{{/* The route guard's env, for its container and its init step. */}}
{{- define "siprust.routeGuardEnv" -}}
{{- $g := .Values.proxy.routeGuard -}}
{{ include "siprust.hostIpEnv" . }}
- { name: ROUTE_GUARD_FITNESS, value: {{ include "siprust.guardFitness" . | quote }} }
- { name: ROUTE_GUARD_PEERS, value: {{ include "siprust.routeGuardPeers" . | quote }} }
- { name: ROUTE_GUARD_POLL_MS, value: {{ int $g.pollMs | toString | quote }} }
- { name: ROUTE_GUARD_DEAD_MS, value: {{ int $g.deadMs | toString | quote }} }
- { name: ROUTE_GUARD_HOLD_MS, value: {{ int $g.holdMs | toString | quote }} }
- { name: ROUTE_GUARD_METRICS, value: {{ include "siprust.guardMetricsBind" . | quote }} }
{{- end }}

{{/* The route guard's privileges: NET_ADMIN on the node's network, nothing else. */}}
{{- define "siprust.routeGuardSecurity" -}}
runAsUser: 0
runAsNonRoot: false
allowPrivilegeEscalation: false
readOnlyRootFilesystem: true
capabilities: { drop: ["ALL"], add: ["NET_ADMIN"] }
{{- end }}
