{{- /*
SPDX-FileCopyrightText: 2026 mupf-dev
SPDX-License-Identifier: FSL-1.1-ALv2
*/ -}}

{{- define "ai-gw.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "ai-gw.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{- define "ai-gw.selectorLabels" -}}
app.kubernetes.io/name: {{ include "ai-gw.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "ai-gw.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{ include "ai-gw.selectorLabels" . }}
app.kubernetes.io/version: {{ .Values.image.tag | default .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/part-of: ai-gw
{{- end }}

{{- define "ai-gw.image" -}}
{{- if .Values.image.digest -}}
{{ .Values.image.repository }}@{{ .Values.image.digest }}
{{- else -}}
{{ .Values.image.repository }}:{{ .Values.image.tag | default .Chart.AppVersion }}
{{- end -}}
{{- end }}

{{- define "ai-gw.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "ai-gw.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{/* "true", wenn die Datenbank SQLite ist (keine Secret-URL, config.store.url sqlite:…). */}}
{{- define "ai-gw.sqlite" -}}
{{- $url := "" }}
{{- with .Values.config.store }}{{ $url = .url | default "" }}{{ end }}
{{- if and (not .Values.store.existingSecret) (hasPrefix "sqlite:" $url) }}true{{ end }}
{{- end }}

{{/* Eingabeprüfung – bricht `helm install/template` mit verständlicher Meldung ab. */}}
{{- define "ai-gw.validate" -}}
{{- if not .Values.masterKey.existingSecret }}
{{- fail "masterKey.existingSecret fehlt: Secret mit dem Master-Key anlegen (aigw keygen) und angeben – siehe docs/betrieb/helm.md" }}
{{- end }}
{{- if and (include "ai-gw.sqlite" .) (gt (int .Values.replicaCount) 1) }}
{{- fail "SQLite erlaubt nur replicaCount: 1 – für mehrere Replikate Postgres über store.existingSecret verwenden" }}
{{- end }}
{{- end }}
