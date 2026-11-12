// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Wählt das Verzeichnis der einzubettenden Admin-UI.
//!
//! Reihenfolge: `AIGW_UI_DIST` (Pfad, z. B. im Dockerfile) → `<repo>/ui/dist` (falls gebaut) →
//! `ui-fallback/` (Platzhalterseite). Damit baut der Workspace auch ohne Node-Werkzeugkette.
//! Das gewählte Verzeichnis wird als `AIGW_UI_DIR` an `rust-embed` übergeben.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=AIGW_UI_DIST");
    let manifest =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into()));
    let ui_root = manifest.join("../../ui");
    let candidates = [
        std::env::var("AIGW_UI_DIST").ok().map(PathBuf::from),
        Some(ui_root.join("dist")),
    ];
    let chosen = candidates
        .into_iter()
        .flatten()
        .find(|d| d.join("index.html").is_file());
    let (dir, kind) = match chosen {
        Some(d) => (d, "ui"),
        None => (manifest.join("ui-fallback"), "fallback"),
    };
    let dir = dir.canonicalize().unwrap_or(dir);

    // Neu übersetzen, wenn sich die Assets ändern oder eine UI gebaut wird (ui/dist entsteht).
    // Ein nicht existierender Pfad würde den Build-Schritt bei jedem Lauf auslösen, daher wird
    // im Fallback-Fall das (existierende) ui/-Verzeichnis beobachtet.
    println!("cargo:rerun-if-changed={}", dir.display());
    if kind == "fallback" && ui_root.is_dir() {
        println!("cargo:rerun-if-changed={}", ui_root.display());
    }
    println!("cargo:rustc-env=AIGW_UI_DIR={}", dir.display());
    println!("cargo:rustc-env=AIGW_UI_KIND={kind}");
}
