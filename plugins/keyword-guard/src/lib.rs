// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: Apache-2.0

//! Beispiel-Plugin `keyword-guard` für ai-gw (AIGW-63): Stichwort-Blocker und Header-Setzer.
//!
//! Konfiguration (`config` der Bindung):
//!
//! ```json
//! {
//!   "block": ["geheimprojekt"],          // Anfrage/Antwort mit diesem Wort blockieren
//!   "mask": ["intern"],                  // Wort durch "***" ersetzen (Anfrage und Antwort)
//!   "maskWith": "***",
//!   "output": true,                      // auch Antworten/Streams prüfen
//!   "upstreamHeaders": {"x-guard": "on"},  // Header an den Upstream
//!   "responseHeaders": {"x-guard": "checked"} // Header an den Client
//! }
//! ```
//!
//! Wörter: Buchstaben, Ziffern, Leerzeichen, `-` und `_` (damit das Ersetzen im JSON-Text
//! gültiges JSON lässt). Groß-/Kleinschreibung wird beim Blockieren ignoriert, beim Maskieren
//! nicht. Im Stream wird je Ereignis geprüft (über Ereignisgrenzen geteilte Wörter erkennt das
//! Beispiel nicht).

use serde_json::{Map, Value, json};

wit_bindgen::generate!({
    path: "../../crates/aigw-wasm/wit",
    world: "plugin",
});

use aigw::plugin::host::log;
use aigw::plugin::types::{Action, Header, Phase, Rejection};

struct Guard;

struct Config {
    block: Vec<String>,
    mask: Vec<String>,
    mask_with: String,
    output: bool,
    upstream: Vec<Header>,
    response: Vec<Header>,
}

fn words(v: Option<&Value>, what: &str) -> Result<Vec<String>, String> {
    let Some(v) = v else { return Ok(Vec::new()) };
    let arr = v.as_array().ok_or(format!("{what}: Liste erwartet"))?;
    arr.iter()
        .map(|w| {
            let w = w.as_str().ok_or(format!("{what}: Text erwartet"))?;
            let ok = !w.is_empty()
                && w.chars()
                    .all(|c| c.is_alphanumeric() || c == ' ' || c == '-' || c == '_');
            if ok {
                Ok(w.to_owned())
            } else {
                Err(format!("{what}: ungültiges Wort {w:?}"))
            }
        })
        .collect()
}

fn headers(v: Option<&Value>, what: &str) -> Result<Vec<Header>, String> {
    let Some(v) = v else { return Ok(Vec::new()) };
    let obj: &Map<String, Value> = v.as_object().ok_or(format!("{what}: Objekt erwartet"))?;
    obj.iter()
        .map(|(k, v)| {
            Ok(Header {
                name: k.to_ascii_lowercase(),
                value: v
                    .as_str()
                    .ok_or(format!("{what}.{k}: Text erwartet"))?
                    .to_owned(),
            })
        })
        .collect()
}

fn parse(config: &str) -> Result<Config, String> {
    let v: Value = serde_json::from_str(config).map_err(|e| e.to_string())?;
    let o = v.as_object().ok_or("Objekt erwartet")?;
    for k in o.keys() {
        if !matches!(
            k.as_str(),
            "block" | "mask" | "maskWith" | "output" | "upstreamHeaders" | "responseHeaders"
        ) {
            return Err(format!("unbekanntes Feld {k:?}"));
        }
    }
    let mask_with = o
        .get("maskWith")
        .and_then(Value::as_str)
        .unwrap_or("***")
        .to_owned();
    if mask_with.contains(['"', '\\']) {
        return Err("maskWith: keine Anführungszeichen oder Backslashes".into());
    }
    Ok(Config {
        block: words(o.get("block"), "block")?
            .into_iter()
            .map(|w| w.to_lowercase())
            .collect(),
        mask: words(o.get("mask"), "mask")?,
        mask_with,
        output: o.get("output").and_then(Value::as_bool).unwrap_or(false),
        upstream: headers(o.get("upstreamHeaders"), "upstreamHeaders")?,
        response: headers(o.get("responseHeaders"), "responseHeaders")?,
    })
}

impl Guest for Guard {
    fn describe(config: String) -> Result<Phases, String> {
        let c = parse(&config)?;
        Ok(if c.output {
            Phases::REQUEST | Phases::RESPONSE | Phases::CHUNK
        } else {
            Phases::REQUEST
        })
    }

    fn handle(input: Input) -> Result<Output, String> {
        let c = parse(&input.config)?;
        let lower = input.payload.to_lowercase();
        if let Some(w) = c.block.iter().find(|w| lower.contains(w.as_str())) {
            log(
                1,
                &format!("keyword-guard: gesperrtes Wort in {:?}", input.phase),
            );
            return Ok(Output {
                action: Action::Block(Rejection {
                    status: 400,
                    code: Some("keyword_blocked".into()),
                    message: format!(
                        "Inhalt enthält ein gesperrtes Stichwort ({} Zeichen)",
                        w.len()
                    ),
                }),
                meta: None,
                upstream_headers: Vec::new(),
                response_headers: Vec::new(),
            });
        }
        let mut payload = input.payload.clone();
        let mut masked = 0usize;
        for w in &c.mask {
            let n = payload.matches(w.as_str()).count();
            if n > 0 {
                masked += n;
                payload = payload.replace(w.as_str(), &c.mask_with);
            }
        }
        let mut meta: Map<String, Value> = serde_json::from_str(&input.meta).unwrap_or_default();
        let meta = if masked > 0 {
            let prev = meta
                .get("keywordGuardMasked")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            meta.insert("keywordGuardMasked".into(), json!(prev + masked as u64));
            Some(Value::Object(meta).to_string())
        } else {
            None
        };
        let (upstream_headers, response_headers) = match input.phase {
            Phase::Request => (c.upstream, c.response),
            Phase::Response => (Vec::new(), c.response),
            Phase::Chunk => (Vec::new(), Vec::new()),
        };
        Ok(Output {
            action: if masked > 0 {
                Action::Modify(payload)
            } else {
                Action::Pass
            },
            meta,
            upstream_headers,
            response_headers,
        })
    }
}

export!(Guard);
