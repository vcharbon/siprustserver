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
Plain env entries of a name -> value map, sorted by name; a null value is
left out. Takes the map.
*/}}
{{- define "siprust.envMap" -}}
{{- range $name := keys . | sortAlpha }}
{{- $value := get $ $name }}
{{- if not (kindIs "invalid" $value) }}
- name: {{ $name }}
  value: {{ $value | toString | quote }}
{{- end }}
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
