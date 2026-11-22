#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 mupf-dev
# SPDX-License-Identifier: FSL-1.1-ALv2
#
# Release-Vorprüfung und Release-Notizen (AIGW-119).
#
#   scripts/release-notes.sh 0.1.0            # prüft und gibt den CHANGELOG-Abschnitt aus
#   scripts/release-notes.sh v0.1.0           # führendes "v" (Tag-Name) ist erlaubt
#   ALLOW_PLACEHOLDER=1 scripts/release-notes.sh 0.1.0   # Datum darf noch offen sein (Probelauf)
#
# Prüft, dass die Version zu Cargo.toml ([workspace.package]), Cargo.lock (Paket aigw),
# ui/package.json und dem Helm-Chart (version, appVersion) passt und dass CHANGELOG.md einen Abschnitt `## [<version>] – JJJJ-MM-TT` mit
# echtem Datum hat. Ausgabe auf stdout: der Abschnitt ohne Überschrift (für `gh release create
# --notes-file`), ergänzt um Hinweise zu Prüfsummen und Attestierungen. Fehler → Exit-Code 1.
set -euo pipefail
cd "$(dirname "$0")/.."

raw=${1:?Aufruf: scripts/release-notes.sh <version>}
v=${raw#v}
fail() { echo "FEHLER: $*" >&2; exit 1; }

# SemVer 2.0 (ohne Build-Metadaten): 1.2.3 oder 1.2.3-rc.1
[[ "$v" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?$ ]] ||
  fail "'$raw' ist keine SemVer-Version"

cargo_v=$(awk '/^\[workspace\.package\]/{f=1;next} /^\[/{f=0} f&&/^version *=/{gsub(/[" ]/,"",$0); sub(/version=/,""); print; exit}' Cargo.toml)
[[ "$cargo_v" == "$v" ]] || fail "Cargo.toml [workspace.package] version = $cargo_v, erwartet $v"
lock_v=$(awk '/^name = "aigw"$/{getline; gsub(/[" ]/,""); sub(/version=/,""); print; exit}' Cargo.lock)
[[ "$lock_v" == "$v" ]] || fail "Cargo.lock: aigw $lock_v, erwartet $v (cargo update -w)"
ui_v=$(sed -n 's/^  "version": "\(.*\)",$/\1/p' ui/package.json | head -1)
[[ "$ui_v" == "$v" ]] || fail "ui/package.json version = $ui_v, erwartet $v"

chart=deploy/helm/ai-gw/Chart.yaml
chart_v=$(sed -n 's/^version: *//p' "$chart")
chart_app=$(sed -n 's/^appVersion: *"\{0,1\}\([^"]*\)"\{0,1\}$/\1/p' "$chart")
[[ "$chart_v" == "$v" && "$chart_app" == "$v" ]] ||
  fail "$chart: version $chart_v / appVersion $chart_app, erwartet $v"

esc=${v//./\\.}
head=$(grep -E "^## \[$esc\]" CHANGELOG.md || true)
[[ -n "$head" ]] || fail "CHANGELOG.md: kein Abschnitt '## [$v]'"
if ! [[ "$head" =~ ^##\ \[$esc\]\ –\ [0-9]{4}-[0-9]{2}-[0-9]{2}$ ]]; then
  [[ "${ALLOW_PLACEHOLDER:-0}" == 1 ]] ||
    fail "CHANGELOG.md: Überschrift '$head' braucht ein Datum (## [$v] – JJJJ-MM-TT)"
  echo "WARNUNG: Datum im CHANGELOG noch offen ($head)" >&2
fi
grep -q "^\[$esc\]: " CHANGELOG.md || fail "CHANGELOG.md: Link-Verweis '[$v]: …' fehlt"

# Abschnitt bis zur nächsten Versionsüberschrift bzw. den Link-Verweisen am Ende.
notes=$(awk -v h="$head" '
  $0 == h {f=1; next}
  f && (/^## \[/ || /^\[[^]]+\]: /) {exit}
  f {print}' CHANGELOG.md | sed -e '/./,$!d')
[[ -n "${notes//[[:space:]]/}" ]] || fail "CHANGELOG.md: Abschnitt $v ist leer"

printf '%s\n' "$notes"   # $(…) hat Leerzeilen am Ende bereits entfernt
cat <<EOF

---

**Installation:** Binaries (linux amd64/arm64, Admin-UI eingebettet) unten unter *Assets*,
Container-Image \`ghcr.io/mupf-dev/ai-gw:$v\` (Multi-Arch), Helm-Chart \`deploy/helm/ai-gw\` –
siehe [docs/betrieb/installation.md](https://github.com/mupf-dev/ai-gw/blob/v$v/docs/betrieb/installation.md).

**Prüfen:** \`sha256sum -c SHA256SUMS --ignore-missing\`; Herkunft (SLSA-Provenance, Sigstore):
\`gh attestation verify ai-gw-$v-linux-amd64.tar.gz -R mupf-dev/ai-gw\` bzw.
\`gh attestation verify oci://ghcr.io/mupf-dev/ai-gw:$v -R mupf-dev/ai-gw\`.
SBOM (CycloneDX): \`ai-gw-$v.cdx.json\`.
EOF
