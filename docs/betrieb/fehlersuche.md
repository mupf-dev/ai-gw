# Fehlersuche

## 1. Erste Schritte

```sh
aigw config check -c aigw.yaml                 # Startkonfiguration inkl. Umgebung gültig?
aigw healthcheck -c aigw.yaml                  # /readyz der laufenden Instanz
curl -s localhost:4001/admin/api/v1/version    # Version, Snapshot-Version und -Alter
AIGW_LOG=info,aigw_proxy=debug aigw serve …    # mehr Details zum Hot Path
```

Im Container ohne Shell: `docker exec <name> aigw healthcheck`, `docker logs <name>`.
Jede Antwort der Datenebene trägt `x-request-id`; damit findet man die Anfrage im
Log-Explorer der Admin-UI, im Request-Log und (mit OTLP) als Trace.

## 2. Start

| Meldung / Symptom | Ursache und Abhilfe |
|---|---|
| `kein Master-Key: weder secrets.master_key_file noch AIGW_MASTER_KEY gesetzt` | Master-Key fehlt ([konfiguration.md](konfiguration.md) §2) |
| `Master-Key-Datei … nicht lesbar: Permission denied` | Datei für UID 65532 (Container) bzw. den Dienstbenutzer nicht lesbar – Besitzer/Rechte anpassen |
| `unknown field …` / `AIGW_…: unbekannter Abschnitt` | Tippfehler in YAML oder Umgebungsvariable; `aigw config check` zeigt die Stelle |
| `Migration … passt nicht zu diesem Stand` | Datenbank ist neuer als das Binary (Rückschritt) oder stammt aus der Zeit vor AIGW-73 ([datenhaltung.md](datenhaltung.md) §5) |
| `unable to open database file` | SQLite-Verzeichnis nicht beschreibbar (`/data` im Container, Volume-Rechte) bzw. `?mode=rwc` fehlt |
| Postgres `password authentication failed` / `connection refused` | `AIGW_STORE__URL` prüfen; Dienst erreichbar? Ohne Datenbank startet die Instanz nur mit gültigem `snapshot.cache_file` (dann `degraded`) |
| `Address already in use` | Port belegt; `server.data.listen`/`server.control.listen` ändern |
| `upstream: …` | `upstream.ca_file` nicht lesbar oder kein PEM |
| Container `unhealthy` | `docker inspect --format '{{json .State.Health}}' <name>`; eigene Konfiguration mit anderer Steuer-Adresse eingehängt, aber `AIGW_CONFIG` zeigt noch auf die Standarddatei |

## 3. Anfragen an die Datenebene

| Status | Bedeutung | Prüfen |
|---|---|---|
| `401` | Key fehlt, unbekannt, gesperrt, widerrufen oder abgelaufen | Key-Status in der Admin-UI |
| `403` | Modell für Key/Team nicht freigegeben, Guardrail blockiert | `allowedModels` an Key/Team; Log-Explorer zeigt das Plugin |
| `404` `model_not_found` | Modellgruppe/Alias existiert nicht oder hat keine aktiven Deployments | `GET /v1/models` mit demselben Key |
| `413` | Body > `server.data.max_body_bytes` | Limit erhöhen oder Client kürzen |
| `429` | Limit oder Budget erreicht; `Retry-After`, `x-ratelimit-*` | Analytics/Limit-Warnungen; bei strikten Budgets reserviert eine Anfrage ohne `max_tokens` 4 096 Ausgabe-Tokens (`limits.default_max_tokens`) |
| `502`/`503`/`504` | Anbieter-Fehler nach Retries/Fallbacks, alle Deployments offen (Breaker) oder Zeitlimit | Header `x-aigw-deployment`, `x-aigw-attempts`; Metriken `aigw_upstream_errors_total`, `aigw_deployment_breaker_state` |

Deployment wird abgelehnt mit „endpoint … nicht erlaubt“: SSRF-Schutz – private Ziele oder
`http://` müssen in `admin.allow_private_upstreams` bzw. `allow_insecure_upstreams` stehen
([haertung.md](haertung.md)).

## 4. Konfiguration wirkt nicht

- **SQLite:** Änderungen eines anderen Prozesses (z. B. `aigw config apply` neben dem laufenden
  Server) kommen erst mit der Versionsabfrage (`store.reload_poll_secs`, 30 s).
- **Postgres:** sofort per `NOTIFY`; sonst `aigw_snapshot_reload_total` und das Log
  („Konfigurations-Snapshot …“) prüfen.
- Startkonfiguration (YAML/ENV) wirkt nur nach Neustart.
- `gitops.read_only_admin: true` → Schreibrouten der Admin-API antworten mit `403`.

## 5. Leistung

- Hohe Latenz mit `counters.backend: redis` → Rundreisen im Hot Path; `postgres-sync` prüfen.
- `aigw_log_dropped_total` steigt → Datenbank kommt mit dem Request-Log nicht nach (SQLite unter
  Last: auf Postgres wechseln oder `observe.request_log: false`).
- Viele gleichzeitige Verbindungen → Dateideskriptor-Limit (`LimitNOFILE`, `ulimit -n`).
- Messwerte und Empfehlungen: [leistung.md](leistung.md).

## 6. GitOps

- `budget breached: Documents` → Manifest hat mehr als 1 024 Dokumente; aufteilen
  ([konfiguration.md](konfiguration.md) §3).
- `Manifest ungültig (n Fehler), nichts angewendet` → alle Fehler stehen darunter; Apply ist
  alles oder nichts.
- Fehlende `AIGW_SECRET_*`-Variablen → Kopfkommentar des Exports listet alle Platzhalter.
