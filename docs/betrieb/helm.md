# Helm-Chart

Das Chart [`deploy/helm/ai-gw`](../../deploy/helm/ai-gw/) (AIGW-119) installiert ai-gw in
Kubernetes ≥ 1.29 (geprüft gegen die Schemas von 1.34, Helm 3 und 4). Es ist aus dem
kustomize-Grundgerüst [`deploy/kubernetes`](../../deploy/kubernetes/) abgeleitet und ergänzt
Werte für Image, Startkonfiguration, Secrets, Persistenz, Ingress und Monitoring.

| Objekt | Wann | Inhalt |
|---|---|---|
| ConfigMap `<release>` | immer | Startkonfiguration `aigw.yaml` aus `config` |
| Deployment `<release>` | immer | `aigw serve`, nicht-root, schreibgeschütztes Root-FS, Probes |
| Service `<release>-data` | immer | Datenebene `:4000` (`/v1/…`, `/mcp`) – Ziel für Ingress/Clients |
| Service `<release>-control` | immer | Steuerebene `:4001` (Admin-API/-UI, `/metrics`) – nur intern |
| ServiceAccount | `serviceAccount.create` | ohne API-Token (`automountServiceAccountToken: false`) |
| PersistentVolumeClaim `<release>-data` | `persistence.enabled` (Standard) | `/data`: SQLite und Snapshot-Cache; bleibt bei `helm uninstall` erhalten |
| PodDisruptionBudget | `podDisruptionBudget.enabled` und `replicaCount > 1` | `minAvailable: 1` |
| Ingress | `ingress.enabled` | nur Datenebene |
| ServiceMonitor | `serviceMonitor.enabled` | Prometheus-Operator, `/metrics` der Steuerebene |

Mit dem Release-Namen `ai-gw` heißen die Objekte `ai-gw`, `ai-gw-data` usw. (sonst
`<release>-ai-gw`).

## Schnellstart (eine Instanz, SQLite)

```sh
kubectl create namespace aigw
docker run --rm ghcr.io/mupf-dev/ai-gw:0.1.0 keygen > aigw-master-key      # oder: aigw keygen
kubectl -n aigw create secret generic aigw-master-key --from-file=master-key=./aigw-master-key
helm install ai-gw ./deploy/helm/ai-gw -n aigw --set masterKey.existingSecret=aigw-master-key
kubectl -n aigw logs deploy/ai-gw | grep Bootstrap-Token                   # einmaliges Admin-Token
kubectl -n aigw port-forward svc/ai-gw-control 4001:4001                   # → http://localhost:4001/admin/
```

Den Master-Key sicher aufbewahren (Backup!): ohne ihn sind gespeicherte Zugangsdaten und der
Snapshot-Cache nicht mehr lesbar ([konfiguration.md](konfiguration.md), Rotation mit
`aigw config rekey`). Das Chart legt **keine** Secrets an; Geheimnisse gehören nicht in
`values.yaml`.

SQLite läuft nur mit **einer** Instanz (`replicaCount: 1`, Strategie `Recreate`, PVC
`ReadWriteOnce`); `helm template/install` bricht bei `replicaCount > 1` mit SQLite ab.

## Produktion (mehrere Replikate, Postgres + Redis)

Postgres und Redis/Valkey stellt der Betreiber (verwalteter Dienst oder eigenes Chart). Beispiel
mit allen Optionen: [`ci/production-values.yaml`](../../deploy/helm/ai-gw/ci/production-values.yaml).

```sh
kubectl -n aigw create secret generic aigw \
  --from-file=master-key=./aigw-master-key \
  --from-literal=store-url='postgres://aigw:<passwort>@postgres:5432/aigw?sslmode=verify-full'
kubectl -n aigw create secret generic aigw-providers --from-literal=openai=sk-…
helm install ai-gw ./deploy/helm/ai-gw -n aigw -f my-values.yaml
```

```yaml
# my-values.yaml
replicaCount: 2
masterKey: {existingSecret: aigw}
store: {existingSecret: aigw, key: store-url}      # → AIGW_STORE__URL
config:
  store: {url: "postgres://aigw@postgres:5432/aigw", max_connections: 20}
  counters: {backend: redis, redis_url: "redis://redis:6379/0"}   # Limits über alle Pods
persistence: {enabled: false}                      # /data = emptyDir (nur Snapshot-Cache)
extraEnv:                                          # Secret-Verweise der Ressourcen (AIGW_SECRET_<NAME>)
  - name: AIGW_SECRET_OPENAI_API_KEY
    valueFrom: {secretKeyRef: {name: aigw-providers, key: openai}}
ingress:
  enabled: true
  className: nginx
  annotations:
    nginx.ingress.kubernetes.io/proxy-buffering: "off"    # Streaming (SSE)
    nginx.ingress.kubernetes.io/proxy-read-timeout: "600"
  hosts: [{host: aigw.example.org, paths: [{path: /, pathType: Prefix}]}]
  tls: [{secretName: aigw-tls, hosts: [aigw.example.org]}]
serviceMonitor: {enabled: true, labels: {release: prometheus}}
```

Redis mit Passwort: `config.counters.redis_url` ohne Passwort lassen und die vollständige URL
per `extraEnv` (`AIGW_COUNTERS__REDIS_URL` aus einem Secret) setzen. Mehrere Instanzen und
Zähler-Backends: [datenhaltung.md](datenhaltung.md).

## Wichtige Werte

| Wert | Standard | Bedeutung |
|---|---|---|
| `image.repository` / `tag` / `digest` | `ghcr.io/mupf-dev/ai-gw` / appVersion / – | `digest` (`sha256:…`) hat Vorrang vor `tag` |
| `replicaCount` | `1` | > 1 nur mit Postgres |
| `config` | Listener `0.0.0.0:4000/4001`, SQLite `/data/aigw.db`, Master-Key-Datei, Snapshot-Cache, JSON-Log, `allow_mock: false` | Startkonfiguration als YAML ([konfiguration.md](konfiguration.md)); Helm führt eigene Werte mit den Standardwerten zusammen, `null` entfernt einen Schlüssel |
| `masterKey.existingSecret` / `key` | **Pflicht** / `master-key` | Secret mit dem Master-Key, eingehängt unter `/run/secrets/aigw/master-key` (Modus 0440, Gruppe = fsGroup) |
| `store.existingSecret` / `key` | – / `store-url` | Datenbank-URL aus einem Secret (`AIGW_STORE__URL`, überschreibt `config.store.url`) |
| `extraEnv`, `extraEnvFrom` | – | weitere Umgebung, z. B. `AIGW_SECRET_*`, `AIGW_BOOTSTRAP_TOKEN` (aus Secret) |
| `extraVolumes`, `extraVolumeMounts` | – | z. B. CA-Bündel (`upstream.ca_file`), WASM-Plugins, GitOps-Secrets |
| `persistence.*` | PVC 1 Gi `ReadWriteOnce` | `enabled: false` → emptyDir (`emptyDirSizeLimit`), `existingClaim`, `storageClass` (`-` = leer) |
| `tokenizer.fetch.*` | aus | Init-Container `aigw tokenizer fetch` lädt die tiktoken-Rangtabellen nach `/data/tiktoken` (nicht im Image, AIGW-92; braucht ausgehendes HTTPS oder `baseUrl` eines Spiegels); ohne: Token-Schätzung per Heuristik |
| `resources` | requests 500m/256Mi, limit 1Gi | Messwerte: [leistung.md](leistung.md) |
| `probes.*` | readiness `/readyz`, liveness/startup `/healthz` (Port `control`) | Perioden und Schwellen anpassbar |
| `podDisruptionBudget.*` | an, `minAvailable: 1` | nur bei `replicaCount > 1` |
| `ingress.*` | aus | nur Datenebene; Steuerebene nicht öffentlich ([haertung.md](haertung.md)) |
| `serviceMonitor.*` | aus | Intervall 30 s, zusätzliche `labels` für den Prometheus-Selektor |
| `podSecurityContext`, `securityContext` | UID/GID 65532, `runAsNonRoot`, seccomp `RuntimeDefault`, keine Capabilities, kein Privilege Escalation, Root-FS schreibgeschützt | entspricht Pod Security Standard „restricted“ |
| `serviceAccount.*` | anlegen, ohne Token | `annotations` z. B. für Workload Identity |
| `nodeSelector`, `tolerations`, `affinity`, `topologySpreadConstraints`, `priorityClassName`, `podAnnotations`, `podLabels`, `imagePullSecrets` | – | wie üblich |

Eine geänderte Startkonfiguration (`config`) rollt die Pods neu aus (Annotation
`checksum/config`). Ressourcen (Modellgruppen, Keys, Plugins …) gehören nicht in die
Startkonfiguration, sondern in die Datenbank – per Admin-UI/-API oder GitOps
(`aigw config apply`, [konfiguration.md](konfiguration.md)).

## Prüfen

- `scripts/k8s-check.sh` – `helm lint --strict` und `helm template` je Prüfvariante
  (`deploy/helm/ai-gw/ci/*-values.yaml`), kubeconform streng gegen Kubernetes 1.34 (CRDs wie
  ServiceMonitor aus dem [CRDs-catalog](https://github.com/datreeio/CRDs-catalog)), erwartete
  Objekte je Variante und Ablehnung ungültiger Werte (fehlender Master-Key, SQLite mit mehreren
  Replikaten). Läuft in CI (Job „Kubernetes-Manifeste und Helm-Chart“); Helm, kubectl und
  kubeconform werden mit SHA-256-Prüfung geladen.
- `scripts/helm-smoke.sh` – Wegwerf-Cluster mit [kind](https://kind.sigs.k8s.io/): Image bauen
  (oder `IMAGE=… SKIP_BUILD=1`), `helm install` mit SQLite auf PVC, `/readyz`, Admin-UI,
  Organisation + Key + Mock-Modell, Chat über die Datenebene, Neustart mit erhaltener
  Datenbank, `helm upgrade` mit geänderter Konfiguration, `helm uninstall` behält das PVC.

## Aktualisieren und Entfernen

```sh
helm upgrade ai-gw ./deploy/helm/ai-gw -n aigw -f my-values.yaml --set image.tag=0.1.1
helm uninstall ai-gw -n aigw          # PVC <release>-data bleibt (helm.sh/resource-policy: keep)
kubectl -n aigw delete pvc ai-gw-data # Datenbank endgültig löschen
```

Migrationen laufen beim Start (`aigw serve`); vor Upgrades die Datenbank sichern und die
Hinweise im CHANGELOG lesen ([datenhaltung.md](datenhaltung.md)). Bei SQLite (`Recreate`) ist
der Dienst während des Neustarts kurz nicht erreichbar.
