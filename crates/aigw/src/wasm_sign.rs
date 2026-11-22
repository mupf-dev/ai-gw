// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! `aigw wasm keygen|pubkey|sign|verify` (AIGW-101): Ed25519-Signaturen für WASM-Module.
//!
//! Signiert wird `aigw-wasm-module-v1:<sha256 hex>` (Doku 17-wasm-plugins.md §3a). Der private
//! Schlüssel ist PKCS#8 (PEM oder DER; auch aus `openssl genpkey -algorithm ed25519`), der
//! öffentliche wird als PEM ausgegeben und gehört nach `wasm.trusted_keys[].public_key`.

use std::io::Write as _;
use std::path::PathBuf;

use clap::{Args, Subcommand, ValueEnum};

/// Unterbefehle von `aigw wasm`.
#[derive(Debug, Subcommand)]
pub(crate) enum WasmCommand {
    /// Erzeugt ein Ed25519-Schlüsselpaar: privater Schlüssel (PKCS#8-PEM, Rechte 0600) in
    /// `--out`, öffentlicher Schlüssel (PEM) auf die Standardausgabe.
    Keygen(KeygenArgs),
    /// Gibt den öffentlichen Schlüssel (PEM) zu einem privaten Schlüssel aus.
    Pubkey {
        /// Privater Schlüssel (PKCS#8, PEM oder DER).
        #[arg(long)]
        key: PathBuf,
    },
    /// Signiert ein Modul (`.wasm`).
    Sign(SignArgs),
    /// Prüft eine Signatur gegen einen öffentlichen Schlüssel.
    Verify(VerifyArgs),
}

/// Parameter von `aigw wasm keygen`.
#[derive(Debug, Clone, Args)]
pub(crate) struct KeygenArgs {
    /// Datei für den privaten Schlüssel (neu, Rechte 0600).
    #[arg(short, long)]
    pub(crate) out: PathBuf,
    /// Vorhandene Datei überschreiben.
    #[arg(long)]
    pub(crate) force: bool,
}

/// Ausgabeformat von `aigw wasm sign`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub(crate) enum SignFormat {
    /// Nur die Signatur (Base64).
    #[default]
    Signature,
    /// JSON `{sha256, keyId, signature}`.
    Json,
    /// Query-Parameter für `PUT /admin/api/v1/wasm-modules/{name}` (prozentkodiert).
    Query,
}

/// Parameter von `aigw wasm sign`.
#[derive(Debug, Clone, Args)]
pub(crate) struct SignArgs {
    /// Privater Schlüssel (PKCS#8, PEM oder DER).
    #[arg(long)]
    pub(crate) key: PathBuf,
    /// Schlüssel-ID (wie in `wasm.trusted_keys[].id`).
    #[arg(long)]
    pub(crate) key_id: String,
    /// Ausgabeformat.
    #[arg(long, value_enum, default_value_t = SignFormat::Signature)]
    pub(crate) format: SignFormat,
    /// Das Modul.
    pub(crate) module: PathBuf,
}

/// Parameter von `aigw wasm verify`.
#[derive(Debug, Clone, Args)]
pub(crate) struct VerifyArgs {
    /// Öffentlicher Schlüssel: Datei (PEM) oder Base64 direkt.
    #[arg(long)]
    pub(crate) public_key: String,
    /// Schlüssel-ID.
    #[arg(long)]
    pub(crate) key_id: String,
    /// Signatur (Base64).
    #[arg(long)]
    pub(crate) signature: String,
    /// Das Modul.
    pub(crate) module: PathBuf,
}

fn read(path: &PathBuf) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))
}

fn private_key(path: &PathBuf) -> Result<Vec<u8>, String> {
    aigw_core::wasm::private_key_der(&read(path)?)
}

fn out(text: &str) -> Result<(), String> {
    let mut o = std::io::stdout().lock();
    write!(o, "{text}").map_err(|e| e.to_string())
}

/// Signatur als Query-Parameter (Base64 → `+`, `/`, `=` prozentkodiert).
fn query(key_id: &str, signature: &str) -> String {
    let enc: String = signature
        .chars()
        .map(|c| match c {
            '+' => "%2B".to_owned(),
            '/' => "%2F".to_owned(),
            '=' => "%3D".to_owned(),
            c => c.to_string(),
        })
        .collect();
    format!("keyId={key_id}&signature={enc}")
}

/// Signiert den Inhalt `content`: (SHA-256, Signatur).
pub(crate) fn sign(der: &[u8], content: &[u8]) -> Result<(String, String), String> {
    let sha = aigw_core::wasm::sha256_hex(content);
    let sig = aigw_core::wasm::sign_sha256(der, &sha)?;
    Ok((sha, sig))
}

/// Führt `aigw wasm …` aus.
pub(crate) fn run(cmd: &WasmCommand) -> Result<(), String> {
    match cmd {
        WasmCommand::Keygen(a) => {
            let der = aigw_core::wasm::generate_pkcs8()?;
            let public = aigw_core::wasm::public_key_of(&der)?;
            let pem = aigw_core::wasm::private_key_pem(&der);
            crate::keygen::write_private(&a.out, pem.trim_end(), a.force).map_err(|e| {
                let hint = if e.kind() == std::io::ErrorKind::AlreadyExists {
                    " (überschreiben nur mit --force)"
                } else {
                    ""
                };
                format!("{}: {e}{hint}", a.out.display())
            })?;
            let _ = writeln!(
                std::io::stderr(),
                "Privater Schlüssel in {} (Rechte 0600). Öffentlicher Schlüssel für \
                 wasm.trusted_keys[].public_key:",
                a.out.display()
            );
            out(&aigw_core::wasm::public_key_pem(&public))
        }
        WasmCommand::Pubkey { key } => {
            let public = aigw_core::wasm::public_key_of(&private_key(key)?)?;
            out(&aigw_core::wasm::public_key_pem(&public))
        }
        WasmCommand::Sign(a) => {
            aigw_core::resource::validate_name(a.key_id.trim())
                .map_err(|e| format!("--key-id: {e}"))?;
            let (sha, signature) = sign(&private_key(&a.key)?, &read(&a.module)?)?;
            let key_id = a.key_id.trim();
            let text = match a.format {
                SignFormat::Signature => format!("{signature}\n"),
                SignFormat::Json => format!(
                    "{}\n",
                    serde_json::json!({"sha256": sha, "keyId": key_id, "signature": signature})
                ),
                SignFormat::Query => format!("sha256={sha}&{}\n", query(key_id, &signature)),
            };
            out(&text)
        }
        WasmCommand::Verify(a) => {
            let public_key = match std::fs::read_to_string(&a.public_key) {
                Ok(text) => text,
                Err(_) => a.public_key.clone(),
            };
            let trust = aigw_core::wasm::TrustPolicy::new(
                &[aigw_core::wasm::TrustedKey {
                    id: a.key_id.trim().to_owned(),
                    public_key,
                }],
                None,
            )?;
            let sig = aigw_core::wasm::ModuleSignature::parse(&a.key_id, &a.signature)?;
            let sha = aigw_core::wasm::sha256_hex(&read(&a.module)?);
            trust
                .verify(&sha, Some(&sig))
                .map_err(|e| format!("{}: {e}", a.module.display()))?;
            out(&format!("OK {sha} {}\n", sig.key_id))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keygen_sign_verify_roundtrip() {
        let dir = std::env::temp_dir().join(format!("aigw-wasm-sign-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("signing.pem");
        let module = dir.join("m.wasm");
        let _ = std::fs::remove_file(&key);
        std::fs::write(&module, b"\0asm\x0d\0\x01\0inhalt").unwrap();
        run(&WasmCommand::Keygen(KeygenArgs {
            out: key.clone(),
            force: false,
        }))
        .unwrap();
        // Kein stilles Überschreiben.
        assert!(
            run(&WasmCommand::Keygen(KeygenArgs {
                out: key.clone(),
                force: false,
            }))
            .is_err()
        );
        let der = private_key(&key).unwrap();
        let (sha, sig) = sign(&der, b"\0asm\x0d\0\x01\0inhalt").unwrap();
        let public =
            aigw_core::wasm::public_key_pem(&aigw_core::wasm::public_key_of(&der).unwrap());
        let pub_file = dir.join("signing.pub");
        std::fs::write(&pub_file, &public).unwrap();
        let verify = |sig: &str, id: &str| {
            run(&WasmCommand::Verify(VerifyArgs {
                public_key: pub_file.display().to_string(),
                key_id: id.into(),
                signature: sig.into(),
                module: module.clone(),
            }))
        };
        assert!(verify(&sig, "release").is_ok(), "{sha}");
        let other = aigw_core::wasm::sign_sha256(&aigw_core::wasm::generate_pkcs8().unwrap(), &sha)
            .unwrap();
        assert!(verify(&other, "release").is_err());
        // Signatur-Ausgabe als Query ist prozentkodiert.
        assert_eq!(query("k", "a+b/c="), "keyId=k&signature=a%2Bb%2Fc%3D");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
