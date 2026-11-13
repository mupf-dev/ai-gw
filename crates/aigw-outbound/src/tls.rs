// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! TLS-Vertrauensanker (`upstream.roots`, `upstream.ca_file`, F48).
//!
//! Die rustls-Konfiguration wird je Kombination aus Wurzeln und CA-Dateien einmal gebaut und
//! zwischengespeichert (das Laden der System-CAs liest viele Dateien). Ändert sich eine
//! CA-Datei (Größe/Änderungszeit), wird beim nächsten Abruf neu gebaut.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::UNIX_EPOCH;

use aigw_core::config::{RootsKind, UpstreamConfig};
use rustls::RootCertStore;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject as _;

use crate::error::OutboundError;

/// Vertrauensanker einer TLS-Verbindung.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct TlsOptions {
    /// Wurzelzertifikate.
    pub roots: RootsKind,
    /// Zusätzliche CA-Dateien (PEM), in dieser Reihenfolge.
    pub ca_files: Vec<PathBuf>,
}

/// Zustand der CA-Dateien (Größe, Änderungszeit) – erkennt ausgetauschte Dateien.
pub(crate) type Fingerprint = Vec<(u64, u128)>;

type Cache = Mutex<HashMap<TlsOptions, (Fingerprint, Arc<rustls::ClientConfig>)>>;

fn cache() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(Cache::default)
}

static DEFAULTS: OnceLock<TlsOptions> = OnceLock::new();

/// Setzt die prozessweiten Standard-Vertrauensanker (`upstream.*` der Startkonfiguration) und
/// prüft sie dabei (CA-Datei lesbar, mindestens ein Wurzelzertifikat). Gilt für alle danach
/// gebauten Clients ohne eigene [`TlsOptions`]. Ein zweiter Aufruf mit anderen Werten ist ein
/// Fehler.
pub fn install_default_tls(opts: TlsOptions) -> Result<(), OutboundError> {
    opts.client_config()?;
    if DEFAULTS.get_or_init(|| opts.clone()) == &opts {
        Ok(())
    } else {
        Err(OutboundError::Tls(
            "Standard-Vertrauensanker sind bereits gesetzt".into(),
        ))
    }
}

/// Prozessweite Standard-Vertrauensanker (ohne [`install_default_tls`]: `webpki`).
pub fn default_tls() -> TlsOptions {
    DEFAULTS.get().cloned().unwrap_or_default()
}

impl TlsOptions {
    /// Aus dem Abschnitt `upstream` der Startkonfiguration.
    pub fn from_config(cfg: &UpstreamConfig) -> Self {
        Self {
            roots: cfg.roots,
            ca_files: cfg.ca_file.iter().cloned().collect(),
        }
    }

    /// Ergänzt eine CA-Datei (doppelte werden ignoriert).
    #[must_use]
    pub fn with_ca_file(mut self, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        if !self.ca_files.contains(&path) {
            self.ca_files.push(path);
        }
        self
    }

    /// rustls-Konfiguration (zwischengespeichert, ohne ALPN – das setzt der Connector).
    pub fn client_config(&self) -> Result<Arc<rustls::ClientConfig>, OutboundError> {
        self.resolve().map(|(_, cfg)| cfg)
    }

    pub(crate) fn resolve(
        &self,
    ) -> Result<(Fingerprint, Arc<rustls::ClientConfig>), OutboundError> {
        let fp = self.fingerprint()?;
        {
            let c = cache().lock().unwrap_or_else(PoisonError::into_inner);
            if let Some((f, cfg)) = c.get(self)
                && *f == fp
            {
                return Ok((fp, Arc::clone(cfg)));
            }
        }
        let cfg = Arc::new(self.build()?);
        cache()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(self.clone(), (fp.clone(), Arc::clone(&cfg)));
        Ok((fp, cfg))
    }

    pub(crate) fn fingerprint(&self) -> Result<Fingerprint, OutboundError> {
        self.ca_files
            .iter()
            .map(|p| {
                let m = std::fs::metadata(p)
                    .map_err(|e| OutboundError::Tls(format!("CA-Datei {}: {e}", p.display())))?;
                let mtime = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_nanos());
                Ok((m.len(), mtime))
            })
            .collect()
    }

    fn build(&self) -> Result<rustls::ClientConfig, OutboundError> {
        let mut roots = RootCertStore::empty();
        if matches!(self.roots, RootsKind::Webpki | RootsKind::Both) {
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }
        if matches!(self.roots, RootsKind::System | RootsKind::Both) {
            let native = rustls_native_certs::load_native_certs();
            for e in &native.errors {
                tracing::debug!(error = %e, "System-CA nicht lesbar");
            }
            let (added, ignored) = roots.add_parsable_certificates(native.certs);
            tracing::debug!(added, ignored, "System-CAs geladen");
        }
        for path in &self.ca_files {
            add_ca_file(&mut roots, path)?;
        }
        if roots.is_empty() {
            return Err(OutboundError::Tls(format!(
                "keine Wurzelzertifikate (upstream.roots: {:?}, keine CA-Datei) – \
                 System-Zertifikatsspeicher leer?",
                self.roots
            )));
        }
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        Ok(rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| OutboundError::Tls(e.to_string()))?
            .with_root_certificates(roots)
            .with_no_client_auth())
    }
}

fn add_ca_file(roots: &mut RootCertStore, path: &Path) -> Result<(), OutboundError> {
    let err =
        |e: &dyn std::fmt::Display| OutboundError::Tls(format!("CA-Datei {}: {e}", path.display()));
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(path)
        .map_err(|e| err(&e))?
        .collect::<Result<_, _>>()
        .map_err(|e| err(&e))?;
    if certs.is_empty() {
        return Err(err(&"keine Zertifikate (PEM erwartet)"));
    }
    for c in certs {
        roots.add(c).map_err(|e| err(&e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn webpki_default_and_missing_ca_file() {
        let o = TlsOptions::default();
        assert_eq!(o.roots, RootsKind::Webpki);
        let a = o.client_config().unwrap();
        let b = o.client_config().unwrap();
        assert!(Arc::ptr_eq(&a, &b), "zwischengespeichert");
        assert!(a.alpn_protocols.is_empty());
        let bad = TlsOptions::default().with_ca_file("/nonexistent/ca.pem");
        assert!(matches!(bad.client_config(), Err(OutboundError::Tls(_))));
    }

    #[test]
    fn ca_file_without_certificates_is_rejected() {
        let dir = std::env::temp_dir().join(format!("aigw-outbound-tls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("empty.pem");
        std::fs::write(&p, "kein Zertifikat\n").unwrap();
        let o = TlsOptions::default().with_ca_file(&p).with_ca_file(&p);
        assert_eq!(o.ca_files.len(), 1);
        let e = o.client_config().unwrap_err().to_string();
        assert!(e.contains("keine Zertifikate"), "{e}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
