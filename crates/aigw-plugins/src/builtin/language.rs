// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Plugin `language` (AIGW-114): erlaubte bzw. gesperrte Sprachen in Anfragen (optional
//! Antworten). Erkennung mit `whatlang` (Trigramme + Schrift, 69 Sprachen, MIT).
//!
//! ```yaml
//! plugin: language
//! config:
//!   allow: [de, en]        # nur diese Sprachen (ISO 639-1) – oder/und
//!   deny: [ru]             # diese nicht
//!   action: reject         # reject (Standard; Eingabe 400 language_not_allowed) | flag
//!   direction: input       # input (Standard) | output | both
//!   roles: [user]          # geprüfte Rollen (Standard: user)
//!   minChars: 20           # kürzere Texte nicht prüfen
//!   minConfidence: 0.5     # unsicherere Erkennungen nicht werten (0–1)
//!   streamSampleChars: 200 # Ausgabe im Stream: so viel Text zurückhalten und dann entscheiden
//! ```
//!
//! Geprüft wird je Nachricht (Text der Teile und Tool-Ergebnisse, Stichprobe der ersten
//! 2000 Zeichen). Kurze oder unsicher erkannte Texte gelten als erlaubt. Bei gemischten Texten
//! zählt die überwiegende Sprache.

use std::sync::Arc;

use aigw_core::ir::{ApiError, Content, PartKind, StreamEvent};
use serde::Deserialize;
use serde_json::Value;

use super::guard::{Direction, RoleSel, floor_boundary};
use crate::binding::FailMode;
use crate::plugin::{
    CallInfo, ChunkCtx, ChunkFlow, Flow, Hook, Phases, Plugin, PluginError, ReqCtx, RespCtx,
};
use crate::registry::PluginFactory;

const NAME: &str = "language";
/// Stichprobe je Text (Bytes; die Erkennung wird darüber kaum genauer).
const SAMPLE_BYTES: usize = 8000;
const SAMPLE_CHARS: usize = 2000;

/// ISO 639-1 → ISO 639-3 (Codes von `whatlang`). `no` ist ein Alias für Bokmål.
const ISO1: &[(&str, &str)] = &[
    ("af", "afr"),
    ("ak", "aka"),
    ("am", "amh"),
    ("ar", "ara"),
    ("az", "aze"),
    ("be", "bel"),
    ("bg", "bul"),
    ("bn", "ben"),
    ("ca", "cat"),
    ("cs", "ces"),
    ("da", "dan"),
    ("de", "deu"),
    ("el", "ell"),
    ("en", "eng"),
    ("eo", "epo"),
    ("es", "spa"),
    ("et", "est"),
    ("fa", "pes"),
    ("fi", "fin"),
    ("fr", "fra"),
    ("gu", "guj"),
    ("he", "heb"),
    ("hi", "hin"),
    ("hr", "hrv"),
    ("hu", "hun"),
    ("hy", "hye"),
    ("id", "ind"),
    ("it", "ita"),
    ("ja", "jpn"),
    ("jv", "jav"),
    ("ka", "kat"),
    ("km", "khm"),
    ("kn", "kan"),
    ("ko", "kor"),
    ("la", "lat"),
    ("lt", "lit"),
    ("lv", "lav"),
    ("mk", "mkd"),
    ("ml", "mal"),
    ("mr", "mar"),
    ("my", "mya"),
    ("nb", "nob"),
    ("ne", "nep"),
    ("nl", "nld"),
    ("no", "nob"),
    ("or", "ori"),
    ("pa", "pan"),
    ("pl", "pol"),
    ("pt", "por"),
    ("ro", "ron"),
    ("ru", "rus"),
    ("si", "sin"),
    ("sk", "slk"),
    ("sl", "slv"),
    ("sn", "sna"),
    ("sr", "srp"),
    ("sv", "swe"),
    ("ta", "tam"),
    ("te", "tel"),
    ("th", "tha"),
    ("tk", "tuk"),
    ("tl", "tgl"),
    ("tr", "tur"),
    ("uk", "ukr"),
    ("ur", "urd"),
    ("uz", "uzb"),
    ("vi", "vie"),
    ("yi", "yid"),
    ("zh", "cmn"),
    ("zu", "zul"),
];

/// Unterstützte ISO-639-1-Codes (für Fehlermeldungen, UI und Doku).
pub fn supported_codes() -> impl Iterator<Item = &'static str> {
    ISO1.iter().map(|(c, _)| *c)
}

fn to_lang(code: &str) -> Option<whatlang::Lang> {
    let code = code.trim().to_ascii_lowercase();
    ISO1.iter()
        .find(|(c, _)| *c == code)
        .and_then(|(_, iso3)| whatlang::Lang::from_code(*iso3))
}

/// ISO 639-1 einer erkannten Sprache (Rückfall: 639-3).
fn iso1(lang: whatlang::Lang) -> &'static str {
    let code = lang.code();
    ISO1.iter()
        .find(|(c, iso3)| *iso3 == code && *c != "no")
        .map_or(code, |(c, _)| *c)
}

/// Aktion bei einer nicht erlaubten Sprache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LanguageAction {
    /// Ablehnen (Eingabe `400 language_not_allowed`, Ausgabe `content_filter` je F160).
    Reject,
    /// Nur zählen.
    Flag,
}

impl LanguageAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Reject => "reject",
            Self::Flag => "flag",
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Config {
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    deny: Vec<String>,
    #[serde(default)]
    action: Option<LanguageAction>,
    #[serde(default)]
    direction: Option<Direction>,
    #[serde(default)]
    roles: Option<Vec<RoleSel>>,
    #[serde(default)]
    min_chars: Option<usize>,
    #[serde(default)]
    min_confidence: Option<f64>,
    #[serde(default)]
    stream_sample_chars: Option<usize>,
}

/// Plugin `language`.
#[derive(Debug, Clone)]
pub struct Language {
    allow: Vec<whatlang::Lang>,
    deny: Vec<whatlang::Lang>,
    action: LanguageAction,
    direction: Direction,
    roles: Vec<RoleSel>,
    min_chars: usize,
    min_confidence: f64,
    sample: usize,
}

fn parse_list(field: &str, list: &[String]) -> Result<Vec<whatlang::Lang>, String> {
    list.iter()
        .map(|c| {
            to_lang(c).ok_or_else(|| {
                format!(
                    "{field}: unbekannter Sprachcode {c:?} (ISO 639-1, unterstützt: {})",
                    supported_codes().collect::<Vec<_>>().join(", ")
                )
            })
        })
        .collect()
}

impl Language {
    fn from_config(c: Config) -> Result<Self, String> {
        let allow = parse_list("allow", &c.allow)?;
        let deny = parse_list("deny", &c.deny)?;
        if allow.is_empty() && deny.is_empty() {
            return Err("language: allow oder deny angeben".into());
        }
        if let Some(l) = allow.iter().find(|l| deny.contains(l)) {
            return Err(format!(
                "language: {} steht in allow und deny",
                iso1(*l)
            ));
        }
        let min_chars = c.min_chars.unwrap_or(20);
        if !(1..=10_000).contains(&min_chars) {
            return Err("minChars muss zwischen 1 und 10000 liegen".into());
        }
        let min_confidence = c.min_confidence.unwrap_or(0.5);
        if !(min_confidence.is_finite() && (0.0..=1.0).contains(&min_confidence)) {
            return Err("minConfidence muss zwischen 0 und 1 liegen".into());
        }
        let sample = c.stream_sample_chars.unwrap_or(200).max(min_chars);
        if sample > 10_000 {
            return Err("streamSampleChars darf höchstens 10000 sein".into());
        }
        if c.roles.as_ref().is_some_and(Vec::is_empty) {
            return Err("roles: mindestens eine Rolle".into());
        }
        Ok(Self {
            allow,
            deny,
            action: c.action.unwrap_or(LanguageAction::Reject),
            direction: c.direction.unwrap_or(Direction::Input),
            roles: c.roles.unwrap_or_else(|| vec![RoleSel::User]),
            min_chars,
            min_confidence,
            sample,
        })
    }

    /// Erkannte Sprache, wenn der Text lang genug und die Erkennung sicher genug ist.
    fn detect(&self, text: &str) -> Option<whatlang::Lang> {
        let sample = &text[..floor_boundary(text, SAMPLE_BYTES)];
        let sample = match sample.char_indices().nth(SAMPLE_CHARS) {
            Some((i, _)) => &sample[..i],
            None => sample,
        };
        // Nur Buchstaben zählen für die Mindestlänge (Code, Zahlen, URLs zählen nicht).
        if sample.chars().filter(|c| c.is_alphabetic()).count() < self.min_chars {
            return None;
        }
        let info = whatlang::detect(sample)?;
        (info.confidence() >= self.min_confidence).then_some(info.lang())
    }

    fn allowed(&self, lang: whatlang::Lang) -> bool {
        !self.deny.contains(&lang) && (self.allow.is_empty() || self.allow.contains(&lang))
    }

    /// Prüft einen Text; `Some(Sprache)` bei Verstoß mit `reject`. Zählt Verstöße.
    fn check(&self, text: &str, info: &CallInfo) -> Option<&'static str> {
        let lang = self.detect(text)?;
        if self.allowed(lang) {
            return None;
        }
        let code = iso1(lang);
        info.guardrails.add(NAME, code, self.action.as_str(), 1);
        if self.action == LanguageAction::Flag {
            tracing::info!(request_id = %info.request_id, language = code, "language: markiert");
            return None;
        }
        Some(code)
    }

    fn reject_error(code: &str, output: bool) -> ApiError {
        let msg = if output {
            format!("Die Sprache der Antwort ist nicht erlaubt (erkannt: {code})")
        } else {
            format!("Die Sprache der Anfrage ist nicht erlaubt (erkannt: {code})")
        };
        ApiError::invalid_request(msg).with_code("language_not_allowed")
    }
}

/// Text einer Nachricht (Text-/Ablehnungs-Teile und Tool-Ergebnisse, ohne Tool-Argumente).
fn collect(c: &Content, out: &mut String) {
    match c {
        Content::Text(t) => out.push_str(t),
        Content::Parts(parts) => {
            for p in parts {
                match &p.kind {
                    PartKind::Text { text } | PartKind::Refusal { text } => {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(text);
                    }
                    PartKind::ToolResult { content, .. } => collect(content, out),
                    _ => {}
                }
            }
        }
    }
}

#[derive(Debug, Default)]
struct StreamState {
    decided: bool,
    text: String,
    held: Vec<StreamEvent>,
}

impl Language {
    /// Entscheidet über den bisher gesehenen Text: `Err` bei Verstoß.
    fn decide(&self, st: &mut StreamState, info: &CallInfo) -> Result<(), &'static str> {
        st.decided = true;
        let text = std::mem::take(&mut st.text);
        match self.check(&text, info) {
            Some(code) => Err(code),
            None => Ok(()),
        }
    }
}

impl Plugin for Language {
    fn name(&self) -> &'static str {
        NAME
    }

    fn phases(&self) -> Phases {
        let out = self.direction.output();
        Phases {
            request: self.direction.input(),
            response: out,
            chunk: out,
        }
    }

    fn on_request<'a>(&'a self, ctx: &'a mut ReqCtx<'_>) -> Hook<'a, Flow> {
        let mut text = String::new();
        for m in &ctx.request.messages {
            if !self.roles.iter().any(|r| r.matches(m.role)) {
                continue;
            }
            text.clear();
            collect(&m.content, &mut text);
            if let Some(code) = self.check(&text, ctx.info) {
                return Hook::ok(Flow::Block(Self::reject_error(code, false)));
            }
        }
        Hook::ok(Flow::Continue)
    }

    fn on_response<'a>(&'a self, ctx: &'a mut RespCtx<'_>) -> Hook<'a, Flow> {
        let mut text = String::new();
        for c in &ctx.response.choices {
            text.clear();
            collect(&c.message.content, &mut text);
            if let Some(code) = self.check(&text, ctx.info) {
                return Hook::ok(Flow::Block(Self::reject_error(code, true)));
            }
        }
        Hook::ok(Flow::Continue)
    }

    fn on_chunk(
        &self,
        ctx: &mut ChunkCtx<'_>,
        ev: &mut StreamEvent,
    ) -> Result<ChunkFlow, PluginError> {
        let info = ctx.info;
        let st = ctx.state::<StreamState>();
        if st.decided {
            return Ok(ChunkFlow::Continue);
        }
        if let StreamEvent::TextDelta { text, .. } = ev {
            st.text.push_str(text);
            if st.text.chars().count() < self.sample {
                st.held.push(ev.clone());
                return Ok(ChunkFlow::Drop);
            }
        } else if matches!(
            ev,
            StreamEvent::ReasoningDelta { .. } | StreamEvent::ReasoningSignature { .. }
        ) {
            return Ok(ChunkFlow::Continue);
        }
        // Genug Text oder ein anderes Ereignis (Tool-Aufruf, Ende): jetzt entscheiden.
        let res = self.decide(st, info);
        let held = std::mem::take(&mut st.held);
        if let Err(code) = res {
            return Ok(ChunkFlow::Block(Self::reject_error(code, true)));
        }
        for e in held {
            ctx.emit(e);
        }
        Ok(ChunkFlow::Continue)
    }

    fn on_stream_end(&self, ctx: &mut ChunkCtx<'_>) -> Result<ChunkFlow, PluginError> {
        let info = ctx.info;
        let st = ctx.state::<StreamState>();
        if st.decided {
            return Ok(ChunkFlow::Continue);
        }
        let res = self.decide(st, info);
        let held = std::mem::take(&mut st.held);
        if let Err(code) = res {
            return Ok(ChunkFlow::Block(Self::reject_error(code, true)));
        }
        for e in held {
            ctx.emit(e);
        }
        Ok(ChunkFlow::Continue)
    }
}

/// Fabrik von [`Language`].
#[derive(Debug, Clone, Copy, Default)]
pub struct Factory;

impl PluginFactory for Factory {
    fn name(&self) -> &'static str {
        NAME
    }

    fn build(&self, config: &Value) -> Result<Arc<dyn Plugin>, String> {
        let c: Config = serde_json::from_value(config.clone()).map_err(|e| e.to_string())?;
        Ok(Arc::new(Language::from_config(c)?))
    }

    fn default_fail_mode(&self) -> FailMode {
        FailMode::Closed
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use aigw_core::ir::{ChatRequest, Message, Role, StopReason};
    use aigw_core::wire::WireFormat;
    use http::HeaderMap;
    use serde_json::json;

    use super::*;
    use crate::chain::{Chain, ChunkMode, ChunkRunner, Entry};
    use crate::plugin::{Meta, ResponseHeaders};

    const DE: &str = "Bitte fasse mir den folgenden Bericht über die Quartalszahlen kurz zusammen.";
    const EN: &str = "Please summarize the following report about the quarterly figures briefly.";
    const RU: &str = "Пожалуйста, кратко изложите следующий отчёт о квартальных показателях.";
    const FR: &str = "Veuillez résumer brièvement le rapport suivant sur les chiffres trimestriels.";

    fn plugin(cfg: Value) -> Language {
        Language::from_config(serde_json::from_value(cfg).unwrap()).unwrap()
    }

    fn info() -> CallInfo {
        CallInfo::new("req-1", WireFormat::OpenAiChat)
    }

    fn request(p: &Language, info: &CallInfo, msgs: Vec<Message>) -> Flow {
        let headers = HeaderMap::new();
        let mut ctx = ReqCtx {
            info,
            request: ChatRequest {
                model: "m".into(),
                messages: msgs,
                ..ChatRequest::default()
            },
            client_headers: &headers,
            upstream_headers: HeaderMap::new(),
            response_headers: ResponseHeaders::default(),
            meta: Meta::new(),
        };
        match p.on_request(&mut ctx) {
            Hook::Ready(r) => r.unwrap(),
            Hook::Pending(_) => panic!(),
        }
    }

    #[test]
    fn code_table_matches_whatlang() {
        for (c, iso3) in ISO1 {
            let l = whatlang::Lang::from_code(*iso3)
                .unwrap_or_else(|| panic!("{c} → {iso3} unbekannt"));
            if *c != "no" {
                assert_eq!(iso1(l), *c);
            }
        }
        // Jede Sprache von whatlang hat einen 639-1-Code in der Tabelle.
        for l in whatlang::Lang::all() {
            assert_eq!(iso1(*l).len(), 2, "{l:?}");
        }
    }

    #[test]
    fn config_validation() {
        let f = Factory;
        assert!(f.build(&json!({})).is_err());
        assert!(f.build(&json!({"allow": ["xx"]})).is_err());
        assert!(f.build(&json!({"allow": ["de"], "deny": ["de"]})).is_err());
        assert!(f.build(&json!({"deny": ["ru"], "minConfidence": 2})).is_err());
        assert!(f.build(&json!({"deny": ["ru"], "minChars": 0})).is_err());
        assert!(f.build(&json!({"deny": ["ru"], "roles": []})).is_err());
        assert!(f.build(&json!({"deny": ["ru"], "bogus": 1})).is_err());
        assert!(
            f.build(&json!({"allow": ["DE", "en", "zh", "no"], "action": "flag",
                            "direction": "both", "roles": ["user", "tool"],
                            "minChars": 10, "minConfidence": 0.3,
                            "streamSampleChars": 100}))
                .is_ok()
        );
    }

    #[test]
    fn detects_common_languages() {
        let p = plugin(json!({"deny": ["ru"]}));
        assert_eq!(p.detect(DE).map(iso1), Some("de"));
        assert_eq!(p.detect(EN).map(iso1), Some("en"));
        assert_eq!(p.detect(RU).map(iso1), Some("ru"));
        assert_eq!(p.detect(FR).map(iso1), Some("fr"));
        // Zu kurz: nicht geprüft.
        assert_eq!(p.detect("Привет"), None);
    }

    #[test]
    fn allow_and_deny_on_input() {
        let p = plugin(json!({"allow": ["de", "en"]}));
        let i = info();
        assert_eq!(
            request(&p, &i, vec![Message::text(Role::User, DE), Message::text(Role::User, EN)]),
            Flow::Continue
        );
        // Systemnachricht wird standardmäßig nicht geprüft; kurze Texte auch nicht.
        assert_eq!(
            request(
                &p,
                &i,
                vec![Message::text(Role::System, FR), Message::text(Role::User, "ok")]
            ),
            Flow::Continue
        );
        let Flow::Block(e) = request(&p, &i, vec![Message::text(Role::User, FR)]) else {
            panic!()
        };
        assert_eq!(e.code.as_deref(), Some("language_not_allowed"));
        assert_eq!(e.status(), 400);
        assert!(e.message.contains("fr"));
        let h: Vec<_> = i
            .guardrails
            .snapshot()
            .into_iter()
            .map(|h| (h.category, h.action))
            .collect();
        assert_eq!(h, [("fr".to_owned(), "reject")]);

        let p = plugin(json!({"deny": ["ru"], "action": "flag"}));
        let i = info();
        assert_eq!(
            request(&p, &i, vec![Message::text(Role::User, RU)]),
            Flow::Continue
        );
        assert_eq!(i.guardrails.snapshot()[0].category, "ru");
    }

    fn stream(p: Language, parts: &[&str]) -> Vec<StreamEvent> {
        let chain = Arc::new(Chain::new(vec![Arc::new(Entry::new(NAME, Arc::new(p)))]));
        let mut r = ChunkRunner::new(chain, Arc::new(info()), Meta::new(), ChunkMode::Live);
        let mut out = Vec::new();
        let mut evs = vec![StreamEvent::Start {
            id: "s".into(),
            model: "m".into(),
            created: None,
        }];
        evs.extend(parts.iter().map(|t| StreamEvent::TextDelta {
            index: 0,
            text: (*t).into(),
        }));
        evs.push(StreamEvent::Stop {
            reason: StopReason::EndTurn,
            stop_sequence: None,
        });
        for ev in evs {
            r.push(ev, &mut out);
        }
        r.finish(&mut out);
        out
    }

    fn joined(evs: &[StreamEvent]) -> String {
        evs.iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn stream_output_holds_sample_then_decides() {
        let cfg = json!({"allow": ["de"], "direction": "output", "streamSampleChars": 40});
        let chars: Vec<char> = DE.chars().collect();
        let parts: Vec<String> = chars.chunks(7).map(|c| c.iter().collect()).collect();
        let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
        let out = stream(plugin(cfg.clone()), &parts);
        assert_eq!(joined(&out), DE);
        // Englische Antwort: kein Text geht hinaus, sauberer Abbruch mit content_filter.
        let out = stream(plugin(cfg.clone()), &[EN]);
        assert_eq!(joined(&out), "");
        assert_eq!(
            out.last(),
            Some(&StreamEvent::Stop {
                reason: StopReason::ContentFilter,
                stop_sequence: None
            })
        );
        // Kurze Antwort unter der Stichprobe: Entscheidung am Ende, Text vollständig.
        let out = stream(plugin(cfg), &["Ja", ", gern."]);
        assert_eq!(joined(&out), "Ja, gern.");
    }
}
