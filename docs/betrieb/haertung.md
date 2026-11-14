# Härtung und sicherer Betrieb

Betriebshinweise aus dem Sicherheitsreview ([sicherheitsreview.md](sicherheitsreview.md),
AIGW-58). Verweise `S-xx` beziehen sich auf die dortigen Befunde.

## Netz und Listener

- **Datenebene (`:4000`) und Steuerebene (`:4001`) trennen.** Die Steuerebene (Admin-API, UI,
  `/metrics`) nur im Verwaltungsnetz bzw. hinter dem Ingress mit Zugriffsbeschränkung
  veröffentlichen. Zusammenlegen nur, wenn beide ohnehin gleich exponiert sind.
- **Reverse Proxy / Ingress vor die Datenebene**: TLS-Terminierung (ai-gw spricht selbst nur
  Klartext-HTTP/1.1 und h2c, daher auch kein TLS-Handshake-Timeout im Gateway), Verbindungs-
  und Ratenlimits **je Client-IP**, maximale Header-Größe. Der Gateway begrenzt selbst
  Body-Größe (`server.data.max_body_bytes`), liest den Body erst nach gültigem Schlüssel (S-02)
  und hat Verbindungsgrenzen und Lese-Timeouts (S-32, unten).
- **Verbindungsgrenzen und Lese-Timeouts (S-32, AIGW-80).** Je Listener höchstens
  `server.data.max_connections` (Standard 10 000) bzw. `server.control.max_connections`
  (1 024) gleichzeitige Verbindungen (`0` = unbegrenzt); weitere bekommen sofort `503` mit
  `retry-after: 1` und werden geschlossen, ohne die Accept-Schleife aufzuhalten. Pro offener
  Verbindung rechnen: ≈ 85 KiB unter Last (leistung.md) und ein Dateideskriptor –
  `ulimit -n` über `max_connections` beider Listener plus Upstream-Verbindungen setzen.
  Timeouts im Abschnitt `server.http` (gelten für beide Listener, `0` = aus):

  | Feld | Standard | Wirkung |
  |---|---|---|
  | `header_read_timeout_secs` | 10 | HTTP/1: vom ersten Byte einer Anfrage bis zum vollständigen Kopf; danach hartes Schließen (Slowloris). |
  | `body_read_timeout_secs` | 30 | höchste Pause zwischen zwei Teilen des Anfragekörpers. |
  | `body_min_bytes_per_sec` | 1024 | Mindestrate des Anfragekörpers nach `body_read_timeout_secs` Vorlauf (32 MiB brauchen damit höchstens ≈ 9 h; strenger stellen, wenn große Uploads nicht vorkommen). |
  | `idle_timeout_secs` | 120 | Keep-Alive-Leerlauf (HTTP/1 und HTTP/2, geordnet: HTTP/2 mit GOAWAY). **Größer** als das Leerlauf-Timeout eines vorgeschalteten Load Balancers wählen (z. B. AWS ALB 60 s), sonst schließt der Gateway Verbindungen, die der LB noch für offen hält. |
  | `http2_max_concurrent_streams` | 256 | gleichzeitige Streams je HTTP/2-Verbindung. |
  | `http2_keepalive_interval_secs` / `http2_keepalive_timeout_secs` | 30 / 20 | PING an HTTP/2-Gegenstellen; ohne Antwort wird die Verbindung geschlossen (tote Clients mit laufenden Streams). |

  Lese-Timeouts gelten nur, solange der Gateway auf Daten des Clients wartet. Laufende
  Antworten – auch minutenlange SSE-Streams mit langen Pausen (Reasoning) – brechen sie nicht
  ab; die Verbindung zählt bis zum Ende der Antwort als beschäftigt. Kopf- und Leerlauf-Timeout
  werden im Abstand des halben kleineren Werts geprüft (höchstens 5 s), greifen also bis zu
  diesem Abstand später. Metriken auf `/metrics`: `aigw_server_connections_active`,
  `aigw_server_connections_accepted_total`, `aigw_server_connections_rejected_total`
  (Grenze erreicht – Alarm lohnt sich), `aigw_server_timeouts_total{kind=header|body|idle|keepalive}`,
  jeweils mit `listener="data"|"control"`.
- **HSTS** am Proxy setzen, wenn TLS dort endet (der Gateway setzt kein HSTS).
- Ausgehend: Egress-Firewall zusätzlich zur SSRF-Prüfung – nur Anbieter, IdP, Webhook-/MCP-
  Ziele erlauben; Cloud-Metadaten (`169.254.169.254`, `fd00:ec2::254`) auf Netzebene sperren.

## Ausnahmen für private Upstreams

- `admin.allow_private_upstreams` / `allow_insecure_upstreams` so eng wie möglich: einzelne
  Hosts oder kleine CIDR-Bereiche statt `10.0.0.0/8`. Die Freigabe gilt für **alle** Rollen,
  auch für Webhook-/content-safety-Bindungen von Team-Admins (S-22).
- Link-Local/Metadaten nie über eine Namensregel freigeben.

## Master-Key und Secrets

- Master-Key mit `aigw keygen` erzeugen (Datei 0600). Getrennt von DB-Backups aufbewahren;
  wer DB **und** Master-Key hat, hat alle Zugangsdaten.
- Rotation über `secrets.previous_key_files`; alte Schlüssel nach Neuverschlüsselung
  entfernen.
- GitOps: Secrets nur als Verweis (`valueFrom.env` mit `AIGW_SECRET_*`, Dateien unter
  `gitops.secret_dir`), nie im Repo.
- Secrets und ProviderCredentials sind global. Bindungen mit `secret`/`credential` legt nur
  der globale `admin` an (S-01).

## Snapshot-Cache

- `snapshot.cache_file` nur auf einem Pfad, den ausschließlich der Gateway-Benutzer schreiben
  kann (Datei 0600). `snapshot.cache_max_age_secs` (Standard 24 h) begrenzt, wie alt ein
  Cache beim Kaltstart ohne DB sein darf – kürzer wählen, wenn Sperrungen schnell wirken
  müssen (S-07). `0` (unbegrenzt) nur bewusst.

## Admin-Zugang

- **Bootstrap-Token:** `admin.auth.bootstrap: auto` (Standard) – nach dem ersten Admin
  (OIDC oder Admin-Key mit Rolle `admin`) endgültig aus. Ein erzeugtes Token erscheint
  einmalig auf **stderr**, nicht im strukturierten Log (S-06); stderr-Ausgabe des ersten
  Starts nicht dauerhaft archivieren. `bootstrap: on` nur kurzzeitig; danach Prüfen, dass
  `GET /admin/api/v1/auth/info` `bootstrap: false` meldet.
- **OIDC** bevorzugen; Session-`max_age` kurz halten (Gruppenrollen gelten bis Session-Ende,
  S-24). UI nur über HTTPS (Cookies sind `Secure`). Keine fremd kontrollierten
  Schwester-Subdomains unter dem Admin-Host (S-23).
- **Rolle `gitops` ist admin-gleichwertig** (S-05): `config:apply` kann Nutzer und Rollen
  schreiben. CI-Keys mit kurzer Laufzeit (`max_admin_key_lifetime`), nur im CI-Secret-Store,
  Apply nur aus geschützten Branches.
- Einladungslinks über einen vertraulichen Kanal verschicken; sie sind Inhaber-Token (S-25).

## Datenebene

- Keys mit Ablaufdatum, Limits und Budgets je Org/Team; widerrufene Keys wirken sofort
  (Snapshot), auch über Instanzen hinweg nach dem Reload.
- Limit-Metrik `concurrency` wird **noch nicht durchgesetzt** (S-12) – `GET
  /admin/api/v1/usage/limit-warnings` zeigt betroffene Objekte.
- Guardrails:
  - `content-safety` prüft den ganzen Text in bis zu 16 Fenstern zu `maxChars`; längere
    Eingaben scheitern nach `failMode` (Standard `closed`). `maxChars` passend zum Dienst wählen.
  - Im Satzmodus prüft `content-safety` auch Tool-Call-Argumente (eigener Puffer je
    Tool-Block, Freigabe erst nach bestandener Prüfung am Blockende; S-10, AIGW-89).
  - Das `pii`-Plugin mit Ausgabeprüfung verwirft lesbares Reasoning (S-11).
  - Prüfmodell-Aufrufe (`provider: model`) werden dem Key/Team zugerechnet (Kosten, Budget,
    Token-Limits; S-13, AIGW-89). `maxChecks` (Standard 100) begrenzt die Prüfungen je
    Anfrage; bei langen Stream-Antworten im Satzmodus passend wählen und `sentenceOnError:
    block` lassen, sonst geht der Rest nach Erreichen der Grenze ungeprüft hinaus. Die
    Modellgruppe des Prüfmodells zusätzlich mit Kapazitätsgrenzen versehen (die Zulassung
    vorab kennt die Prüfkosten nicht).
  - Azure AI Content Safety bevorzugt mit Entra ID statt API-Key (`auth: entraId`,
    ProviderCredential mit `spec.entra`, z. B. Managed Identity; Rolle *Cognitive Services
    User* auf der Ressource). Das Token geht nur an Azure-Hosts (`*.cognitiveservices.azure.*`,
    `*.services.ai.azure.com`); den Credential-Verweis darf nur der globale `admin` setzen.
- Policy-Webhooks auf `failMode: closed` setzen und mit Secret signieren lassen.
- Payload-Logging: `full` nur, wo nötig. Lesen dürfen nur `admin`/`auditor`. Bei
  Compliance-Pflichten an der Organisation `payloadModeMax` setzen und den Modus mit
  `payloadModeLocked: true` sperren (S-09, AIGW-79) – sonst kann ein `team-admin` den Modus für
  eigene Keys innerhalb des Maximums ändern (auch abschalten).

## Audit

- `GET /admin/api/v1/audit-log/verify` regelmäßig (z. B. täglich) ausführen und
  `headHash` extern (WORM-Speicher, Ticket) ablegen; ein abweichender Kopf zeigt Löschungen
  am Ende. Befunde „ohne Kettenfelder“ deuten auf direkte DB-Schreibzugriffe (S-08).
- DB-Schreibrechte nur für den Gateway-Benutzer; Admin-Zugriffe auf die DB protokollieren.

## Abhängigkeiten

- `scripts/check.sh` führt `cargo deny` aus; zusätzlich `pnpm audit --prod` im UI.
- Ausnahme `RUSTSEC-2026-0459` bis 2027-01-31 erneut prüfen (deny.toml).
