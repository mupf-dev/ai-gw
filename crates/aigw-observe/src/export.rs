// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Exporter für das Request-Log: Trait [`LogExporter`] und JSON-Lines-Datei.
//!
//! Exporter bekommen dieselben aufbereiteten Batches wie die Datenbank (Payload bereits
//! maskiert). OTLP-Logs (AIGW-45) und Webhook folgen als weitere Implementierungen.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io::{BufWriter, Write as _};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use aigw_store::observe::RequestLogRow;
use serde::Serialize;

use crate::process::LogEntry;

/// Fehler eines Exporters.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ExportError {
    /// Ein-/Ausgabe.
    #[error("E/A: {0}")]
    Io(#[from] std::io::Error),
    /// Serialisierung.
    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// Sonstiger Fehler (z. B. Gegenstelle).
    #[error("{0}")]
    Other(String),
}

/// Rückgabe von [`LogExporter::export`] (dyn-kompatibel ohne `async_trait`).
pub type ExportFuture<'a> = Pin<Box<dyn Future<Output = Result<(), ExportError>> + Send + 'a>>;

/// Ziel für Request-Log-Batches neben der Datenbank.
///
/// `export` wird vom Writer nacheinander je Batch aufgerufen; ein langsamer Exporter bremst
/// den Writer (der Puffer läuft dann voll und es wird gezählt verworfen), blockiert aber nie
/// den Proxy. Fehler werden gezählt und geloggt, der Batch gilt trotzdem als verarbeitet.
pub trait LogExporter: Send + Sync + fmt::Debug + 'static {
    /// Name für Logs/Metriken.
    fn name(&self) -> &str;
    /// Exportiert einen Batch.
    fn export<'a>(&'a self, batch: &'a [LogEntry]) -> ExportFuture<'a>;
    /// Schreibt Gepuffertes weg (beim Herunterfahren).
    fn flush(&self) -> ExportFuture<'_> {
        Box::pin(async { Ok(()) })
    }
}

/// Eine Zeile der JSON-Lines-Datei: Rohlog-Felder (camelCase) + optional `payload`.
#[derive(Serialize)]
struct Line<'a> {
    #[serde(flatten)]
    log: &'a RequestLogRow,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload: Option<&'a serde_json::Value>,
}

/// Hängt je Eintrag eine JSON-Zeile an eine Datei an (Verzeichnis muss existieren).
///
/// Serialisiert im Writer-Task, schreibt per `spawn_blocking`. Die Datei wird beim ersten
/// Batch geöffnet (anhängend) und nach jedem Batch geflusht.
#[derive(Debug, Clone)]
pub struct JsonLinesExporter {
    path: PathBuf,
    include_payload: bool,
    file: Arc<Mutex<Option<BufWriter<File>>>>,
}

impl JsonLinesExporter {
    /// Exporter für `path`.
    pub fn new(path: impl Into<PathBuf>, include_payload: bool) -> Self {
        Self {
            path: path.into(),
            include_payload,
            file: Arc::new(Mutex::new(None)),
        }
    }

    fn encode(&self, batch: &[LogEntry]) -> Result<Vec<u8>, ExportError> {
        let mut buf = Vec::with_capacity(batch.len() * 512);
        for e in batch {
            let line = Line {
                log: &e.log,
                payload: if self.include_payload {
                    e.payload.as_ref().map(|p| &p.payload)
                } else {
                    None
                },
            };
            serde_json::to_writer(&mut buf, &line)?;
            buf.push(b'\n');
        }
        Ok(buf)
    }

    fn write_blocking(&self, buf: &[u8]) -> Result<(), ExportError> {
        let mut guard = self
            .file
            .lock()
            .map_err(|_| ExportError::Other("Datei-Sperre vergiftet".into()))?;
        if guard.is_none() {
            let f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            *guard = Some(BufWriter::new(f));
        }
        if let Some(w) = guard.as_mut() {
            w.write_all(buf)?;
            w.flush()?;
        }
        Ok(())
    }
}

impl LogExporter for JsonLinesExporter {
    fn name(&self) -> &str {
        "jsonl"
    }

    fn export<'a>(&'a self, batch: &'a [LogEntry]) -> ExportFuture<'a> {
        Box::pin(async move {
            let buf = self.encode(batch)?;
            let this = self.clone();
            tokio::task::spawn_blocking(move || this.write_blocking(&buf))
                .await
                .map_err(|e| ExportError::Other(e.to_string()))?
        })
    }
}
