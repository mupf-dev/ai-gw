// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Exporter für das Request-Log: Trait [`LogExporter`] und JSON-Lines-Datei.
//!
//! Exporter bekommen dieselben aufbereiteten Batches wie die Datenbank (Payload bereits
//! maskiert). OTLP-Logs (AIGW-45) und Webhook folgen als weitere Implementierungen.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use aigw_core::config::JsonlRotateConfig;
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

/// Zeitquelle (austauschbar für Tests der Tagesrotation).
type Clock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

/// Offene Datei samt Größe und UTC-Tag (für die Rotation).
#[derive(Debug, Default)]
struct OpenFile {
    file: Option<BufWriter<File>>,
    size: u64,
    day: u64,
}

/// Hängt je Eintrag eine JSON-Zeile an eine Datei an (Verzeichnis muss existieren).
///
/// Serialisiert im Writer-Task, schreibt per `spawn_blocking`. Die Datei wird beim ersten
/// Batch geöffnet (anhängend) und nach jedem Batch geflusht.
///
/// **Rotation (F31, `observe.jsonl.rotate`):** beim Tageswechsel (UTC) und/oder bevor die
/// Datei `max_size_mb` überschreiten würde, wird sie in `<pfad>.<UTC-Zeitstempel>` umbenannt
/// (mit `gzip` komprimiert zu `….gz`), danach eine neue Datei begonnen; von den rotierten
/// Dateien bleiben die `keep` jüngsten. [`JsonLinesExporter::reopen`] (`SIGHUP`) schließt die
/// Datei und öffnet beim nächsten Batch neu – für externes logrotate.
#[derive(Clone)]
pub struct JsonLinesExporter {
    path: PathBuf,
    include_payload: bool,
    rotate: JsonlRotateConfig,
    file: Arc<Mutex<OpenFile>>,
    reopen: Arc<AtomicBool>,
    clock: Clock,
}

impl fmt::Debug for JsonLinesExporter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JsonLinesExporter")
            .field("path", &self.path)
            .field("include_payload", &self.include_payload)
            .field("rotate", &self.rotate)
            .finish_non_exhaustive()
    }
}

const DAY_SECS: u64 = 86_400;

fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// `20261113T120000123Z` (UTC, Millisekunden) – sortiert lexikalisch wie zeitlich.
fn stamp(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = i64::try_from(d.as_secs()).unwrap_or(0);
    let dt =
        time::OffsetDateTime::from_unix_timestamp(secs).unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}{:03}Z",
        dt.year(),
        u8::from(dt.month()),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
        d.subsec_millis()
    )
}

impl JsonLinesExporter {
    /// Exporter für `path` (ohne Rotation).
    pub fn new(path: impl Into<PathBuf>, include_payload: bool) -> Self {
        Self {
            path: path.into(),
            include_payload,
            rotate: JsonlRotateConfig::default(),
            file: Arc::new(Mutex::new(OpenFile::default())),
            reopen: Arc::new(AtomicBool::new(false)),
            clock: Arc::new(SystemTime::now),
        }
    }

    /// Rotation nach `observe.jsonl.rotate` (F31).
    #[must_use]
    pub fn with_rotation(mut self, rotate: JsonlRotateConfig) -> Self {
        self.rotate = rotate;
        self
    }

    /// Eigene Zeitquelle (Tests der Tagesrotation).
    #[must_use]
    pub fn with_clock(mut self, clock: impl Fn() -> SystemTime + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    /// Schließt die Datei; der nächste Batch öffnet `path` neu (`SIGHUP`, externes logrotate).
    pub fn reopen(&self) {
        self.reopen.store(true, Ordering::Release);
    }

    /// Rotierte Dateien (älteste zuerst): `<name>.<Zeitstempel>[.gz]` im selben Verzeichnis.
    pub fn rotated_files(&self) -> std::io::Result<Vec<PathBuf>> {
        let dir = match self.path.parent() {
            Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
            _ => PathBuf::from("."),
        };
        let Some(name) = self.path.file_name().and_then(|n| n.to_str()) else {
            return Ok(Vec::new());
        };
        let prefix = format!("{name}.");
        let mut out: Vec<PathBuf> = std::fs::read_dir(&dir)?
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name().to_str().is_some_and(|n| {
                    n.strip_prefix(&prefix)
                        .is_some_and(|rest| rest.as_bytes().first().is_some_and(u8::is_ascii_digit))
                })
            })
            .map(|e| e.path())
            .collect();
        out.sort();
        Ok(out)
    }

    /// Benennt die aktuelle Datei um (ggf. gzip), räumt alte auf. Datei muss geschlossen sein.
    fn rotate_file(&self, now: SystemTime) -> Result<(), ExportError> {
        if !self.path.exists() {
            return Ok(());
        }
        let base = format!("{}.{}", self.path.display(), stamp(now));
        let mut target = PathBuf::from(&base);
        let mut n = 1;
        while target.exists() || Path::new(&format!("{}.gz", target.display())).exists() {
            target = PathBuf::from(format!("{base}-{n}"));
            n += 1;
        }
        std::fs::rename(&self.path, &target)?;
        if self.rotate.gzip {
            let gz = PathBuf::from(format!("{}.gz", target.display()));
            let mut input = File::open(&target)?;
            let out = File::create(&gz)?;
            let mut enc = flate2::write::GzEncoder::new(out, flate2::Compression::default());
            std::io::copy(&mut input, &mut enc)?;
            enc.finish()?.sync_all()?;
            std::fs::remove_file(&target)?;
        }
        if self.rotate.keep > 0 {
            let files = self.rotated_files()?;
            let excess = files.len().saturating_sub(self.rotate.keep);
            for f in files.into_iter().take(excess) {
                if let Err(e) = std::fs::remove_file(&f) {
                    tracing::warn!(file = %f.display(), error = %e, "JSON-Lines: alte Datei nicht löschbar");
                }
            }
        }
        tracing::info!(file = %target.display(), gzip = self.rotate.gzip, "JSON-Lines-Datei rotiert");
        Ok(())
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
        let mut st = self
            .file
            .lock()
            .map_err(|_| ExportError::Other("Datei-Sperre vergiftet".into()))?;
        let now = (self.clock)();
        let today = unix_secs(now) / DAY_SECS;
        if self.reopen.swap(false, Ordering::AcqRel)
            && let Some(mut w) = st.file.take()
        {
            w.flush()?;
        }
        if st.file.is_none() {
            // Bestehende Datei: Größe und Tag der letzten Änderung (Tagesrotation nach Neustart).
            let (size, day) = match std::fs::metadata(&self.path) {
                Ok(m) => (
                    m.len(),
                    m.modified().map_or(today, |t| unix_secs(t) / DAY_SECS),
                ),
                Err(_) => (0, today),
            };
            st.size = size;
            st.day = day;
            st.file = Some(BufWriter::new(self.open()?));
        }
        let r = &self.rotate;
        let max = r.max_size_mb.saturating_mul(1024 * 1024);
        let new_day = r.daily && st.day != today;
        let too_big = max > 0 && st.size > 0 && st.size + buf.len() as u64 > max;
        if (new_day || too_big) && st.size > 0 {
            if let Some(mut w) = st.file.take() {
                w.flush()?;
            }
            self.rotate_file(now)?;
            st.file = Some(BufWriter::new(self.open()?));
            st.size = 0;
        }
        st.day = today;
        if let Some(w) = st.file.as_mut() {
            w.write_all(buf)?;
            w.flush()?;
        }
        st.size += buf.len() as u64;
        Ok(())
    }

    fn open(&self) -> std::io::Result<File> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::io::Read as _;
    use std::sync::atomic::AtomicU64;
    use std::time::Duration;

    use super::*;
    use crate::process::LogEntry;

    fn dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("aigw-jsonl-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn entry(id: &str) -> LogEntry {
        LogEntry {
            log: RequestLogRow {
                id: id.into(),
                ..RequestLogRow::default()
            },
            payload: None,
        }
    }

    fn lines(p: &Path) -> usize {
        std::fs::read_to_string(p)
            .unwrap_or_default()
            .lines()
            .count()
    }

    #[test]
    fn rotates_by_size_keeps_n_and_gzips() {
        let d = dir();
        let path = d.join("req.jsonl");
        let e = JsonLinesExporter::new(&path, false).with_rotation(JsonlRotateConfig {
            daily: false,
            max_size_mb: 1,
            keep: 2,
            gzip: true,
        });
        // einige 100 KiB je Batch → mehrere Rotationen
        let batch: Vec<LogEntry> = (0..500).map(|i| entry(&format!("{i:0>90}"))).collect();
        let buf = e.encode(&batch).unwrap();
        for _ in 0..16 {
            e.write_blocking(&buf).unwrap();
        }
        let rotated = e.rotated_files().unwrap();
        assert_eq!(rotated.len(), 2, "keep = 2: {rotated:?}");
        for f in &rotated {
            assert!(f.to_string_lossy().ends_with(".gz"), "{f:?}");
            let mut s = String::new();
            flate2::read::GzDecoder::new(File::open(f).unwrap())
                .read_to_string(&mut s)
                .unwrap();
            assert!(s.lines().count() >= 500 && s.len() as u64 <= 1024 * 1024);
        }
        assert!(std::fs::metadata(&path).unwrap().len() <= 1024 * 1024);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn rotates_daily_and_reopens() {
        let d = dir();
        let path = d.join("req.jsonl");
        let now = Arc::new(AtomicU64::new(10 * DAY_SECS + 3_600));
        let n = Arc::clone(&now);
        let e = JsonLinesExporter::new(&path, false)
            .with_rotation(JsonlRotateConfig {
                daily: true,
                ..JsonlRotateConfig::default()
            })
            .with_clock(move || UNIX_EPOCH + Duration::from_secs(n.load(Ordering::SeqCst)));
        let buf = e.encode(&[entry("a")]).unwrap();
        e.write_blocking(&buf).unwrap();
        e.write_blocking(&buf).unwrap();
        assert!(e.rotated_files().unwrap().is_empty());
        now.store(11 * DAY_SECS + 5, Ordering::SeqCst); // nächster UTC-Tag
        e.write_blocking(&buf).unwrap();
        let rotated = e.rotated_files().unwrap();
        assert_eq!(rotated.len(), 1);
        assert!(
            rotated[0]
                .to_string_lossy()
                .ends_with("req.jsonl.19700112T000005000Z")
        );
        assert_eq!(lines(&rotated[0]), 2);
        assert_eq!(lines(&path), 1);

        // SIGHUP: extern umbenannt → nach reopen() entsteht die Datei neu
        let moved = d.join("extern.jsonl");
        std::fs::rename(&path, &moved).unwrap();
        e.write_blocking(&buf).unwrap(); // schreibt noch in die umbenannte Datei
        assert_eq!(lines(&moved), 2);
        e.reopen();
        e.write_blocking(&buf).unwrap();
        assert_eq!(lines(&path), 1);
        assert_eq!(lines(&moved), 2);
        std::fs::remove_dir_all(&d).unwrap();
    }
}
