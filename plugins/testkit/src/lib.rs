// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Test-Plugin `testkit` für die Tests und Messungen von ai-gw (AIGW-63). Verhalten über
//! `config.mode`:
//!
//! - `pass` – nichts tun (Leer-Plugin für die Latenzmessung)
//! - `spin` – Endlosschleife (Fuel- bzw. Zeitgrenze)
//! - `alloc` – `mb` MiB anfordern und beschreiben (Speichergrenze)
//! - `trap` – Panic (Trap; schreibt vorher auf stderr)
//! - `error` – Fehler zurückgeben
//! - `bad-output` – ungültiges JSON als Änderung
//! - `meta` – Meta-Eingabe als Antwort-Header `x-testkit-meta` spiegeln, `{"seen": true}` setzen
//! - `respond` – Anfrage-Phase: feste Antwort (IR-JSON: `content` als `{"text": …}`)
//! - `drop` – Stream-Ereignisse verwerfen
//! - `describe-error` – schon `describe` lehnt die Konfiguration ab

use serde_json::Value;

wit_bindgen::generate!({
    path: "../../crates/aigw-wasm/wit",
    world: "plugin",
});

use aigw::plugin::types::{Action, Header};

struct Testkit;

fn mode(config: &str) -> (String, u64) {
    let v: Value = serde_json::from_str(config).unwrap_or(Value::Null);
    (
        v.get("mode")
            .and_then(Value::as_str)
            .unwrap_or("pass")
            .to_owned(),
        v.get("mb").and_then(Value::as_u64).unwrap_or(64),
    )
}

fn out(action: Action) -> Output {
    Output {
        action,
        meta: None,
        upstream_headers: Vec::new(),
        response_headers: Vec::new(),
    }
}

impl Guest for Testkit {
    fn describe(config: String) -> Result<Phases, String> {
        match mode(&config).0.as_str() {
            "describe-error" => Err("Konfiguration ungültig (Test)".into()),
            _ => Ok(Phases::REQUEST | Phases::RESPONSE | Phases::CHUNK),
        }
    }

    fn handle(input: Input) -> Result<Output, String> {
        let (mode, mb) = mode(&input.config);
        match mode.as_str() {
            "spin" => {
                let mut x: u64 = 0;
                loop {
                    x = std::hint::black_box(x.wrapping_add(1));
                }
            }
            "alloc" => {
                let mut v: Vec<u8> = Vec::new();
                v.resize((mb as usize) * 1024 * 1024, 1);
                Ok(out(if std::hint::black_box(v[v.len() / 2]) == 1 {
                    Action::Pass
                } else {
                    Action::Drop
                }))
            }
            "trap" => panic!("absichtlicher Abbruch (Test)"),
            "error" => Err("kaputt (Test)".into()),
            "bad-output" => Ok(out(Action::Modify("kein json".into()))),
            "meta" => Ok(Output {
                action: Action::Pass,
                meta: Some(r#"{"seen":true,"aigw.secret":"x"}"#.into()),
                upstream_headers: Vec::new(),
                response_headers: vec![Header {
                    name: "x-testkit-meta".into(),
                    value: input.meta.clone(),
                }],
            }),
            "respond" => Ok(out(Action::Respond(
                r#"{"id":"","model":"","choices":[{"index":0,"message":{"role":"assistant","content":{"text":"aus wasm"}}}]}"#.into(),
            ))),
            "drop" => Ok(out(Action::Drop)),
            _ => Ok(out(Action::Pass)),
        }
    }
}

export!(Testkit);
