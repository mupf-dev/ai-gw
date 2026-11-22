# Release erstellen (Maintainer)

Releases baut ausschließlich der Workflow
[`.github/workflows/release.yml`](../../.github/workflows/release.yml), ausgelöst durch einen
Tag `v<version>` (SemVer, z. B. `v0.1.0`, Vorabversion `v0.2.0-rc.1`). Es sind **keine**
Repository-Secrets nötig: `GITHUB_TOKEN` genügt für GHCR, Attestierungen und den Release.

## Ablauf des Workflows

1. **Vorprüfung** – `scripts/release-notes.sh <version>`: Tag = Version in `Cargo.toml`
   (`[workspace.package]`), `Cargo.lock` (Paket `aigw`) und `ui/package.json`; CHANGELOG hat
   einen Abschnitt `## [<version>] – JJJJ-MM-TT` mit echtem Datum und den Link-Verweis.
2. **CI** – derselbe Prüflauf wie `ci.yml` (als wiederverwendbarer Workflow): `check.sh`
   (strikt), THIRD-PARTY.md, kustomize/Helm/kubeconform.
3. **Artefakte und Image** – `scripts/release-build.sh` mit `SMOKE=1`:
   - Binaries linux/amd64 und linux/arm64 aus der Dockerfile-Stufe `binary` (dieselbe
     Build-Stufe wie das Image, arm64 quer kompiliert ohne QEMU), Archive reproduzierbar
     (feste Zeitstempel = Commit-Zeit, Besitzer, Reihenfolge);
   - SBOM (CycloneDX, syft) und `SHA256SUMS`;
   - Images je Architektur laden, `scripts/container-smoke.sh` (arm64 unter QEMU);
   - Multi-Arch-Image nach `ghcr.io/mupf-dev/ai-gw` mit Tags `<version>`, `<major>.<minor>`,
     `latest` (Vorabversionen nur `<version>`), OCI-Labels und -Annotationen;
   - signierte Attestierungen (`actions/attest`, Sigstore): SLSA-Provenance und SBOM für
     Archive und Image (Image-Attestierungen zusätzlich in der Registry).
4. **GitHub-Release** – Titel `ai-gw <version>`, Notizen aus dem CHANGELOG-Abschnitt plus
   Installations-/Prüfhinweise, alle Artefakte als Assets; Vorabversionen als Prerelease.

Warum glibc statt musl: Binary und Image entstehen aus derselben, gemessenen Build-Stufe
(aws-lc-rs, mimalloc, SQLite mit glibc, [leistung.md](leistung.md)); musl bräuchte eine zweite
Cross-Toolchain und eigene Messungen. Mindestens nötig ist die glibc von Debian 12 (2.36).

## Release vorbereiten

1. CHANGELOG: Inhalte aus `[Unreleased]` in einen neuen Abschnitt
   `## [<version>] – JJJJ-MM-TT` verschieben (Datum = Tag des Releases), Link-Verweise
   `[Unreleased]: …compare/v<version>...HEAD` und `[<version>]: …releases/tag/v<version>`
   anpassen.
2. Version setzen: `Cargo.toml` (`[workspace.package] version`), `cargo update -w` (Cargo.lock),
   `ui/package.json`, `deploy/helm/ai-gw/Chart.yaml` (`version` und `appVersion`),
   `deploy/kubernetes/kustomization.yaml` (`newTag`).
3. Lokal bzw. auf der Buildbox prüfen:
   ```sh
   scripts/release-notes.sh <version> >/dev/null          # Vorprüfung
   AIGW_STRICT=1 scripts/check.sh && scripts/k8s-check.sh
   VERSION=<version> SMOKE=1 scripts/release-build.sh     # dist/ wie im Release, ohne Push
   scripts/helm-smoke.sh                                  # kind
   ```
4. Optional Probelauf in GitHub: Workflow „release“ manuell starten (`workflow_dispatch`) –
   baut und testet alles, lädt `dist/` als Workflow-Artefakt hoch, **ohne** Push, Attestierung
   und Release (CHANGELOG-Datum darf dabei noch offen sein).
5. Commit auf `main`, CI grün abwarten.

## Taggen

```sh
git tag -s v<version> -m "ai-gw <version>"     # signiert (ohne Signaturschlüssel: -a)
git push origin v<version>
```

## Nacharbeiten

- Beim **ersten** Release: Das GHCR-Paket `ai-gw` ist zunächst privat. Unter
  *Package settings* Sichtbarkeit auf **Public** stellen und prüfen, dass das Paket mit dem
  Repository verknüpft ist (Label `org.opencontainers.image.source`).
- Prüfen: `docker buildx imagetools inspect ghcr.io/mupf-dev/ai-gw:<version>` (amd64 + arm64),
  `gh attestation verify oci://ghcr.io/mupf-dev/ai-gw:<version> -R mupf-dev/ai-gw`,
  `gh attestation verify ai-gw-<version>-linux-amd64.tar.gz -R mupf-dev/ai-gw`.
- CHANGELOG: leeren Abschnitt `## [Unreleased]` beibehalten.

Schlägt der Workflow nach dem Push des Images fehl (z. B. beim Release-Schritt), den Lauf
erneut starten („Re-run failed jobs“); Tags werden dabei überschrieben, das Image ist für
denselben Commit inhaltlich gleich.
