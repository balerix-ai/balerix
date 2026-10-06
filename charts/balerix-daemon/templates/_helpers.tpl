{{- define "balerix-daemon.name" -}}
{{ .Values.name | default .Release.Name }}
{{- end }}

{{- define "balerix-daemon.labels" -}}
app.kubernetes.io/name: balerix-daemon
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version }}
{{- end }}

{{/* A ClaimSpec; an empty storage class is left out. */}}
{{- define "balerix-daemon.claim" -}}
size: {{ .size | quote }}
{{- with .storageClassName }}
storageClassName: {{ . | quote }}
{{- end }}
{{- end }}

{{/* The plugins this chart makes, in interceptor order. */}}
{{- define "balerix-daemon.ownPlugins" -}}
{{- $names := list -}}
{{- range $p := list "flow" "web" "matrix" "github" -}}
{{- if (index $.Values.plugins $p).enabled -}}
{{- $names = append $names $p -}}
{{- end -}}
{{- end -}}
{{- toJson $names -}}
{{- end }}
