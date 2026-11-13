// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! JSON-RPC 2.0 (Grundlage von MCP): Nachrichten, IDs, Fehler.
//!
//! Eigene, schlanke Umsetzung: Nachrichten bleiben `serde_json::Value`-nah, damit unbekannte
//! Felder (z. B. `_meta`, neue Protokollfelder) unverändert durchgereicht werden.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// `-32700`: ungültiges JSON.
pub const PARSE_ERROR: i64 = -32700;
/// `-32600`: keine gültige JSON-RPC-Nachricht.
pub const INVALID_REQUEST: i64 = -32600;
/// `-32601`: Methode unbekannt bzw. nicht unterstützt.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// `-32602`: ungültige Parameter (MCP: auch unbekanntes Tool).
pub const INVALID_PARAMS: i64 = -32602;
/// `-32603`: interner Fehler.
pub const INTERNAL_ERROR: i64 = -32603;
/// `-32002`: Ressource nicht gefunden (MCP).
pub const RESOURCE_NOT_FOUND: i64 = -32002;
/// `-32003`: nicht freigegeben (Gateway).
pub const FORBIDDEN: i64 = -32003;
/// `-32004`: Limit oder Budget überschritten (Gateway).
pub const RATE_LIMITED: i64 = -32004;
/// `-32005`: Upstream nicht erreichbar oder fehlerhaft (Gateway).
pub const UPSTREAM_ERROR: i64 = -32005;
/// `-32006`: Zeitlimit des Upstreams überschritten (Gateway).
pub const UPSTREAM_TIMEOUT: i64 = -32006;

/// Request-ID (Zahl oder Text).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    /// Ganzzahl.
    Num(i64),
    /// Text.
    Str(String),
}

impl RequestId {
    /// Aus einem JSON-Wert (`None` bei `null` oder anderen Typen).
    pub fn from_value(v: &Value) -> Option<Self> {
        match v {
            Value::Number(n) => n.as_i64().map(Self::Num),
            Value::String(s) => Some(Self::Str(s.clone())),
            _ => None,
        }
    }

    /// Als JSON-Wert.
    pub fn to_value(&self) -> Value {
        match self {
            Self::Num(n) => Value::from(*n),
            Self::Str(s) => Value::String(s.clone()),
        }
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Num(n) => write!(f, "{n}"),
            Self::Str(s) => write!(f, "{s:?}"),
        }
    }
}

/// JSON-RPC-Fehlerobjekt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    /// Fehlercode.
    pub code: i64,
    /// Meldung.
    pub message: String,
    /// Zusatzdaten.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    /// Fehler ohne Zusatzdaten.
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// Fehler des Gateways mit HTTP-Statusäquivalent und Kennung in `data`
    /// (`{"status": 403, "code": "tool_not_allowed"}`).
    pub fn gateway(code: i64, status: u16, kind: &str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: Some(json!({"status": status, "code": kind})),
        }
    }

    /// `-32601`.
    pub fn method_not_found(method: &str) -> Self {
        Self::new(
            METHOD_NOT_FOUND,
            format!("Methode {method:?} wird nicht unterstützt"),
        )
    }

    /// `-32602`.
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, message)
    }

    /// HTTP-Statusäquivalent aus `data.status` (falls gesetzt).
    pub fn status(&self) -> Option<u16> {
        self.data
            .as_ref()?
            .get("status")?
            .as_u64()
            .and_then(|s| u16::try_from(s).ok())
    }

    /// Kennung aus `data.code` (falls gesetzt).
    pub fn kind(&self) -> Option<&str> {
        self.data.as_ref()?.get("code")?.as_str()
    }
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for RpcError {}

/// Eine JSON-RPC-Nachricht.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    /// Anfrage (mit ID, erwartet eine Antwort).
    Request {
        /// ID.
        id: RequestId,
        /// Methode.
        method: String,
        /// Parameter.
        params: Option<Value>,
    },
    /// Benachrichtigung (ohne ID).
    Notification {
        /// Methode.
        method: String,
        /// Parameter.
        params: Option<Value>,
    },
    /// Antwort (`id` fehlt nur bei Fehlern vor dem Lesen der ID).
    Response {
        /// ID der Anfrage.
        id: Option<RequestId>,
        /// Ergebnis oder Fehler.
        result: Result<Value, RpcError>,
    },
}

impl Message {
    /// Liest eine Nachricht aus Bytes (eine Nachricht, kein Batch).
    pub fn parse(bytes: &[u8]) -> Result<Self, RpcError> {
        let v: Value = serde_json::from_slice(bytes)
            .map_err(|e| RpcError::new(PARSE_ERROR, format!("Ungültiges JSON: {e}")))?;
        Self::from_value(v)
    }

    /// Liest eine Nachricht aus einem JSON-Wert.
    pub fn from_value(v: Value) -> Result<Self, RpcError> {
        let Value::Object(mut m) = v else {
            return Err(RpcError::new(
                INVALID_REQUEST,
                "JSON-RPC-Nachricht muss ein Objekt sein (Batches werden nicht unterstützt)",
            ));
        };
        if m.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Err(RpcError::new(INVALID_REQUEST, "jsonrpc muss \"2.0\" sein"));
        }
        let params = m.remove("params");
        if params
            .as_ref()
            .is_some_and(|p| !p.is_object() && !p.is_array())
        {
            return Err(RpcError::new(
                INVALID_REQUEST,
                "params muss ein Objekt oder Array sein",
            ));
        }
        let id = m.get("id").map(RequestId::from_value);
        match m.remove("method") {
            Some(Value::String(method)) => match id {
                None => Ok(Self::Notification { method, params }),
                Some(Some(id)) => Ok(Self::Request { id, method, params }),
                Some(None) => Err(RpcError::new(
                    INVALID_REQUEST,
                    "id muss eine Zahl oder Zeichenkette sein",
                )),
            },
            Some(_) => Err(RpcError::new(INVALID_REQUEST, "method muss Text sein")),
            None => {
                let id = id.flatten();
                if let Some(e) = m.remove("error") {
                    let e: RpcError = serde_json::from_value(e).map_err(|e| {
                        RpcError::new(INVALID_REQUEST, format!("Ungültiges Fehlerobjekt: {e}"))
                    })?;
                    Ok(Self::Response { id, result: Err(e) })
                } else if let Some(r) = m.remove("result") {
                    Ok(Self::Response { id, result: Ok(r) })
                } else {
                    Err(RpcError::new(
                        INVALID_REQUEST,
                        "weder method noch result/error",
                    ))
                }
            }
        }
    }

    /// Als JSON-Wert.
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("jsonrpc".into(), Value::from("2.0"));
        match self {
            Self::Request { id, method, params } => {
                m.insert("id".into(), id.to_value());
                m.insert("method".into(), Value::String(method.clone()));
                if let Some(p) = params {
                    m.insert("params".into(), p.clone());
                }
            }
            Self::Notification { method, params } => {
                m.insert("method".into(), Value::String(method.clone()));
                if let Some(p) = params {
                    m.insert("params".into(), p.clone());
                }
            }
            Self::Response { id, result } => {
                m.insert(
                    "id".into(),
                    id.as_ref().map_or(Value::Null, RequestId::to_value),
                );
                match result {
                    Ok(r) => m.insert("result".into(), r.clone()),
                    Err(e) => m.insert(
                        "error".into(),
                        serde_json::to_value(e).unwrap_or(Value::Null),
                    ),
                };
            }
        }
        Value::Object(m)
    }

    /// Serialisiert als JSON-Bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&self.to_value()).unwrap_or_default()
    }

    /// Antwort mit Ergebnis.
    pub fn result(id: RequestId, result: Value) -> Self {
        Self::Response {
            id: Some(id),
            result: Ok(result),
        }
    }

    /// Antwort mit Fehler.
    pub fn error(id: Option<RequestId>, error: RpcError) -> Self {
        Self::Response {
            id,
            result: Err(error),
        }
    }

    /// Benachrichtigung.
    pub fn notification(method: impl Into<String>, params: Option<Value>) -> Self {
        Self::Notification {
            method: method.into(),
            params,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_kinds() {
        let r = Message::parse(br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#).unwrap();
        assert_eq!(
            r,
            Message::Request {
                id: RequestId::Num(1),
                method: "ping".into(),
                params: None
            }
        );
        let n =
            Message::parse(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).unwrap();
        assert!(matches!(n, Message::Notification { .. }));
        let ok = Message::parse(br#"{"jsonrpc":"2.0","id":"a","result":{}}"#).unwrap();
        assert!(matches!(ok, Message::Response { result: Ok(_), .. }));
        let err =
            Message::parse(br#"{"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"x"}}"#)
                .unwrap();
        assert!(matches!(err, Message::Response { result: Err(ref e), .. } if e.code == -32601));
        for bad in [
            &br#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#[..],
            br#"{"id":1,"method":"ping"}"#,
            br#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#,
            br#"{"jsonrpc":"2.0","id":1,"method":"x","params":3}"#,
        ] {
            assert_eq!(
                Message::parse(bad).unwrap_err().code,
                INVALID_REQUEST,
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
        assert_eq!(Message::parse(b"{").unwrap_err().code, PARSE_ERROR);
    }

    #[test]
    fn roundtrip_keeps_unknown_params() {
        let raw = json!({"jsonrpc":"2.0","id":"x","method":"tools/call","params":{"name":"a","_meta":{"progressToken":5}}});
        let m = Message::from_value(raw.clone()).unwrap();
        assert_eq!(m.to_value(), raw);
        let e = RpcError::gateway(FORBIDDEN, 403, "tool_not_allowed", "nein");
        assert_eq!(e.status(), Some(403));
        assert_eq!(e.kind(), Some("tool_not_allowed"));
    }
}
