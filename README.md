# ai-gw

Eigenes AI-Gateway nach Vorbild von LiteLLM, Kong AI Gateway und Azure AI Gateway.

- **Gateway + Admin-API:** Rust (tokio, hyper/axum, rustls), ein Binary
- **Admin-UI:** Svelte 5 + Vite + Bits UI + Tailwind, ins Binary eingebettet
- **Datenhaltung:** SQLite (Standard) oder Postgres; Zähler-Backends `local`, `postgres-sync`, `redis`

Entscheidungen: [docs/konzept/00-entscheidungen.md](docs/konzept/00-entscheidungen.md)

## Lizenz

AGPL-3.0-or-later, siehe [LICENSE](LICENSE).
