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

{{/* The keepalived image of the VRRP sidecar, vip-loopback and masq-exempt. */}}
{{- define "siprust.keepalivedImage" -}}
image: {{ printf "%s:%s" .Values.keepalivedImage.repository (toString .Values.keepalivedImage.tag) | quote }}
imagePullPolicy: {{ .Values.keepalivedImage.pullPolicy }}
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

{{/* The addresses of proxy.keepalived.extraVirtualIps, space-separated. */}}
{{- define "siprust.extraVips" -}}
{{- $out := list }}
{{- range .Values.proxy.keepalived.extraVirtualIps }}
{{- $out = append $out .address }}
{{- end }}
{{- join " " $out }}
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
