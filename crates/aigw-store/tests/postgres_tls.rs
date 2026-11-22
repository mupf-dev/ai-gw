// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Postgres mit TLS (z. B. Azure Database for PostgreSQL Flexible Server,
//! `require_secure_transport=on`).
//!
//! - `sslmode=require`: verschlüsselt, Zertifikat ungeprüft (wie libpq).
//! - `sslmode=verify-full`: Kette und Hostname geprüft – gegen die eingebetteten Wurzeln
//!   (webpki-roots, Mozilla-Liste; unabhängig vom Container-Image, also auch im distroless-Image)
//!   plus optional `sslrootcert=<pem>`.
//!
//! Der Wurzel-Test läuft immer. Die Verbindungstests nur mit `AIGW_TEST_POSTGRES_TLS_URL`
//! (Postgres mit `ssl=on`, URL ohne Abfrage, Host passend zum Serverzertifikat) und
//! `AIGW_TEST_POSTGRES_TLS_CA` (PEM der ausstellenden CA, nicht in webpki-roots).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use aigw_core::resource::new_id;
use aigw_store::Store;
use sqlx::AssertSqlSafe;

/// Wurzeln der aktuellen Zertifikatskette von Azure Database for PostgreSQL (DigiCert Global
/// Root G2, Microsoft RSA/ECC Root Certificate Authority 2017) sind in den eingebetteten
/// Wurzeln enthalten, sodass `verify-full` ohne `sslrootcert` funktioniert. Die alte
/// „DigiCert Global Root CA“ (G1) hat Mozilla entfernt – Server mit dieser alten Kette brauchen
/// `sslrootcert` (Doku: docs/betrieb/konfiguration.md).
#[test]
fn azure_postgres_roots_are_embedded() {
    let has = |cn: &str| {
        webpki_roots::TLS_SERVER_ROOTS.iter().any(|ta| {
            ta.subject
                .as_ref()
                .windows(cn.len())
                .any(|w| w == cn.as_bytes())
        })
    };
    for cn in [
        "DigiCert Global Root G2",
        "Microsoft RSA Root Certificate Authority 2017",
        "Microsoft ECC Root Certificate Authority 2017",
    ] {
        assert!(has(cn), "{cn} fehlt in webpki-roots");
    }
    assert!(
        !has("DigiCert Global Root CA"),
        "G1 wieder enthalten – Doku zu sslrootcert anpassen"
    );
}

fn tls_env() -> Option<(String, String)> {
    let url = std::env::var("AIGW_TEST_POSTGRES_TLS_URL").ok()?;
    let ca = std::env::var("AIGW_TEST_POSTGRES_TLS_CA").ok()?;
    Some((url, ca))
}

async fn uses_ssl(url: &str) -> bool {
    let pool = sqlx::PgPool::connect(url).await.unwrap();
    let (ssl,): (bool,) =
        sqlx::query_as("SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()")
            .fetch_one(&pool)
            .await
            .unwrap();
    pool.close().await;
    ssl
}

#[tokio::test]
async fn postgres_tls_modes() {
    let Some((base, ca)) = tls_env() else {
        eprintln!("AIGW_TEST_POSTGRES_TLS_URL/_CA nicht gesetzt – TLS-Test übersprungen");
        return;
    };
    // require: verschlüsselt ohne Zertifikatsprüfung (selbstsigniert ok).
    let require = format!("{base}?sslmode=require");
    assert!(uses_ssl(&require).await);

    // Store inkl. Migration über TLS in einer Wegwerf-Datenbank.
    let db = format!("aigw_tls_{}", new_id().simple());
    let admin = sqlx::PgPool::connect(&require).await.unwrap();
    sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {db}")))
        .execute(&admin)
        .await
        .unwrap();
    let (prefix, _) = base.rsplit_once('/').expect("URL mit Datenbankpfad");
    let store = Store::connect(&format!("{prefix}/{db}?sslmode=require"))
        .await
        .unwrap();
    store.migrate().await.unwrap();
    drop(store);

    // verify-full mit eigener CA: Kette + Hostname geprüft.
    let verified = format!("{base}?sslmode=verify-full&sslrootcert={ca}");
    assert!(uses_ssl(&verified).await);
    let store = Store::connect(&format!(
        "{prefix}/{db}?sslmode=verify-full&sslrootcert={ca}"
    ))
    .await
    .unwrap();
    store.migrate().await.unwrap();
    drop(store);

    // verify-full ohne die CA: selbstsigniert ist nicht in den eingebetteten Wurzeln → Fehler
    // (die Prüfung ist also wirklich aktiv).
    let e = sqlx::PgPool::connect(&format!("{base}?sslmode=verify-full")).await;
    assert!(
        e.is_err(),
        "verify-full ohne passende Wurzel muss scheitern"
    );
    // verify-full mit falschem Hostnamen scheitert ebenfalls.
    let other = base.replacen("localhost", "127.0.0.1", 1);
    if other != base {
        let e =
            sqlx::PgPool::connect(&format!("{other}?sslmode=verify-full&sslrootcert={ca}")).await;
        assert!(e.is_err(), "Hostname muss geprüft werden");
    }

    sqlx::query(AssertSqlSafe(format!("DROP DATABASE {db} WITH (FORCE)")))
        .execute(&admin)
        .await
        .unwrap();
}
