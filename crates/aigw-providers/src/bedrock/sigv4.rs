// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! AWS Signature Version 4 (Header-Variante), eigene Implementierung auf aws-lc-rs.
//!
//! Ablauf nach <https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_sigv-create-signed-request.html>:
//!
//! 1. **Kanonische Anfrage**: Methode, kanonischer Pfad (jedes Pfadsegment so, wie es auf der
//!    Leitung steht, *noch einmal* prozentkodiert – für alle Dienste außer S3), sortierte
//!    Query, kanonische Header (Name klein, Wert getrimmt, Leerraumfolgen zu einem Leerzeichen,
//!    nach Name sortiert), Liste der signierten Header, SHA-256 des Bodys (hex).
//! 2. **String to sign**: `AWS4-HMAC-SHA256`, Zeitstempel, Scope
//!    `<datum>/<region>/<dienst>/aws4_request`, SHA-256 der kanonischen Anfrage.
//! 3. **Signierschlüssel**: `HMAC("AWS4"+secret, datum)` → Region → Dienst → `aws4_request`.
//! 4. **Signatur** = hex(HMAC(Signierschlüssel, String to sign)); Header `Authorization`.
//!
//! Die Tests prüfen gegen Fälle der offiziellen AWS-SigV4-Testsuite
//! (`aws-c-auth/tests/aws-signing-test-suite/v4`, gleiche Fälle wie die frühere
//! `aws-sig-v4-test-suite`).

use std::fmt;
use std::sync::Arc;

use aigw_core::ir::ApiError;
use aigw_core::resource::SecretString;
use aws_lc_rs::{digest, hmac};
use http::{HeaderName, HeaderValue, Uri};

use crate::codec::{hex, uri_encode, utc_parts};

/// Algorithmus-Kennung.
pub const ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// SHA-256 eines leeren Bodys (hex).
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Header, die nie signiert werden (werden unterwegs verändert oder sind die Signatur selbst).
const UNSIGNED_HEADERS: &[&str] = &[
    "authorization",
    "user-agent",
    "expect",
    "x-amzn-trace-id",
    "content-length",
    "transfer-encoding",
    "connection",
];

/// AWS-Zugangsdaten.
#[derive(Clone)]
pub struct AwsCredentials {
    /// Access Key ID (nicht geheim, steht in jeder Signatur).
    pub access_key_id: String,
    /// Secret Access Key.
    pub secret_access_key: SecretString,
    /// Session-Token (temporäre Zugangsdaten aus STS/IMDS/Web-Identity).
    pub session_token: Option<SecretString>,
}

impl fmt::Debug for AwsCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AwsCredentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &self.secret_access_key)
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// Quelle für AWS-Zugangsdaten (Erweiterungspunkt).
///
/// Wie [`TokenSource`](crate::auth::TokenSource) synchron: Implementierungen für temporäre
/// Zugangsdaten (IMDSv2, ECS-Container-Credentials, `AssumeRoleWithWebIdentity`) halten die
/// aktuellen Werte im RAM und erneuern sie im Hintergrund vor Ablauf. Eingebaut ist nur
/// [`StaticCredentials`] (Access Key/Secret/Session-Token aus der Credential-Ressource).
pub trait AwsCredentialsProvider: Send + Sync + fmt::Debug {
    /// Aktuelle Zugangsdaten.
    fn credentials(&self) -> Result<Arc<AwsCredentials>, ApiError>;
}

/// Feste Zugangsdaten.
#[derive(Debug, Clone)]
pub struct StaticCredentials(pub Arc<AwsCredentials>);

impl AwsCredentialsProvider for StaticCredentials {
    fn credentials(&self) -> Result<Arc<AwsCredentials>, ApiError> {
        Ok(self.0.clone())
    }
}

/// `YYYYMMDDTHHMMSSZ` (Header `x-amz-date`).
pub fn amz_date(unix_secs: u64) -> String {
    let (y, mo, d, h, mi, s) = utc_parts(unix_secs);
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

/// SHA-256 als Hex.
pub fn sha256_hex(data: &[u8]) -> String {
    hex(digest::digest(&digest::SHA256, data).as_ref())
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> hmac::Tag {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), data)
}

/// Signierschlüssel für Datum (`YYYYMMDD`), Region und Dienst.
pub fn signing_key(secret_access_key: &str, date: &str, region: &str, service: &str) -> [u8; 32] {
    let k_date = hmac_sha256(
        format!("AWS4{secret_access_key}").as_bytes(),
        date.as_bytes(),
    );
    let k_region = hmac_sha256(k_date.as_ref(), region.as_bytes());
    let k_service = hmac_sha256(k_region.as_ref(), service.as_bytes());
    let k_signing = hmac_sha256(k_service.as_ref(), b"aws4_request");
    let mut out = [0u8; 32];
    out.copy_from_slice(k_signing.as_ref());
    out
}

/// Kanonischer Pfad: jedes Byte außer unreservierten Zeichen und `/` wird kodiert – auch `%`,
/// d. h. ein bereits kodierter Leitungspfad wird ein zweites Mal kodiert (SigV4 für alle Dienste
/// außer S3). Leerer Pfad → `/`.
pub fn canonical_uri(path: &str) -> String {
    if path.is_empty() {
        return "/".to_owned();
    }
    uri_encode(path, true)
}

/// Kanonische Query: Paare nach Name, dann Wert sortiert; die Werte werden so übernommen, wie
/// sie auf der Leitung stehen (der Aufrufer kodiert nach RFC 3986).
pub fn canonical_query(query: &str) -> String {
    if query.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<(&str, &str)> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| p.split_once('=').unwrap_or((p, "")))
        .collect();
    pairs.sort_unstable();
    let mut out = String::with_capacity(query.len() + 1);
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        out.push_str(k);
        out.push('=');
        out.push_str(v);
    }
    out
}

fn normalize_value(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for word in v.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// Kanonische Anfrage und Liste der signierten Header.
///
/// `headers`: (Name, Wert) in beliebiger Schreibweise/Reihenfolge; mehrfach vorkommende Namen
/// werden mit `,` zusammengefasst.
pub fn canonical_request(
    method: &str,
    path: &str,
    query: &str,
    headers: &[(&str, &str)],
    payload_sha256: &str,
) -> (String, String) {
    let mut hs: Vec<(String, String)> = headers
        .iter()
        .map(|(n, v)| (n.to_ascii_lowercase(), normalize_value(v)))
        .collect();
    hs.sort_by(|a, b| a.0.cmp(&b.0));
    let mut merged: Vec<(String, String)> = Vec::with_capacity(hs.len());
    for (n, v) in hs {
        match merged.last_mut() {
            Some(last) if last.0 == n => {
                last.1.push(',');
                last.1.push_str(&v);
            }
            _ => merged.push((n, v)),
        }
    }
    let signed: Vec<&str> = merged.iter().map(|(n, _)| n.as_str()).collect();
    let signed = signed.join(";");
    let mut out = String::with_capacity(256);
    out.push_str(method);
    out.push('\n');
    out.push_str(&canonical_uri(path));
    out.push('\n');
    out.push_str(&canonical_query(query));
    out.push('\n');
    for (n, v) in &merged {
        out.push_str(n);
        out.push(':');
        out.push_str(v);
        out.push('\n');
    }
    out.push('\n');
    out.push_str(&signed);
    out.push('\n');
    out.push_str(payload_sha256);
    (out, signed)
}

/// Scope `<datum>/<region>/<dienst>/aws4_request`.
pub fn credential_scope(date: &str, region: &str, service: &str) -> String {
    format!("{date}/{region}/{service}/aws4_request")
}

/// String to sign.
pub fn string_to_sign(amz_date: &str, scope: &str, canonical_request: &str) -> String {
    format!(
        "{ALGORITHM}\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    )
}

/// Signatur (hex) eines Strings to sign.
pub fn signature(signing_key: &[u8; 32], string_to_sign: &str) -> String {
    hex(hmac_sha256(signing_key, string_to_sign.as_bytes()).as_ref())
}

/// Host-Wert, wie hyper ihn sendet (Port nur, wenn nicht Standard des Schemas).
pub fn host_of(uri: &Uri) -> Option<String> {
    let host = uri.host()?;
    let default = match uri.scheme_str() {
        Some("https") => 443,
        _ => 80,
    };
    Some(match uri.port_u16() {
        Some(p) if p != default => format!("{host}:{p}"),
        _ => host.to_owned(),
    })
}

/// Fehler beim Signieren.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SignError {
    /// URI ohne Host.
    #[error("URI ohne Host")]
    NoHost,
    /// Zugangsdaten enthalten unzulässige Header-Zeichen.
    #[error("Zugangsdaten enthalten unzulässige Zeichen")]
    InvalidCredentials,
}

/// Signiert eine fertige Anfrage: setzt `x-amz-date`, ggf. `x-amz-security-token` und
/// `Authorization`. Signiert werden `host` sowie alle vorhandenen Header außer
/// [`UNSIGNED_HEADERS`] (`user-agent`, `content-length` …).
pub fn sign_request<B: AsRef<[u8]>>(
    req: &mut http::Request<B>,
    creds: &AwsCredentials,
    region: &str,
    service: &str,
    unix_secs: u64,
) -> Result<(), SignError> {
    let date_time = amz_date(unix_secs);
    let date = &date_time[..8];
    let host = host_of(req.uri()).ok_or(SignError::NoHost)?;
    let payload_hash = sha256_hex(req.body().as_ref());
    let path = req.uri().path().to_owned();
    let query = req.uri().query().unwrap_or("").to_owned();
    let method = req.method().as_str().to_owned();

    let headers = req.headers_mut();
    headers.insert(
        HeaderName::from_static("x-amz-date"),
        HeaderValue::from_str(&date_time).map_err(|_| SignError::InvalidCredentials)?,
    );
    if let Some(token) = &creds.session_token {
        let mut v =
            HeaderValue::from_str(token.expose()).map_err(|_| SignError::InvalidCredentials)?;
        v.set_sensitive(true);
        headers.insert(HeaderName::from_static("x-amz-security-token"), v);
    }

    let mut list: Vec<(&str, &str)> = Vec::with_capacity(headers.len() + 1);
    list.push(("host", &host));
    for (name, value) in headers.iter() {
        let n = name.as_str();
        if n == "host" || UNSIGNED_HEADERS.contains(&n) {
            continue;
        }
        // Nicht-UTF-8-Werte setzt der Adapter nie; sicherheitshalber überspringen.
        if let Ok(v) = value.to_str() {
            list.push((n, v));
        }
    }
    let (canonical, signed) = canonical_request(&method, &path, &query, &list, &payload_hash);
    let scope = credential_scope(date, region, service);
    let sts = string_to_sign(&date_time, &scope, &canonical);
    let key = signing_key(creds.secret_access_key.expose(), date, region, service);
    let sig = signature(&key, &sts);
    let auth = format!(
        "{ALGORITHM} Credential={}/{scope}, SignedHeaders={signed}, Signature={sig}",
        creds.access_key_id
    );
    let mut v = HeaderValue::from_str(&auth).map_err(|_| SignError::InvalidCredentials)?;
    v.set_sensitive(true);
    req.headers_mut().insert(http::header::AUTHORIZATION, v);
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    // Quelle der Fixtures: offizielle AWS-SigV4-Testsuite,
    // https://github.com/awslabs/aws-c-auth/tree/main/tests/aws-signing-test-suite/v4
    // (Dateien request.txt, context.json, header-canonical-request.txt,
    // header-string-to-sign.txt, header-signature.txt). Gemeinsamer Kontext:
    // AKIDEXAMPLE / wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY, us-east-1, Dienst "service",
    // 2015-08-30T12:36:00Z.
    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
    const TIME: u64 = 1_440_938_160;
    const SESSION_TOKEN: &str = "AQoDYXdzEPT//////////wEXAMPLEtc764bNrC9SAPBSM22wDOk4x4HIZ8j4FZTwdQWLWsKWHGBuFqwAeMicRXmxfpSPfIeoIYRqTflfKD8YUuwthAx7mSEI/qkPpKPi/kMcGdQrmGdeehM4IC1NtBmUpp2wUE8phUZampKsburEDy0KPkyQDYwT7WZ0wq5VSXDvp75YU9HFvlRd8Tx6q6fE8YQcHNVXAkiY9q6d+xo0rKwT38xVqr7ZD0u0iPPkUL64lIZbqBAz+scqKmlzm8FDrypNC9Yjc8fPOLn9FX9KSYvKTr4rvx3iSIlTJabIQwj2ICCR/oLxBA==";

    struct Case {
        name: &'static str,
        method: &'static str,
        path: &'static str,
        query: &'static str,
        headers: &'static [(&'static str, &'static str)],
        body: &'static str,
        canonical: &'static str,
        string_to_sign: &'static str,
        signature: &'static str,
    }

    const CASES: &[Case] = &[
        Case {
            name: "get-vanilla",
            method: "GET",
            path: "/",
            query: "",
            headers: &[
                ("Host", "example.amazonaws.com"),
                ("X-Amz-Date", "20150830T123600Z"),
            ],
            body: "",
            canonical: "GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            string_to_sign: "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/service/aws4_request\nbb579772317eb040ac9ed261061d46c1f17a8133879d6129b6e1c25292927e63",
            signature: "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31",
        },
        Case {
            name: "post-vanilla",
            method: "POST",
            path: "/",
            query: "",
            headers: &[
                ("Host", "example.amazonaws.com"),
                ("X-Amz-Date", "20150830T123600Z"),
            ],
            body: "",
            canonical: "POST\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            string_to_sign: "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/service/aws4_request\n553f88c9e4d10fc9e109e2aeb65f030801b70c2f6468faca261d401ae622fc87",
            signature: "5da7c1a2acd57cee7505fc6676e4e544621c30862966e37dddb68e92efbe5d6b",
        },
        Case {
            name: "get-vanilla-query-order-key-case",
            method: "GET",
            path: "/",
            query: "Param2=value2&Param1=value1",
            headers: &[
                ("Host", "example.amazonaws.com"),
                ("X-Amz-Date", "20150830T123600Z"),
            ],
            body: "",
            canonical: "GET\n/\nParam1=value1&Param2=value2\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            string_to_sign: "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/service/aws4_request\n816cd5b414d056048ba4f7c5386d6e0533120fb1fcfa93762cf0fc39e2cf19e0",
            signature: "b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500",
        },
        Case {
            name: "post-header-key-sort",
            method: "POST",
            path: "/",
            query: "",
            headers: &[
                ("Host", "example.amazonaws.com"),
                ("My-Header1", "value1"),
                ("X-Amz-Date", "20150830T123600Z"),
            ],
            body: "",
            canonical: "POST\n/\n\nhost:example.amazonaws.com\nmy-header1:value1\nx-amz-date:20150830T123600Z\n\nhost;my-header1;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            string_to_sign: "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/service/aws4_request\n9368318c2967cf6de74404b30c65a91e8f6253e0a8659d6d5319f1a812f87d65",
            signature: "c5410059b04c1ee005303aed430f6e6645f61f4dc9e1461ec8f8916fdf18852c",
        },
        Case {
            name: "get-utf8",
            method: "GET",
            path: "/ሴ",
            query: "",
            headers: &[
                ("Host", "example.amazonaws.com"),
                ("X-Amz-Date", "20150830T123600Z"),
            ],
            body: "",
            canonical: "GET\n/%E1%88%B4\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            string_to_sign: "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/service/aws4_request\n2a0a97d02205e45ce2e994789806b19270cfbbb0921b278ccf58f5249ac42102",
            signature: "8318018e0b0f223aa2bbf98705b62bb787dc9c0e678f255a891fd03141be5d85",
        },
        Case {
            name: "get-space-unnormalized",
            method: "GET",
            path: "/example space/",
            query: "",
            headers: &[
                ("Host", "example.amazonaws.com"),
                ("X-Amz-Date", "20150830T123600Z"),
            ],
            body: "",
            canonical: "GET\n/example%20space/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            string_to_sign: "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/service/aws4_request\n63ee75631ed7234ae61b5f736dfc7754cdccfedbff4b5128a915706ee9390d86",
            signature: "652487583200325589f1fba4c7e578f72c47cb61beeca81406b39ddec1366741",
        },
        Case {
            name: "post-x-www-form-urlencoded",
            method: "POST",
            path: "/",
            query: "",
            headers: &[
                ("Content-Type", "application/x-www-form-urlencoded"),
                ("Host", "example.amazonaws.com"),
                ("Content-Length", "13"),
                (
                    "X-Amz-Content-Sha256",
                    "9095672bbd1f56dfc5b65f3e153adc8731a4a654192329106275f4c7b24d0b6e",
                ),
                ("X-Amz-Date", "20150830T123600Z"),
            ],
            body: "Param1=value1",
            canonical: "POST\n/\n\ncontent-length:13\ncontent-type:application/x-www-form-urlencoded\nhost:example.amazonaws.com\nx-amz-content-sha256:9095672bbd1f56dfc5b65f3e153adc8731a4a654192329106275f4c7b24d0b6e\nx-amz-date:20150830T123600Z\n\ncontent-length;content-type;host;x-amz-content-sha256;x-amz-date\n9095672bbd1f56dfc5b65f3e153adc8731a4a654192329106275f4c7b24d0b6e",
            string_to_sign: "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/service/aws4_request\nb1edd1d03544c25390e32085d55b57acc9a3961bb59415ff86c45c3d89d16cfb",
            signature: "d3875051da38690788ef43de4db0d8f280229d82040bfac253562e56c3f20e0b",
        },
        Case {
            name: "post-sts-header-before",
            method: "POST",
            path: "/",
            query: "",
            headers: &[
                ("Host", "example.amazonaws.com"),
                ("X-Amz-Date", "20150830T123600Z"),
                ("X-Amz-Security-Token", SESSION_TOKEN),
            ],
            body: "",
            canonical: "",
            string_to_sign: "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/service/aws4_request\nc237e1b440d4c63c32ca95b5b99481081cb7b13c7e40434868e71567c1a882f6",
            signature: "85d96828115b5dc0cfc3bd16ad9e210dd772bbebba041836c64533a82be05ead",
        },
    ];

    #[test]
    fn official_test_suite_vectors() {
        let key = signing_key(SECRET, "20150830", "us-east-1", "service");
        for c in CASES {
            let (canonical, _) = canonical_request(
                c.method,
                c.path,
                c.query,
                c.headers,
                &sha256_hex(c.body.as_bytes()),
            );
            if !c.canonical.is_empty() {
                assert_eq!(canonical, c.canonical, "{}: kanonische Anfrage", c.name);
            }
            let sts = string_to_sign(
                "20150830T123600Z",
                &credential_scope("20150830", "us-east-1", "service"),
                &canonical,
            );
            assert_eq!(sts, c.string_to_sign, "{}: string to sign", c.name);
            assert_eq!(signature(&key, &sts), c.signature, "{}: Signatur", c.name);
        }
    }

    #[test]
    fn signing_key_derivation_example() {
        // Beispiel aus der AWS-Doku „Examples of how to derive a signing key for Signature
        // Version 4“ (iam, us-east-1, 20120215).
        let key = signing_key(SECRET, "20120215", "us-east-1", "iam");
        assert_eq!(
            hex(&key),
            "f4780e2d9f65fa895f9c67b32ce1baf0b0d8a43505a000a1a9e090d414db404d"
        );
    }

    #[test]
    fn sign_request_matches_suite_with_session_token() {
        // post-sts-header-before über die Anfrage-API (http::Request).
        let mut req = http::Request::builder()
            .method("POST")
            .uri("https://example.amazonaws.com/")
            .body(Vec::<u8>::new())
            .unwrap();
        let creds = AwsCredentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: SecretString::new(SECRET),
            session_token: Some(SecretString::new(SESSION_TOKEN)),
        };
        sign_request(&mut req, &creds, "us-east-1", "service", TIME).unwrap();
        let auth = req.headers()["authorization"].to_str().unwrap();
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date;x-amz-security-token, \
             Signature=85d96828115b5dc0cfc3bd16ad9e210dd772bbebba041836c64533a82be05ead"
        );
        assert_eq!(req.headers()["x-amz-date"], "20150830T123600Z");
        assert!(req.headers()["authorization"].is_sensitive());
    }

    #[test]
    fn bedrock_path_is_double_encoded() {
        // Modell-IDs mit ':' bzw. ARNs mit '/' stehen kodiert im Pfad; SigV4 kodiert erneut.
        assert_eq!(
            canonical_uri("/model/anthropic.claude-3-haiku-20240307-v1%3A0/converse"),
            "/model/anthropic.claude-3-haiku-20240307-v1%253A0/converse"
        );
        assert_eq!(canonical_uri(""), "/");
    }

    #[test]
    fn host_without_default_port() {
        let u: Uri = "https://bedrock-runtime.eu-central-1.amazonaws.com:443/x"
            .parse()
            .unwrap();
        assert_eq!(
            host_of(&u).unwrap(),
            "bedrock-runtime.eu-central-1.amazonaws.com"
        );
        let u: Uri = "http://127.0.0.1:8080/x".parse().unwrap();
        assert_eq!(host_of(&u).unwrap(), "127.0.0.1:8080");
    }

    #[test]
    fn debug_hides_secrets() {
        let creds = AwsCredentials {
            access_key_id: "AKID".into(),
            secret_access_key: SecretString::new("geheim"),
            session_token: Some(SecretString::new("tok")),
        };
        let s = format!("{creds:?}");
        assert!(!s.contains("geheim") && !s.contains("tok\""));
    }
}
