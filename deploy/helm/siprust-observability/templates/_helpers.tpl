{{/* Metadata labels of every object. Takes (dict "root" $ "app" <name>). */}}
{{- define "obs.labels" -}}
app: {{ .app }}
app.kubernetes.io/name: {{ .app }}
app.kubernetes.io/part-of: siprust-observability
app.kubernetes.io/instance: {{ .root.Release.Name }}
app.kubernetes.io/managed-by: {{ .root.Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .root.Chart.Name .root.Chart.Version }}
{{- end }}

{{/* `image` and `imagePullPolicy` of a container. Takes the image values. */}}
{{- define "obs.image" -}}
image: {{ printf "%s:%s" .repository (toString .tag) | quote }}
imagePullPolicy: {{ .pullPolicy }}
{{- end }}

{{/* Pod-level image pull secrets. */}}
{{- define "obs.pullSecrets" -}}
{{- with .Values.imagePullSecrets }}
imagePullSecrets:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- end }}

{{/* The host stack address, required. */}}
{{- define "obs.host" -}}
{{- required "hostStack.address is required: where VictoriaMetrics and VictoriaLogs listen, as the pods reach them" .Values.hostStack.address }}
{{- end }}
