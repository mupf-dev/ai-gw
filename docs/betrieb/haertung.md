# Härtung und sicherer Betrieb

Betriebshinweise aus dem Sicherheitsreview ([sicherheitsreview.md](sicherheitsreview.md),
AIGW-58). Verweise `S-xx` beziehen sich auf die dortigen Befunde.

## Netz und Listener

- **Datenebene (`:4000`) und Steuerebene (`:4001`) trennen.** Die Steuerebene (Admin-API, UI,
  `/metrics`) nur im Verwaltungsnetz bzw. hinter dem Ingress mit Zugriffsbeschränkung
  veröffentlichen. Zusammenlegen nur, wenn beide ohnehin gleich exponiert sind.
- **Reverse Proxy / Ingress vor die Datenebene** (S-32): TLS-Terminierung, Header-/Body-
  Lese-Timeouts (Slowloris), Verbindungs- und Ratenlimits je Client-IP, maximale
  Header-Größe. Der Gateway selbst begrenzt Body-Größe (`server.data.max_body_bytes`) und
  liest den Body erst nach gültigem Schlüssel (S-02), hat aber keine globale
  Verbindungsgrenze.
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
  - Für Tool-Calls im Stream (S-10) bei Bedarf `outputMode: buffered`.
  - Das `pii`-Plugin mit Ausgabeprüfung verwirft lesbares Reasoning (S-11).
  - Prüfmodell-Aufrufe (`provider: model`) zählen nicht gegen Limits des Keys (S-13) –
    eigene Kostenstelle/Modellgruppe mit Kapazitätsgrenzen verwenden.
- Policy-Webhooks auf `failMode: closed` setzen und mit Secret signieren lassen.
- Payload-Logging: `full` nur, wo nötig. Lesen dürfen nur `admin`/`auditor`. Ein
  `team-admin` kann den Modus für eigene Keys ändern (S-09) – bei Compliance-Pflichten die
  Audit-Einträge zu `payloadMode` überwachen.

## Audit

- `GET /admin/api/v1/audit-log/verify` regelmäßig (z. B. täglich) ausführen und
  `headHash` extern (WORM-Speicher, Ticket) ablegen; ein abweichender Kopf zeigt Löschungen
  am Ende. Befunde „ohne Kettenfelder“ deuten auf direkte DB-Schreibzugriffe (S-08).
- DB-Schreibrechte nur für den Gateway-Benutzer; Admin-Zugriffe auf die DB protokollieren.

## Abhängigkeiten

- `scripts/check.sh` führt `cargo deny` aus; zusätzlich `pnpm audit --prod` im UI.
- Ausnahme `RUSTSEC-2026-0459` bis 2027-01-31 erneut prüfen (deny.toml).
