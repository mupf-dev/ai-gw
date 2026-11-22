// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Feste Test-Bundles aus der Sigstore-Konformitätssuite (`sigstore/sigstore-conformance`,
//! `test/assets/bundle-verify`, Apache-2.0; Stand e275ffb) gegen das Public-Good-Trust-Root –
//! ohne Netz. Echte Bundles aus Fulcio/Rekor (Produktion) belegen die Interoperabilität;
//! die `_fail`-Fälle decken je einen Prüfschritt ab. Rekor-v2-/TSA-/DSSE-Fälle sind nicht
//! enthalten (nicht unterstützt, siehe Crate-Doku).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use aigw_sigstore::{Artifact, Error, Identity, Policy, TrustedRoot, verify_at};

const ISSUER: &str = "https://token.actions.githubusercontent.com";
const IDENTITY: &str = "https://github.com/sigstore-conformance/extremely-dangerous-public-oidc-beacon/.github/workflows/extremely-dangerous-oidc-beacon.yml@refs/heads/main";
/// „Jetzt“ für die Tests (2026-11-01): fest, damit die Ergebnisse nicht von der Uhr abhängen.
const NOW: i64 = 1_793_491_200;

fn data() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}

fn escape(s: &str) -> String {
    regex::escape(s)
}

fn read_opt(p: &Path) -> Option<Vec<u8>> {
    std::fs::read(p).ok()
}

/// Prüft einen Fall wie die Konformitätssuite (Identität exakt).
fn run(case: &str) -> Result<aigw_sigstore::Verified, Error> {
    let dir = data().join("conformance").join(case);
    let root_json = read_opt(&dir.join("trusted_root.json"))
        .unwrap_or_else(|| std::fs::read(data().join("trusted_root.public-good.json")).unwrap());
    let root = TrustedRoot::from_json(&root_json)?;
    let artifact = read_opt(&dir.join("artifact"))
        .unwrap_or_else(|| std::fs::read(data().join("conformance/a.txt")).unwrap());
    let identity = read_opt(&dir.join("identity"))
        .map(|b| String::from_utf8(b).unwrap().trim().to_owned())
        .unwrap_or_else(|| IDENTITY.to_owned());
    let issuer = read_opt(&dir.join("issuer"))
        .map(|b| String::from_utf8(b).unwrap().trim().to_owned())
        .unwrap_or_else(|| ISSUER.to_owned());
    let policy = Policy::new(vec![
        Identity::new(&escape(&issuer), &escape(&identity)).unwrap(),
    ]);
    let bundle = std::fs::read(dir.join("bundle.sigstore.json")).unwrap();
    verify_at(&root, &policy, &bundle, Artifact::Bytes(&artifact), NOW)
}

#[test]
fn happy_paths() {
    for case in [
        "happy-path-v0.1",
        "happy-path-v0.2",
        "happy-path-v0.3",
        "happy-path-v0.3-new-mediaType",
        "trust-root-tlog-validity-end-inclusive",
    ] {
        let v = run(case).unwrap_or_else(|e| panic!("{case}: {e}"));
        assert_eq!(v.subject, IDENTITY, "{case}");
        assert_eq!(v.issuer, ISSUER, "{case}");
        assert_eq!(v.log_url, "https://rekor.sigstore.dev", "{case}");
        assert!(v.integrated_time > 1_600_000_000, "{case}");
    }
}

#[test]
fn failing_cases() {
    // (Fall, erwartete Fehlerart als Text-Präfix der Anzeige)
    let cases: &[(&str, &str)] = &[
        ("bundle-empty-certificate-chain_fail", "Bundle ungültig"),
        ("bundle-from-wrong-instance_fail", "Transparenz-Log"),
        ("bundle-invalid-base64-signature_fail", "Bundle ungültig"),
        ("bundle-malformed-json_fail", "Bundle ungültig"),
        ("bundle-negative-log-index_fail", "Transparenz-Log"),
        ("bundle-unknown-version_fail", "nicht unterstützt"),
        ("bundle-with-root-cert_fail", "Zertifikat"),
        ("checkpoint-bad-keyhint_fail", "Transparenz-Log: Checkpoint"),
        ("checkpoint-wrong-roothash_fail", "Transparenz-Log"),
        ("inclusion-proof-corrupted-hash_fail", "Transparenz-Log"),
        ("incorrect-public-key_fail", "Transparenz-Log"),
        // Der Zeitpunkt (Mai 2026) lag bei Erstellung der Suite in der Zukunft; heute fällt der Fall
        // auf, weil er außerhalb der Zertifikatsgültigkeit liegt (Zukunft: `digest_only_and_tampering`).
        ("integrated-time-in-future_fail", "Zertifikat"),
        (
            "invalid-checkpoint-signature_fail",
            "Transparenz-Log: Checkpoint",
        ),
        // Schlüssel des CT-Logs nicht lesbar → schon das Trust-Root wird abgelehnt.
        ("invalid-ct-key_fail", "Trust-Root"),
        ("invalid-inclusion-proof_fail", "Transparenz-Log"),
        ("message-digest-mismatch_fail", "Signatur"),
        ("set-invalid-signature_fail", "Transparenz-Log"),
        ("signature-mismatch_fail", "Signatur"),
        ("trust-root-tlog-missing-validity-start_fail", "Trust-Root"),
        ("wrong-hashedrekord-artifact_fail", "Transparenz-Log"),
        ("wrong-hashedrekord-cert-and-sig_fail", "Transparenz-Log"),
        ("wrong-hashedrekord-entry_fail", "Transparenz-Log"),
        ("wrong-material_fail", "Signatur"),
    ];
    let mut wrong = Vec::new();
    for (case, want) in cases {
        match run(case) {
            Ok(_) => wrong.push(format!("{case}: unerwartet gültig")),
            Err(e) if !e.to_string().starts_with(want) => {
                wrong.push(format!("{case}: erwartet {want:?}, erhalten {e}"));
            }
            Err(_) => {}
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

fn happy() -> (TrustedRoot, Vec<u8>, Vec<u8>) {
    let root = TrustedRoot::from_json(
        &std::fs::read(data().join("trusted_root.public-good.json")).unwrap(),
    )
    .unwrap();
    let bundle =
        std::fs::read(data().join("conformance/happy-path-v0.3/bundle.sigstore.json")).unwrap();
    let artifact = std::fs::read(data().join("conformance/a.txt")).unwrap();
    (root, bundle, artifact)
}

#[test]
fn identity_matching_is_anchored() {
    let (root, bundle, artifact) = happy();
    let check = |issuer: &str, subject: &str| {
        verify_at(
            &root,
            &Policy::new(vec![Identity::new(issuer, subject).unwrap()]),
            &bundle,
            Artifact::Bytes(&artifact),
            NOW,
        )
    };
    // Regex über den Workflow-Pfad (typischer Einsatz: Releases eines Repos).
    let ok = check(
        r"https://token\.actions\.githubusercontent\.com",
        r"https://github\.com/sigstore-conformance/[^/]+/\.github/workflows/.+@refs/heads/main",
    );
    assert!(ok.is_ok(), "{ok:?}");
    // Teiltreffer reichen nicht (verankert).
    let partial = check(
        r"https://token\.actions\.githubusercontent\.com",
        r"sigstore-conformance",
    );
    assert!(
        matches!(partial, Err(Error::Identity { .. })),
        "{partial:?}"
    );
    let issuer = check(r"https://accounts\.google\.com", r".*");
    assert!(matches!(issuer, Err(Error::Identity { .. })), "{issuer:?}");
    let prefix = check(
        r"https://token\.actions\.githubusercontent\.com.*",
        r"https://github\.com/sigstore-conformance/extremely-dangerous-public-oidc-beacon/.*@refs/heads/mai",
    );
    assert!(matches!(prefix, Err(Error::Identity { .. })), "{prefix:?}");
    // Mehrere Identitäten: eine passende genügt.
    let multi = verify_at(
        &root,
        &Policy::new(vec![
            Identity::new(r"https://accounts\.google\.com", r".*@example\.org").unwrap(),
            Identity::new(r"https://token\.actions\.githubusercontent\.com", r".*").unwrap(),
        ]),
        &bundle,
        Artifact::Bytes(&artifact),
        NOW,
    );
    assert!(multi.is_ok(), "{multi:?}");
    assert!(Identity::new("(", "x").is_err());
    assert!(Identity::new("", "x").is_err());
}

#[test]
fn digest_only_and_tampering() {
    let (root, bundle, artifact) = happy();
    let policy = Policy::new(vec![Identity::new(".*", ".*").unwrap()]);
    let mut d = aigw_sigstore::sha256(&artifact);
    assert!(verify_at(&root, &policy, &bundle, Artifact::Sha256(d), NOW).is_ok());
    // Anderer Inhalt.
    let mut other = artifact.clone();
    other.push(b'!');
    let e = verify_at(&root, &policy, &bundle, Artifact::Bytes(&other), NOW).unwrap_err();
    assert!(matches!(e, Error::Signature(_)), "{e:?}");
    d[0] ^= 1;
    assert!(verify_at(&root, &policy, &bundle, Artifact::Sha256(d), NOW).is_err());
    // Vor dem Log-Eintrag („jetzt“ in der Vergangenheit): Eintrag liegt in der Zukunft.
    let e = verify_at(
        &root,
        &policy,
        &bundle,
        Artifact::Bytes(&artifact),
        1_600_000_000,
    )
    .unwrap_err();
    assert!(matches!(e, Error::Tlog(_)), "{e:?}");
}

#[test]
fn inclusion_proof_policy() {
    let (root, bundle, artifact) = happy();
    // Proof entfernen: mit Standardrichtlinie abgelehnt, mit require_inclusion_proof=false (nur
    // SET) gültig.
    let mut v: serde_json::Value = serde_json::from_slice(&bundle).unwrap();
    v["verificationMaterial"]["tlogEntries"][0]
        .as_object_mut()
        .unwrap()
        .remove("inclusionProof");
    let stripped = serde_json::to_vec(&v).unwrap();
    let mut policy = Policy::new(vec![Identity::new(".*", ".*").unwrap()]);
    let e = verify_at(&root, &policy, &stripped, Artifact::Bytes(&artifact), NOW).unwrap_err();
    assert_eq!(e, Error::Tlog("Inclusion-Proof fehlt".into()));
    policy.require_inclusion_proof = false;
    assert!(verify_at(&root, &policy, &stripped, Artifact::Bytes(&artifact), NOW).is_ok());
    // SET entfernen: immer abgelehnt (signierter Zeitpunkt nötig).
    v["verificationMaterial"]["tlogEntries"][0]
        .as_object_mut()
        .unwrap()
        .remove("inclusionPromise");
    let no_set = serde_json::to_vec(&v).unwrap();
    assert!(verify_at(&root, &policy, &no_set, Artifact::Bytes(&artifact), NOW).is_err());
}

#[test]
fn trust_root_errors() {
    assert!(TrustedRoot::from_json(b"{}").is_err());
    assert!(TrustedRoot::from_json(b"kein json").is_err());
    let mut v: serde_json::Value = serde_json::from_slice(
        &std::fs::read(data().join("trusted_root.public-good.json")).unwrap(),
    )
    .unwrap();
    v["mediaType"] = "application/json".into();
    assert!(TrustedRoot::from_json(&serde_json::to_vec(&v).unwrap()).is_err());
    // Trust-Root ohne passende Fulcio-CA: Kette nicht vertrauenswürdig.
    let (_, bundle, artifact) = happy();
    let mut v: serde_json::Value = serde_json::from_slice(
        &std::fs::read(data().join("trusted_root.public-good.json")).unwrap(),
    )
    .unwrap();
    // CT-Log mit falschem (gültigem) Schlüssel: SCT ungültig.
    let mut ct = v.clone();
    let tlog_key = ct["tlogs"][0]["publicKey"]["rawBytes"].clone();
    for l in ct["ctlogs"].as_array_mut().unwrap() {
        l["publicKey"]["rawBytes"] = tlog_key.clone();
    }
    let root = TrustedRoot::from_json(&serde_json::to_vec(&ct).unwrap()).unwrap();
    let e = verify_at(
        &root,
        &Policy::new(vec![Identity::new(".*", ".*").unwrap()]),
        &bundle,
        Artifact::Bytes(&artifact),
        NOW,
    )
    .unwrap_err();
    assert!(matches!(e, Error::Sct(_)), "{e:?}");
    let cas = v["certificateAuthorities"].as_array_mut().unwrap();
    cas.remove(1);
    let root = TrustedRoot::from_json(&serde_json::to_vec(&v).unwrap()).unwrap();
    let e = verify_at(
        &root,
        &Policy::new(vec![Identity::new(".*", ".*").unwrap()]),
        &bundle,
        Artifact::Bytes(&artifact),
        NOW,
    )
    .unwrap_err();
    assert!(matches!(e, Error::Certificate(_)), "{e:?}");
}
