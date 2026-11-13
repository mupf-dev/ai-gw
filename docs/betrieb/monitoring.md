# Monitoring

Details zu Metriken, Spans und Request-Log: [07-beobachtung.md](../konzept/07-beobachtung.md).

## 1. Health-Endpunkte

| Endpunkt | Ebene | Bedeutung | Verwendung |
|---|---|---|---|
| `GET /healthz` | Daten + Steuerung | Prozess lebt, Listener nimmt an | Liveness |
| `GET /readyz` | Daten + Steuerung | `{"status":"ready"}` bzw. `"degraded"` (HTTP 200) mit Snapshot-Version, Alter, Quelle | Readiness, Load Balancer |
| `GET /admin/api/v1/version` | Steuerung | Version und Snapshot-Zustand | Diagnose |
| `aigw healthcheck` | – | fragt `/readyz` der Steuerebene ab, Exit 0/1 | Docker-`HEALTHCHECK` (ohne Shell/curl) |

`degraded` heißt: Der Konfigurations-Snapshot stammt aus dem Platten-Cache (Kaltstart ohne
Datenbank) oder konnte länger als `snapshot.stale_after_secs` nicht abgeglichen werden. Die
Instanz bedient weiter Anfragen; deshalb bewusst HTTP 200. Alarmieren über die Metrik
`aigw_snapshot_stale_seconds` bzw. das Feld `status`.

## 2. Prometheus

`GET /metrics` auf der **Steuerebene** (`:4001`, Text-Format 0.0.4). Abschaltbar mit
`observe.metrics.enabled: false`; die Kardinalität ist begrenzt (`observe.metrics.max_model_groups`,
`max_deployments` – darüber hinaus Sammel-Label).

```yaml
scrape_configs:
  - job_name: aigw
    metrics_path: /metrics
    static_configs:
      - targets: ["aigw-1:4001", "aigw-2:4001"]
```

Wichtige Metriken:

| Metrik | Inhalt |
|---|---|
| `aigw_requests_total`, `aigw_request_duration_seconds`, `aigw_ttft_seconds` | Anfragen, Dauer, Zeit bis zum ersten Token (je Modellgruppe, Status) |
| `aigw_tokens_total`, `aigw_cost_total` | Tokens und Kosten |
| `aigw_inflight_requests`, `aigw_deployment_inflight_requests` | laufende Anfragen |
| `aigw_upstream_attempts_total`, `aigw_upstream_errors_total`, `aigw_upstream_retries_total`, `aigw_fallbacks_total` | Upstream-Versuche, Fehlerklassen, Retries, Fallbacks |
| `aigw_deployment_breaker_state` | Circuit Breaker je Deployment (0 geschlossen, 1 offen, 2 halb offen) |
| `aigw_stream_client_aborts_total` | vom Client abgebrochene Streams |
| `aigw_guardrail_hits_total` | Treffer der Guardrail-Plugins |
| `aigw_snapshot_version`, `aigw_snapshot_reload_total`, `aigw_snapshot_build_seconds`, `aigw_snapshot_stale_seconds` | Konfigurations-Snapshot |
| `aigw_counter_backend_degraded`, `aigw_counter_replay_*` | Zähler-Backend (Redis-Ausfall, Nachtragen) |
| `aigw_log_written_total`, `aigw_log_dropped_total`, `aigw_log_write_failed_total`, `aigw_log_export_errors_total` | Request-Log und Export |
| `aigw_mcp_calls_total`, `aigw_mcp_call_duration_seconds` | MCP-Gateway |
| `aigw_server_connections_active`, `aigw_server_connections_rejected_total`, `aigw_server_timeouts_total` | Client-Verbindungen je Listener, wegen `max_connections` abgewiesene (`503`), Timeouts (`kind`: header, body, idle, keepalive) |

Vorschläge für Alarme:

- `aigw_snapshot_stale_seconds > 300` – Datenbank nicht erreichbar bzw. Reload hängt.
- `aigw_counter_backend_degraded == 1` – Redis/Postgres-Sync ausgefallen (Limits nur lokal).
- `rate(aigw_log_dropped_total[5m]) > 0` – Request-Log-Puffer läuft über (DB zu langsam).
- `aigw_deployment_breaker_state == 1` länger als einige Minuten – Anbieter gestört.
- Anteil `aigw_upstream_errors_total` an `aigw_upstream_attempts_total` je Anbieter.
- `rate(aigw_server_connections_rejected_total[5m]) > 0` – Verbindungsgrenze erreicht (Last oder Angriff; `server.*.max_connections`).

## 3. OpenTelemetry

Eigener OTLP/HTTP-Exporter (Protobuf), kein Collector-SDK nötig:

```yaml
observe:
  otlp:
    endpoint: http://otel-collector:4318     # /v1/traces und /v1/logs werden angehängt
    headers: {}                              # z. B. authorization für SaaS-Backends (per ENV setzen)
    service_name: ai-gw
    traces:
      enabled: true
      sampling_ratio: 1.0
      parent_based: true
      accept_traceparent: true               # eingehenden traceparent übernehmen
      propagate: true                        # traceparent an Anbieter weitergeben
    logs:
      enabled: false                         # Request-Log zusätzlich als OTLP-Logs
      include_payload: false
```

- Spans je Anfrage mit GenAI-Semantic-Conventions (Wurzel, Auth, Plugins, Routing, je
  Upstream-Versuch); Export gebündelt und mit Wiederholung (`observe.otlp.retry`).
- Ein nicht erreichbarer Collector beeinträchtigt den Hot Path nicht (Puffer, verworfene
  Batches zählen in den Export-Fehlermetriken).
- Docker Compose: Profil `otel` in [deploy/compose/](../../deploy/compose/docker-compose.yaml).
- Link vom Log-Explorer der Admin-UI ins Tracing-System: `admin.ui.trace_url`
  (`{traceId}` wird ersetzt).

## 4. Logs

- Betriebslog auf stderr: `--log-format json` bzw. `AIGW_LOG_FORMAT=json` für
  Log-Aggregation; Filter `AIGW_LOG` (z. B. `info,aigw_proxy=debug`).
- Request-Log je Anfrage in der Datenbank (`observe.request_log`, Aufbewahrung
  `observe.retention.*`), auswertbar in der Admin-UI (Log-Explorer, Analytics).
- Optional als JSON-Lines-Datei (`observe.jsonl`, Rotation täglich/nach Größe, `SIGHUP` öffnet
  neu für externes logrotate).
- Payload-Logging (`observe.payload: off|metadata|masked|full`) nur bewusst einschalten;
  Organisation/Team können es begrenzen (`payloadModeMax`).
