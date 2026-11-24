# Betriebshandbuch

Stand: 0.1.0 (MVP) · Zielgruppe: Betrieb/Plattform-Teams

| Dokument | Inhalt |
|---|---|
| [installation.md](installation.md) | Release-Artefakte, Binary, Container-Image, Docker Compose, Kubernetes, Image-Größe, Multi-Arch |
| [helm.md](helm.md) | Helm-Chart: Werte, Secrets, SQLite vs. Postgres, Ingress, ServiceMonitor, Prüfung |
| [release.md](release.md) | Release erstellen (Maintainer): Vorbereitung, Probelauf, Tag, Nacharbeiten |
| [konfiguration.md](konfiguration.md) | Startkonfiguration (YAML + `AIGW_<ABSCHNITT>__<FELD>`), Prüfen, Beispiele, Master-Key und Rotation, GitOps-Grenzen |
| [datenhaltung.md](datenhaltung.md) | SQLite vs. Postgres, Zähler-Backends/Redis, mehrere Instanzen (HA), Backup/Restore, Upgrade und Migrationen |
| [monitoring.md](monitoring.md) | Health-Endpunkte, Prometheus-Metriken, OpenTelemetry (Traces/Logs), Request-Log |
| [oidc.md](oidc.md) | Anmeldung an Admin-UI/-API über OIDC (Microsoft Entra ID, Keycloak), Rollen |
| [fehlersuche.md](fehlersuche.md) | Häufige Fehlerbilder und Diagnose |
| [haertung.md](haertung.md) | Härtung und sicherer Betrieb (aus dem Sicherheitsreview) |
| [leistung.md](leistung.md) | Last- und Latenzmessungen, Empfehlungen für Lastbetrieb |
| [rezept-accounts-rotieren.md](rezept-accounts-rotieren.md) | Rezept: mehrere Anbieter-Accounts mit Budget je Deployment nacheinander oder verteilt nutzen |
| [sicherheitsreview.md](sicherheitsreview.md) | Befunde des Sicherheitsreviews (AIGW-58) |

Schnellstart mit Docker: [README](../../README.md#schnellstart-mit-docker).

Architektur und Begriffe (Modellgruppe, Deployment, Snapshot, Plugin-Kette …):
[docs/konzept/01-architektur.md](../konzept/01-architektur.md).
