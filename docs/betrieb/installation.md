# Installation

ai-gw ist **ein** statisch gelinktes Rust-Binary (`aigw`, glibc) mit eingebetteter Admin-UI.
Es braucht zur Laufzeit nur eine Startkonfiguration (YAML), einen Master-Key und eine
Datenbank (SQLite-Datei oder Postgres).

| Port | Ebene | Inhalt | Exponieren |
|---|---|---|---|
| `4000` | Daten (`server.data.listen`) | `/v1/*` (OpenAI-/Anthropic-kompatibel), `/mcp`, `/healthz`, `/readyz` | Clients, hinter Reverse Proxy/Ingress mit TLS |
| `4001` | Steuerung (`server.control.listen`) | Admin-API `/admin/api/v1`, Admin-UI `/admin/`, `/metrics`, `/healthz`, `/readyz` | nur Verwaltungsnetz |

Gleiche Adresse für beide → ein gemeinsamer Listener (nur, wenn beide ohnehin gleich exponiert
sind, siehe [haertung.md](haertung.md)).

## Release-Artefakte

Jeder Release (Tag `v<version>`, [Releases](https://github.com/mupf-dev/ai-gw/releases)) liefert:

| Artefakt | Inhalt |
|---|---|
| `ai-gw-<version>-linux-amd64.tar.gz`, `…-linux-arm64.tar.gz` | Binary `aigw` (Admin-UI eingebettet, glibc ≥ 2.34: Debian 12+, Ubuntu 22.04+, RHEL/Alma/Rocky 9+), `config/aigw.example.yaml`, LICENSE, THIRD-PARTY.md, CHANGELOG |
| `ghcr.io/mupf-dev/ai-gw:<version>` | Multi-Arch-Image (amd64 + arm64), zusätzlich Tags `<major>.<minor>` und `latest` (nicht bei Vorabversionen) |
| `SHA256SUMS` | Prüfsummen der Archive und des SBOM |
| `ai-gw-<version>.cdx.json` | SBOM (CycloneDX) aus `Cargo.lock` und `ui/pnpm-lock.yaml` |
| `ai-gw-<version>.provenance.sigstore.json` | SLSA-Build-Provenance (Sigstore-Bundle) der Archive |
| Helm-Chart | [`deploy/helm/ai-gw`](../../deploy/helm/ai-gw/) im Quellstand des Tags, siehe [helm.md](helm.md) |

```sh
V=0.1.0; A=amd64        # oder arm64
curl -fLO https://github.com/mupf-dev/ai-gw/releases/download/v$V/ai-gw-$V-linux-$A.tar.gz
curl -fLO https://github.com/mupf-dev/ai-gw/releases/download/v$V/SHA256SUMS
sha256sum -c SHA256SUMS --ignore-missing
gh attestation verify ai-gw-$V-linux-$A.tar.gz -R mupf-dev/ai-gw      # Herkunft (optional)
tar -xzf ai-gw-$V-linux-$A.tar.gz && ./ai-gw-$V-linux-$A/aigw --version

docker pull ghcr.io/mupf-dev/ai-gw:$V
gh attestation verify oci://ghcr.io/mupf-dev/ai-gw:$V -R mupf-dev/ai-gw
```

Die Attestierungen (Provenance und SBOM) erzeugt der Release-Workflow mit
`actions/attest` über Sigstore; `gh attestation verify` prüft, dass das Artefakt aus diesem
Repository und dem Workflow `.github/workflows/release.yml` stammt. Ablauf für Maintainer:
[release.md](release.md).

## 1. Container-Image

### Bauen

```sh
docker build -t ai-gw:dev .                       # BuildKit nötig (RUN --mount)
docker build -t ai-gw:0.1.0 \
  --build-arg VERSION=0.1.0 \
  --build-arg REVISION=$(git rev-parse HEAD) \
  --build-arg CREATED=$(date -u +%Y-%m-%dT%H:%M:%SZ) .
```

Stufen ([Dockerfile](../../Dockerfile)):

1. **ui** – `node:24-bookworm-slim`, pnpm mit `--frozen-lockfile`, `vite build`; läuft immer
   auf der Build-Plattform (Ausgabe ist plattformunabhängig).
2. **build** – `rust:1-slim-bookworm`, Toolchain aus `rust-toolchain.toml`,
   `cargo build --release --locked -p aigw` mit `AIGW_UI_DIST` (UI eingebettet). Build-Cache
   für Registry und `target/` je Zielplattform. `--build-arg CARGO_FEATURES=--no-default-features`
   baut ohne mimalloc (System-Allokator) **und ohne eingebettete tiktoken-Rangtabellen**;
   nur ohne Tabellen: `--build-arg CARGO_FEATURES="--no-default-features --features mimalloc"`
   (dann `tokenizer.ranks_dir`, siehe [konfiguration.md](konfiguration.md), AIGW-95).
3. **Laufzeit** – `gcr.io/distroless/cc-debian12:nonroot`: glibc, libgcc, CA-Zertifikate,
   **keine Shell, kein Paketmanager**, Benutzer `65532:65532`.

Eigenschaften des Images:

| Eigenschaft | Wert |
|---|---|
| Benutzer | `65532:65532` (nonroot) |
| Einstieg | `ENTRYPOINT ["/usr/local/bin/aigw"]`, `CMD ["serve"]` |
| Konfiguration | `AIGW_CONFIG=/etc/aigw/aigw.yaml` ([config/aigw.container.yaml](../../config/aigw.container.yaml): Listener `0.0.0.0`, SQLite `/data/aigw.db`) |
| Daten | `VOLUME /data` (gehört 65532, Arbeitsverzeichnis) |
| Healthcheck | `aigw healthcheck` → `GET /readyz` der Steuerebene (Adresse aus der Konfiguration), Intervall 15 s |
| Root-FS | schreibgeschützt möglich (`--read-only`); geschrieben wird nur unter `/data` |
| Labels | OCI (`org.opencontainers.image.*`), Lizenz `FSL-1.1-ALv2`, Version/Revision per Build-Arg |
| Größe | ≈ 26 MB komprimiert (Registry/Download), ≈ 65 MB entpackt (linux/amd64, Stand 0.1.0; arm64 siehe unten): Binary `aigw` 39 MB (UI, Tokenizer-Tabellen [Feature `embedded-tokenizers`, Standard], Migrationen eingebettet) + distroless/cc ≈ 25 MB. `docker images` zeigt mit containerd-Image-Store ≈ 93 MB, weil dort komprimierte und entpackte Schichten zusammen zählen |

### Mehrere Architekturen (amd64 + arm64)

Seit AIGW-91 werden **linux/amd64 und linux/arm64** gebaut und getestet. Die Stufe `build`
läuft immer auf der Build-Plattform (`FROM --platform=$BUILDPLATFORM`) und kompiliert für die
Zielplattform **quer**: Rust-Ziel `aarch64-unknown-linux-gnu` bzw. `x86_64-unknown-linux-gnu`,
Linker und C-Compiler aus Debian (`gcc-aarch64-linux-gnu`, `libc6-dev-arm64-cross`). Der
Compiler läuft also nativ, nicht unter QEMU – der arm64-Build dauert etwa so lange wie der
amd64-Build. Die C-Anteile werden mit demselben Cross-GCC gebaut:

| Komponente | Besonderheit bei arm64 |
|---|---|
| aws-lc-rs / aws-lc-sys (TLS, AES-GCM) | `cc`-Builder mit vorgenerierten Bindings – kein cmake, kein bindgen, kein Go; `ring` ist nicht im Abhängigkeitsbaum |
| mimalloc (Standard-Allokator, AIGW-57) | baut quer ohne Anpassung; `CARGO_FEATURES=--no-default-features` schaltet auf den System-Allokator |
| SQLite (`sqlite-bundled`) | wird mitkompiliert, keine Laufzeit-Abhängigkeit |
| glibc | Cross-Sysroot aus bookworm (2.36) = glibc von `distroless/cc-debian12` |

```sh
scripts/build-images.sh                               # beide Architekturen → ai-gw:dev-amd64, ai-gw:dev-arm64
PLATFORMS=linux/arm64 TAG=0.1.0 SAVE_DIR=dist scripts/build-images.sh   # + dist/ai-gw-0.1.0-arm64.tar.gz
SMOKE=1 scripts/build-images.sh                       # danach scripts/container-smoke.sh je Image
PUSH=registry.example.org/ai-gw TAG=0.1.0 scripts/build-images.sh       # Multi-Arch-Manifest pushen
```

Direkt mit buildx (Multi-Arch-Manifest braucht den containerd-Image-Store oder einen
`docker-container`-Builder):

```sh
docker buildx build --platform linux/amd64,linux/arm64 \
  --build-arg VERSION=0.1.0 -t registry.example.org/ai-gw:0.1.0 --push .
```

Testen eines fremden Architektur-Images: `SMOKE=1` registriert bei Bedarf QEMU über
`tonistiigi/binfmt` (privilegierter Container, nur auf Build-Maschinen) und führt den
vollständigen Container-Smoke (SQLite read-only, Compose mit Postgres/Redis/OTel) mit dem
arm64-Image aus. Auf einem arm64-Host genügt `docker load` des Tarballs:

```sh
gunzip -c ai-gw-0.1.0-arm64.tar.gz | docker load
docker run -d --name aigw -p 4001:4001 -e AIGW_MASTER_KEY=$(docker run --rm ai-gw:0.1.0-arm64 keygen) ai-gw:0.1.0-arm64
curl -fsS http://127.0.0.1:4001/readyz
```

Größen (Stand AIGW-91, gemessen mit `scripts/build-images.sh`):

| Plattform | Binary `aigw` | Image komprimiert (`docker save \| gzip`) | Bau (8 vCPU, Registry im Cache) |
|---|---|---|---|
| linux/amd64 | ≈ 39 MiB | ≈ 25 MiB | vergleichbar |
| linux/arm64 (quer kompiliert) | ≈ 33 MiB | ≈ 24 MiB | ≈ 3,5 min |

Auf einem kleinen arm64-Host (2 vCPU) belegt ein frisch gestarteter Container mit SQLite
≈ 22 MiB RAM.

### Starten (SQLite, Einzelinstanz)

```sh
docker run --rm ai-gw:dev keygen > aigw-master-key     # 32 Byte, hex
docker volume create aigw-data
docker run -d --name aigw --read-only --cap-drop ALL --security-opt no-new-privileges \
  -p 127.0.0.1:4000:4000 -p 127.0.0.1:4001:4001 \
  -e AIGW_MASTER_KEY="$(cat aigw-master-key)" \
  -v aigw-data:/data ai-gw:dev
docker logs aigw 2>&1 | grep Bootstrap-Token            # einmaliges Admin-Token
```

Besser als die Umgebungsvariable ist eine Datei (Docker-/Kubernetes-Secret):
`AIGW_SECRETS__MASTER_KEY_FILE=/run/secrets/aigw-master-key`. Die Datei muss für UID 65532
lesbar sein (`chown 65532:65532`, Rechte `0400`) – Docker Compose übernimmt bei
dateibasierten Secrets Besitzer und Rechte der Host-Datei.

Hilfsbefehle im Container (keine Shell nötig):

```sh
docker exec aigw aigw healthcheck                       # Exit 0 = bereit
docker exec aigw aigw config check                      # Startkonfiguration prüfen
docker run --rm -v "$PWD":/w:ro ai-gw:dev config validate /w/resources.yaml
```

## 2. Docker Compose

Unter [`deploy/compose/`](../../deploy/compose/):

| Datei | Inhalt |
|---|---|
| `docker-compose.sqlite.yaml` | Einzelinstanz mit SQLite im Volume |
| `docker-compose.yaml` | ai-gw + Postgres 17 + Redis 7, optional OpenTelemetry-Collector (Profil `otel`); Startkonfiguration [config/examples/ha-postgres-redis/aigw.yaml](../../config/examples/ha-postgres-redis/aigw.yaml) |
| `env.example` | Vorlage für `.env` (nur Platzhalter) |
| `otel-collector.yaml` | Collector-Konfiguration (OTLP/HTTP → `debug`) |

```sh
cd deploy/compose
cp env.example .env && $EDITOR .env                    # POSTGRES_PASSWORD setzen
mkdir -p secrets
docker run --rm ai-gw:dev keygen > secrets/aigw-master-key
sudo chown 65532:65532 secrets/aigw-master-key && sudo chmod 0400 secrets/aigw-master-key
docker compose up -d
docker compose ps                                       # aigw: healthy
# mit Collector:
AIGW_OTLP_ENDPOINT=http://otel-collector:4318 docker compose --profile otel up -d
```

Der Container läuft schreibgeschützt (`read_only: true`), ohne Capabilities und mit
`no-new-privileges`. Geheimnisse: Master-Key als Docker-Secret (Datei), DB-Passwort über
`.env` → `AIGW_STORE__URL`; `.env` und `secrets/` sind per `.gitignore` ausgeschlossen.

## 3. Kubernetes

**Helm (empfohlen):** [`deploy/helm/ai-gw`](../../deploy/helm/ai-gw/) – Werte für Image,
Startkonfiguration, Secrets (`existingSecret`), Persistenz, Ingress, ServiceMonitor; siehe
[helm.md](helm.md).

**kustomize-Grundgerüst:** [`deploy/kubernetes/`](../../deploy/kubernetes/) enthält:
ConfigMap (Startkonfiguration), Deployment (2 Replikate, nicht-root, `readOnlyRootFilesystem`,
Probes auf `/healthz`/`/readyz` der Steuerebene), getrennte Services `aigw-data` und
`aigw-control`, PodDisruptionBudget. Postgres und Redis werden vorausgesetzt (Betreiber-Dienst
oder eigenes Chart).

```sh
kubectl create namespace aigw
kubectl -n aigw create secret generic aigw \
  --from-file=master-key=./aigw-master-key \
  --from-literal=store-url='postgres://aigw:<passwort>@postgres:5432/aigw'
kubectl -n aigw apply -k deploy/kubernetes
```

Prüfen ohne Cluster: `scripts/k8s-check.sh` rendert das Grundgerüst mit `kubectl kustomize`
und validiert alle Objekte streng mit [kubeconform](https://github.com/yannh/kubeconform) gegen
die Schemas von Kubernetes 1.34 (Werkzeuge werden bei Bedarf mit SHA-256-Prüfung geladen; läuft
auch in CI). Mit erreichbarem Cluster zusätzlich `KUBE_SERVER_DRY_RUN=1 scripts/k8s-check.sh`
(`kubectl apply --dry-run=server`). `kubectl apply --dry-run=client` braucht ebenfalls einen
erreichbaren API-Server (Discovery). Das Image ist Multi-Arch-fähig; gemischte Knotenpools
amd64/arm64 brauchen ein Multi-Arch-Manifest (`PUSH=… scripts/build-images.sh`).

Ingress/Gateway-API, NetworkPolicies und Ressourcengrenzen an die eigene Umgebung anpassen;
die Steuerebene nicht öffentlich veröffentlichen.

## 4. Binary ohne Container

Fertige Binaries: [Release-Artefakte](#release-artefakte). Selbst bauen:

```sh
cargo build --release --locked -p aigw                   # UI vorher: pnpm -C ui build
install -m 0755 target/release/aigw /usr/local/bin/aigw  # bzw. ai-gw-<version>-linux-<arch>/aigw
install -d -o aigw -g aigw -m 0750 /var/lib/aigw
aigw keygen --out /etc/aigw/master-key                   # Rechte 0600, Besitzer anpassen
aigw config check -c /etc/aigw/aigw.yaml
aigw serve -c /etc/aigw/aigw.yaml
```

Beispiel für eine systemd-Unit:

```ini
[Unit]
Description=ai-gw AI-Gateway
After=network-online.target
Wants=network-online.target

[Service]
User=aigw
Group=aigw
Environment=AIGW_CONFIG=/etc/aigw/aigw.yaml
ExecStart=/usr/local/bin/aigw serve
# SIGHUP öffnet Exportdateien (JSON-Lines) neu – für logrotate
ExecReload=/bin/kill -HUP $MAINPID
Restart=on-failure
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/lib/aigw
PrivateTmp=true
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

`LimitNOFILE` großzügig wählen: jede Client- und jede Upstream-Verbindung belegt einen
Dateideskriptor (Messung mit 4 000 parallelen Streams: [leistung.md](leistung.md) §4).

## 5. Erste Schritte nach der Installation

1. `/readyz` prüfen: `curl -s localhost:4001/readyz` → `{"status":"ready",…}`.
2. Admin-UI `http://localhost:4001/admin/` mit dem **Bootstrap-Token** öffnen (einmalig auf
   stderr bzw. `AIGW_BOOTSTRAP_TOKEN`); es erlischt automatisch, sobald ein OIDC-Admin oder
   ein Admin-API-Key mit Rolle `admin` existiert ([oidc.md](oidc.md)).
3. Organisation, Zugangsdaten, Modellgruppen und Deployments anlegen – per UI oder GitOps
   (`aigw config apply`, Beispiele in [config/examples/](../../config/examples/)).
4. API-Key für Clients anlegen und testen:
   `curl localhost:4000/v1/chat/completions -H "authorization: Bearer sk-aigw-…" -d '{"model":"gpt-4o-mini","messages":[{"role":"user","content":"Hallo"}]}'`
