{{/* The pod selector; app.kubernetes.io/name is what each Daemon's NetworkPolicy admits (desired::common::MANAGER). */}}
{{- define "balerix-operator.selector" -}}
app.kubernetes.io/name: balerix-operator
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "balerix-operator.labels" -}}
{{ include "balerix-operator.selector" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version }}
{{- end }}

{{/* What the controllers do (Spec O §24.1); the same under a ClusterRole or a Role. */}}
{{- define "balerix-operator.rules" -}}
- apiGroups: [balerix.ai]
  resources: [daemons, fleets, crews, agents, plugins]
  verbs: [get, list, watch, create, update, patch, delete]
- apiGroups: [balerix.ai]
  resources:
    - daemons/status
    - fleets/status
    - crews/status
    - agents/status
    - plugins/status
    - daemons/finalizers
    - fleets/finalizers
    - crews/finalizers
    - agents/finalizers
    - plugins/finalizers
  verbs: [get, update, patch]
- apiGroups: [""]
  resources: [secrets, services, configmaps, persistentvolumeclaims, pods]
  verbs: [get, list, watch, create, update, patch, delete]
- apiGroups: [apps]
  resources: [statefulsets, deployments]
  verbs: [get, list, watch, create, update, patch, delete]
- apiGroups: [batch]
  resources: [jobs]
  verbs: [get, list, watch, create, update, patch, delete]
- apiGroups: [networking.k8s.io]
  resources: [networkpolicies]
  verbs: [get, list, watch, create, update, patch, delete]
- apiGroups: [events.k8s.io]
  resources: [events]
  verbs: [create, patch]
{{- end }}
