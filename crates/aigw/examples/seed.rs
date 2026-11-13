// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Entwicklungshilfe: wendet GitOps-Manifeste (Modellgruppen, Deployments, Preise,
//! Organisationen, Teams) direkt auf eine Datenbank an (anlegen oder ersetzen).
//!
//! ```text
//! cargo run -p aigw --example seed -- <datenbank-url> <manifest.yaml>
//! ```
//!
//! Gedacht für `scripts/runtime-smoke.sh` und lokale Versuche, solange die Admin-API diese
//! Ressourcen noch nicht anlegen kann (AIGW-65). Ein laufendes `aigw serve` übernimmt die
//! Änderungen über den Reloader (`store.reload_poll_secs` bei SQLite aus anderem Prozess).
//! Zugangsdaten und API-Keys werden nicht unterstützt (brauchen den Master-Key bzw. erzeugen
//! Geheimnisse).

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;

use aigw_core::resource::{Manifest, ObjectMeta, ResourceSpec};
use aigw_store::{PlainResource, Store, StoreError};

const ACTOR: &str = "seed";

async fn upsert<R: PlainResource>(
    store: &Store,
    meta: ObjectMeta,
    spec: R,
) -> Result<&'static str, StoreError> {
    match store.get::<R>(&meta.name).await {
        Ok(_) => store.update(ACTOR, meta, spec).await.map(|_| "ersetzt"),
        Err(StoreError::NotFound { .. }) => {
            store.create(ACTOR, meta, spec).await.map(|_| "angelegt")
        }
        Err(e) => Err(e),
    }
}

async fn run(url: &str, path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let text = std::fs::read_to_string(path)?;
    let docs = Manifest::parse_yaml(&text)?;
    let store = Store::connect(url).await?;
    store.migrate().await?;
    for doc in docs {
        let kind = doc.spec.kind();
        let name = doc.metadata.name.clone();
        let meta = ObjectMeta {
            created_at: None,
            updated_at: None,
            ..doc.metadata
        };
        let what = match doc.spec {
            ResourceSpec::Organization(s) => upsert(&store, meta, s).await?,
            ResourceSpec::Team(s) => upsert(&store, meta, s).await?,
            ResourceSpec::ModelGroup(s) => upsert(&store, meta, s).await?,
            ResourceSpec::Deployment(s) => upsert(&store, meta, s).await?,
            ResourceSpec::Price(s) => upsert(&store, meta, s).await?,
            _ => return Err(format!("{kind} {name:?}: vom Seed-Helfer nicht unterstützt").into()),
        };
        println!("{kind} {name}: {what}");
    }
    println!("config_version {}", store.config_version().await?);
    store.close().await;
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let [_, url, path] = args.as_slice() else {
        eprintln!("Aufruf: seed <datenbank-url> <manifest.yaml>");
        return ExitCode::from(2);
    };
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("Tokio: {e}");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(run(url, path)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Fehler: {e}");
            ExitCode::FAILURE
        }
    }
}
