// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Microsoft Entra ID als [`TokenSource`](crate::auth::TokenSource) für Azure OpenAI und Claude in
//! Azure AI Foundry (`provider: anthropic`)
//! (`settings.auth: entraId`, AIGW-82).
//!
//! | `settings.entraCredential` | Ablauf | Herkunft |
//! |---|---|---|
//! | `clientSecret` | OAuth 2.0 Client Credentials mit `client_secret` | Zugangsdaten `tenantId`, `clientId`, `clientSecret` |
//! | `clientCertificate` | Client Credentials mit signierter JWT-Assertion (RS256, `x5t`, optional `x5c`) | Zugangsdaten `tenantId`, `clientId`, `clientCertificate` (PEM: Zertifikat(e) + privater RSA-Schlüssel, auch verschlüsselt mit `clientCertificatePassword`) |
//! | `workloadIdentity` | Client Credentials mit föderiertem Token (`client_assertion` aus `AZURE_FEDERATED_TOKEN_FILE`) | Prozessumgebung (AKS), `tenantId`/`clientId` auch aus Zugangsdaten/Einstellungen |
//! | `managedIdentity` | Azure-Metadatendienst (IMDS `169.254.169.254`), App Service (`IDENTITY_ENDPOINT` + `IDENTITY_HEADER`) bzw. Azure Arc (`IDENTITY_ENDPOINT` + `IMDS_ENDPOINT`, Challenge mit Schlüsseldatei) | optional `clientId` (benutzerzugewiesen; nicht bei Azure Arc) |
//!
//! **Client-Zertifikat (AIGW-94):** Verschlüsselte PKCS#8-Schlüssel (`ENCRYPTED PRIVATE KEY`,
//! PBES2/PBKDF2/AES-CBC, siehe [`crate::entra_key`]) mit dem Passwort aus dem Zugangsdaten-Feld
//! `clientCertificatePassword` (als Wert oder per `valueFrom` aus Datei/Umgebung).
//! `settings.entraSendX5c: true` (bzw. `spec.entra.sendX5c`) schickt die Zertifikatskette als
//! `x5c` im Header der Assertion mit – nötig für die Anmeldung über Subject Name + Issuer
//! („SNI“, automatische Zertifikatsrotation ohne neuen Fingerabdruck in Entra).
//!
//! **Azure Arc (AIGW-94):** Der lokale Agent beantwortet die erste Anfrage mit `401` und
//! `WWW-Authenticate: Basic realm=<Datei>`; die Datei muss im Arc-Verzeichnis
//! ([`ARC_TOKEN_DIR`]) liegen, auf `.key` enden und höchstens 4 KiB groß sein (Pfad nach
//! Auflösen symbolischer Links geprüft). Ihr Inhalt geht als `Authorization: Basic …` in die
//! zweite Anfrage. Wie jede Managed Identity nur mit `upstream.entra.ambient_identity`.
//!
//! Ohne `entraCredential` gilt `clientSecret` bzw. `clientCertificate`, je nachdem, welches Feld
//! die Zugangsdaten enthalten.
//!
//! - **Scope:** `settings.entraScope`, Standard [`COGNITIVE_SERVICES_SCOPE`]. Bei
//!   Workload/Managed Identity (Identität des Gateways selbst) nur die Cognitive-Services-Scopes
//!   der Azure-Clouds ([`AMBIENT_SCOPES`]).
//! - **Authority:** `settings.entraAuthority` (Sovereign Clouds, z. B.
//!   `https://login.microsoftonline.us`), Standard [`DEFAULT_AUTHORITY`]. Bei Workload Identity
//!   ausschließlich aus `AZURE_AUTHORITY_HOST` (das föderierte Token des Pods geht an keinen per
//!   Admin-API gewählten Host).
//! - **Cache je Credential:** Deployments mit denselben Zugangsdaten (und gleichem Scope,
//!   gleicher Authority) teilen sich eine Token-Quelle – auch über Konfigurationswechsel hinweg,
//!   solange ein Deployment sie nutzt. Der Schlüssel ist ein SHA-256 über alle Merkmale
//!   einschließlich des Geheimnisses (Rotation = neue Quelle); Geheimnisse selbst liegen nur in
//!   der Quelle.
//! - **HTTP:** Alle Abrufe laufen über aigw-outbound mit dem Client des Deployments
//!   (`upstream.roots`, SSRF-Regeln). Managed Identity schaltet über
//!   [`aigw_outbound::OutboundClient::with_exempt_host`] gezielt nur den Metadaten-Endpunkt frei.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError, Weak};

use aigw_core::ir::{ErrorClass, UpstreamError};
use aigw_core::resource::SecretString;
use aigw_core::snapshot::DeploymentCtx;
use aws_lc_rs::digest;
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use bytes::Bytes;
use http::header::{ACCEPT, AUTHORIZATION, WWW_AUTHENTICATE};
use http::{HeaderName, HeaderValue, Method, Uri};
use serde_json::json;

use crate::auth::{
    FetchFuture, FetchedToken, RefreshingToken, TokenFetcher, parse_token_reply, post_token_form,
    send_token_request, token_error,
};
use crate::client::UpstreamClient;
use crate::codec::{base64_decode, base64url, random_id, unix_now, uri_encode};
use crate::settings::Settings;
use crate::{ConfigError, codec};

/// Scope für Azure OpenAI / Azure AI Services (öffentliche Cloud).
pub const COGNITIVE_SERVICES_SCOPE: &str = "https://cognitiveservices.azure.com/.default";
/// Erlaubte Scopes für Workload/Managed Identity (öffentlich, US Government, China).
pub const AMBIENT_SCOPES: &[&str] = &[
    COGNITIVE_SERVICES_SCOPE,
    "https://cognitiveservices.azure.us/.default",
    "https://cognitiveservices.azure.cn/.default",
];
/// Standard-Authority (öffentliche Cloud).
pub const DEFAULT_AUTHORITY: &str = "https://login.microsoftonline.com";
/// Azure Instance Metadata Service (Managed Identity auf VMs, AKS-Knoten, Container Apps mit
/// IMDS).
pub const IMDS_ENDPOINT: &str = "http://169.254.169.254/metadata/identity/oauth2/token";
const IMDS_API_VERSION: &str = "2018-02-01";
const APP_SERVICE_API_VERSION: &str = "2019-08-01";
const ARC_API_VERSION: &str = "2020-06-01";
/// Verzeichnis der Challenge-Schlüsseldateien des Azure-Arc-Agenten (Connected Machine).
#[cfg(not(windows))]
pub const ARC_TOKEN_DIR: &str = "/var/opt/azcmagent/tokens";
/// Verzeichnis der Challenge-Schlüsseldateien des Azure-Arc-Agenten (Connected Machine).
#[cfg(windows)]
pub const ARC_TOKEN_DIR: &str = r"C:\ProgramData\AzureConnectedMachineAgent\Tokens";
/// Höchstgröße der Arc-Schlüsseldatei (wie die Azure SDKs).
const ARC_MAX_KEY_FILE: u64 = 4096;
/// Lebensdauer der Client-Assertion (Entra: höchstens 10 Minuten sinnvoll).
const ASSERTION_LIFETIME_SECS: u64 = 600;
const CLIENT_ASSERTION_TYPE: &str = "urn:ietf:params:oauth:client-assertion-type:jwt-bearer";

/// Werte aus der Prozessumgebung (einmal gelesen; Tests setzen sie direkt).
#[derive(Clone, Default)]
pub struct EntraEnv {
    /// `AZURE_TENANT_ID`.
    pub tenant_id: Option<String>,
    /// `AZURE_CLIENT_ID`.
    pub client_id: Option<String>,
    /// `AZURE_FEDERATED_TOKEN_FILE` (Workload Identity).
    pub federated_token_file: Option<PathBuf>,
    /// `AZURE_AUTHORITY_HOST` (Workload Identity, Sovereign Clouds).
    pub authority_host: Option<String>,
    /// `IDENTITY_ENDPOINT` (App Service / Functions / Container Apps).
    pub identity_endpoint: Option<String>,
    /// `IDENTITY_HEADER` (Geheimnis zu `IDENTITY_ENDPOINT`).
    pub identity_header: Option<SecretString>,
    /// IMDS-Endpunkt (Standard [`IMDS_ENDPOINT`]; nur Tests ändern ihn).
    pub imds_endpoint: Option<String>,
    /// `IMDS_ENDPOINT` (Azure Arc setzt ihn zusammen mit `IDENTITY_ENDPOINT`; dient nur der
    /// Erkennung).
    pub arc_imds_endpoint: Option<String>,
    /// Erlaubtes Verzeichnis der Arc-Schlüsseldateien (Standard [`ARC_TOKEN_DIR`]; nur Tests
    /// ändern es).
    pub arc_token_dir: Option<PathBuf>,
    /// Workload/Managed Identity (die Identität des Gateway-Prozesses) freigegeben?
    /// Startkonfiguration `upstream.entra.ambient_identity` (AIGW-90), Standard `false`.
    pub ambient_identity: bool,
}

/// Prozessweite Freigabe für Workload/Managed Identity (`upstream.entra.ambient_identity`).
static AMBIENT_IDENTITY: AtomicBool = AtomicBool::new(false);

/// Schlüssel der Startkonfiguration, der Workload/Managed Identity freigibt.
pub const AMBIENT_IDENTITY_KEY: &str = "upstream.entra.ambient_identity";

/// Setzt die Freigabe für Workload/Managed Identity (beim Start aus
/// `upstream.entra.ambient_identity`, vor dem ersten Routenaufbau).
pub fn set_ambient_identity(allowed: bool) {
    AMBIENT_IDENTITY.store(allowed, Ordering::Relaxed);
}

/// Ist Workload/Managed Identity prozessweit freigegeben?
pub fn ambient_identity() -> bool {
    AMBIENT_IDENTITY.load(Ordering::Relaxed)
}

impl fmt::Debug for EntraEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EntraEnv")
            .field("tenant_id", &self.tenant_id)
            .field("client_id", &self.client_id)
            .field("federated_token_file", &self.federated_token_file)
            .field("authority_host", &self.authority_host)
            .field("identity_endpoint", &self.identity_endpoint)
            .field(
                "identity_header",
                &self.identity_header.as_ref().map(|_| "***"),
            )
            .field("arc_imds_endpoint", &self.arc_imds_endpoint)
            .field("ambient_identity", &self.ambient_identity)
            .finish_non_exhaustive()
    }
}

impl EntraEnv {
    /// Liest die Umgebungsvariablen des Prozesses (leere Werte zählen als nicht gesetzt).
    pub fn from_process() -> Self {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        Self {
            tenant_id: var("AZURE_TENANT_ID"),
            client_id: var("AZURE_CLIENT_ID"),
            federated_token_file: var("AZURE_FEDERATED_TOKEN_FILE").map(PathBuf::from),
            authority_host: var("AZURE_AUTHORITY_HOST"),
            identity_endpoint: var("IDENTITY_ENDPOINT"),
            identity_header: var("IDENTITY_HEADER").map(SecretString::new),
            imds_endpoint: None,
            arc_imds_endpoint: var("IMDS_ENDPOINT"),
            arc_token_dir: None,
            // Wird je Aufruf aus [`ambient_identity`] übernommen (siehe [`token_source`]).
            ambient_identity: false,
        }
    }
}

/// Zertifikat (DER des ersten `CERTIFICATE`-Blocks), optional die Kette für `x5c` und der
/// private RSA-Schlüssel.
struct ClientCertificate {
    /// `x5t`: Base64url(SHA-1(DER)) – von Entra zur Zuordnung des Zertifikats verlangt.
    x5t: String,
    /// `x5c`: alle `CERTIFICATE`-Blöcke (Blatt zuerst) als Standard-Base64 (RFC 7515 §4.1.6);
    /// nur mit `sendX5c`.
    x5c: Option<Vec<String>>,
    key: RsaKeyPair,
}

impl fmt::Debug for ClientCertificate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientCertificate")
            .field("x5t", &self.x5t)
            .field("x5c", &self.x5c.as_ref().map(Vec::len))
            .finish_non_exhaustive()
    }
}

/// Höchstzahl der Zertifikate in `x5c` (Blatt + Zwischenzertifikate).
const MAX_X5C: usize = 10;

/// Alle PEM-Blöcke mit `label` (z. B. `CERTIFICATE`) als DER.
fn pem_blocks(pem: &str, label: &str) -> Result<Vec<Vec<u8>>, String> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let mut out = Vec::new();
    let mut rest = pem;
    while let Some(i) = rest.find(&begin) {
        let after = &rest[i + begin.len()..];
        let j = after
            .find(&end)
            .ok_or_else(|| format!("PEM: END-Zeile für {label} fehlt"))?;
        out.push(
            base64_decode(&after[..j]).ok_or_else(|| format!("PEM: Base64 in {label} ungültig"))?,
        );
        rest = &after[j + end.len()..];
    }
    Ok(out)
}

impl ClientCertificate {
    /// PEM mit Zertifikat(en) und privatem Schlüssel; `password` für einen verschlüsselten
    /// PKCS#8-Schlüssel (`ENCRYPTED PRIVATE KEY`). Meldungen nennen nie Passwort oder Schlüssel.
    fn from_pem(pem: &str, password: Option<&str>, send_x5c: bool) -> Result<Self, String> {
        let pem = pem.replace("\\n", "\n");
        let certs = pem_blocks(&pem, "CERTIFICATE")?;
        let cert = certs
            .first()
            .ok_or("clientCertificate: kein CERTIFICATE-Block")?;
        if certs.iter().any(|c| c.first() != Some(&0x30)) {
            return Err("clientCertificate: Zertifikat ist kein DER-Objekt".into());
        }
        if send_x5c && certs.len() > MAX_X5C {
            return Err(format!(
                "clientCertificate: höchstens {MAX_X5C} Zertifikate für x5c"
            ));
        }
        let invalid = "clientCertificate: privater Schlüssel ist kein gültiger RSA-Schlüssel";
        let key = if let Some(der) = pem_blocks(&pem, "ENCRYPTED PRIVATE KEY")?
            .into_iter()
            .next()
        {
            let password = password.ok_or(
                "clientCertificate: Schlüssel ist verschlüsselt – Passwort in \
                 clientCertificatePassword angeben",
            )?;
            let mut plain = crate::entra_key::decrypt_pkcs8(&der, password.as_bytes())
                .map_err(|e| format!("clientCertificate: {e}"))?;
            let key = RsaKeyPair::from_pkcs8(&plain).map_err(|_| invalid.to_owned());
            plain.fill(0);
            key?
        } else {
            if password.is_some() {
                return Err(
                    "clientCertificatePassword angegeben, aber der Schlüssel ist nicht \
                     verschlüsselt (ENCRYPTED PRIVATE KEY erwartet)"
                        .into(),
                );
            }
            if let Some(der) = pem_blocks(&pem, "PRIVATE KEY")?.into_iter().next() {
                RsaKeyPair::from_pkcs8(&der).map_err(|_| invalid.to_owned())?
            } else if pem.contains("Proc-Type: 4,ENCRYPTED") {
                return Err(
                    "clientCertificate: verschlüsselter Schlüssel im alten OpenSSL-Format – \
                     nach PKCS#8 umwandeln (openssl pkcs8 -topk8 -v2 aes-256-cbc)"
                        .into(),
                );
            } else if let Some(der) = pem_blocks(&pem, "RSA PRIVATE KEY")?.into_iter().next() {
                RsaKeyPair::from_der(&der).map_err(|_| invalid.to_owned())?
            } else {
                return Err(
                    "clientCertificate: kein privater Schlüssel (PRIVATE KEY, ENCRYPTED PRIVATE \
                     KEY bzw. RSA PRIVATE KEY)"
                        .into(),
                );
            }
        };
        let thumb = digest::digest(&digest::SHA1_FOR_LEGACY_USE_ONLY, cert);
        Ok(Self {
            x5t: base64url(thumb.as_ref()),
            x5c: send_x5c.then(|| certs.iter().map(|c| codec::base64(c)).collect()),
            key,
        })
    }

    /// Signierte Client-Assertion (RFC 7523) für `client_id` an `audience` (Token-URL).
    fn assertion(&self, client_id: &str, audience: &str, now: u64) -> Result<String, String> {
        let mut header = json!({ "alg": "RS256", "typ": "JWT", "x5t": self.x5t });
        if let Some(chain) = &self.x5c {
            header["x5c"] = json!(chain);
        }
        let claims = json!({
            "aud": audience,
            "iss": client_id,
            "sub": client_id,
            "jti": random_id(""),
            "nbf": now,
            "iat": now,
            "exp": now + ASSERTION_LIFETIME_SECS,
        });
        let mut input = base64url(header.to_string().as_bytes());
        input.push('.');
        input.push_str(&base64url(claims.to_string().as_bytes()));
        let mut sig = vec![0u8; self.key.public_modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                input.as_bytes(),
                &mut sig,
            )
            .map_err(|_| "Signieren der Client-Assertion fehlgeschlagen".to_owned())?;
        input.push('.');
        input.push_str(&base64url(&sig));
        Ok(input)
    }
}

/// Managed-Identity-Endpunkt.
enum MiEndpoint {
    /// IMDS (`Metadata: true`).
    Imds(Uri),
    /// App Service (`X-IDENTITY-HEADER`).
    AppService(Uri, HeaderValue),
    /// Azure Arc (Challenge mit Schlüsseldatei aus dem erlaubten Verzeichnis).
    Arc(Uri, PathBuf),
}

/// Art der Anmeldung.
enum Grant {
    ClientSecret(SecretString),
    ClientCertificate(ClientCertificate),
    WorkloadIdentity(PathBuf),
    ManagedIdentity(MiEndpoint),
}

/// Holt Entra-ID-Tokens für einen Satz Zugangsdaten.
pub struct EntraFetcher {
    method: Grant,
    /// Token-URL (`{authority}/{tenant}/oauth2/v2.0/token`); bei Managed Identity die
    /// Endpunkt-URL ohne Abfrage.
    token_url: Uri,
    client_id: Option<String>,
    scope: String,
}

impl fmt::Debug for EntraFetcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EntraFetcher")
            .field("method", &self.method_name())
            .field("token_url", &self.token_url)
            .field("client_id", &self.client_id)
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

/// Token-Quelle für Entra ID mit Cache und Hintergrund-Erneuerung.
pub type EntraTokenSource = RefreshingToken<EntraFetcher>;

impl EntraFetcher {
    /// `clientSecret`, `clientCertificate`, `workloadIdentity` oder `managedIdentity`.
    pub fn method_name(&self) -> &'static str {
        match self.method {
            Grant::ClientSecret(_) => "clientSecret",
            Grant::ClientCertificate(_) => "clientCertificate",
            Grant::WorkloadIdentity(_) => "workloadIdentity",
            Grant::ManagedIdentity(_) => "managedIdentity",
        }
    }

    /// Token-URL bzw. Managed-Identity-Endpunkt.
    pub fn token_url(&self) -> &Uri {
        &self.token_url
    }

    fn client_id(&self) -> &str {
        self.client_id.as_deref().unwrap_or_default()
    }

    /// Formular für den Client-Credentials-Grant (ohne Nachweis).
    fn grant(&self) -> String {
        format!(
            "grant_type=client_credentials&client_id={}&scope={}",
            uri_encode(self.client_id(), false),
            uri_encode(&self.scope, false)
        )
    }

    fn with_assertion(&self, assertion: &str) -> String {
        format!(
            "{}&client_assertion_type={}&client_assertion={}",
            self.grant(),
            uri_encode(CLIENT_ASSERTION_TYPE, false),
            uri_encode(assertion, false)
        )
    }

    async fn managed_identity(
        &self,
        client: &UpstreamClient,
        endpoint: &MiEndpoint,
    ) -> Result<FetchedToken, UpstreamError> {
        let resource = self.scope.strip_suffix("/.default").unwrap_or(&self.scope);
        let (base, version) = match endpoint {
            MiEndpoint::Imds(u) => (u, IMDS_API_VERSION),
            MiEndpoint::AppService(u, _) => (u, APP_SERVICE_API_VERSION),
            MiEndpoint::Arc(u, _) => (u, ARC_API_VERSION),
        };
        let mut url = format!(
            "{base}?api-version={version}&resource={}",
            uri_encode(resource, false)
        );
        if let Some(id) = &self.client_id {
            url.push_str("&client_id=");
            url.push_str(&uri_encode(id, false));
        }
        let build = |auth: Option<&HeaderValue>| {
            let mut req = http::Request::builder()
                .method(Method::GET)
                .uri(url.as_str())
                .header(ACCEPT, HeaderValue::from_static("application/json"));
            req = match endpoint {
                MiEndpoint::Imds(_) | MiEndpoint::Arc(..) => {
                    req.header("metadata", HeaderValue::from_static("true"))
                }
                MiEndpoint::AppService(_, h) => {
                    req.header(HeaderName::from_static("x-identity-header"), h.clone())
                }
            };
            if let Some(a) = auth {
                req = req.header(AUTHORIZATION, a.clone());
            }
            req.body(Bytes::new())
                .map_err(|e| token_error(ErrorClass::Internal, e.to_string(), Some(500)))
        };
        let mut reply = send_token_request(client, build(None)?).await?;
        if let MiEndpoint::Arc(_, dir) = endpoint {
            if reply.status != http::StatusCode::UNAUTHORIZED {
                tracing::warn!(
                    status = reply.status.as_u16(),
                    "Azure Arc: Challenge (401) erwartet"
                );
                return Err(token_error(
                    ErrorClass::Auth,
                    format!(
                        "Managed Identity (Azure Arc): Challenge erwartet, HTTP {}",
                        reply.status
                    ),
                    None,
                ));
            }
            let auth = arc_challenge(&reply.headers, dir).await?;
            reply = send_token_request(client, build(Some(&auth))?).await?;
        }
        // IMDS: 404 (Identität wird gerade zugewiesen) und 410 (Dienst startet) sind
        // vorübergehend.
        if matches!(reply.status.as_u16(), 404 | 410) {
            tracing::warn!(
                status = reply.status.as_u16(),
                "Managed Identity: Metadatendienst noch nicht bereit"
            );
            return Err(token_error(
                ErrorClass::ServerError,
                format!(
                    "Managed Identity: Metadatendienst nicht bereit (HTTP {})",
                    reply.status
                ),
                Some(reply.status.as_u16()),
            ));
        }
        parse_token_reply(reply.status, &reply.body, Some(unix_now()))
    }
}

/// Liest den Challenge-Schlüssel des Azure-Arc-Agenten: Pfad aus `WWW-Authenticate: Basic
/// realm=<Pfad>`, nur eine `.key`-Datei direkt im erlaubten Verzeichnis `dir` (nach Auflösen
/// symbolischer Links), höchstens [`ARC_MAX_KEY_FILE`] Byte. Liefert den `Authorization`-Wert.
/// Fehler nennen weder Inhalt noch Pfad (der Pfad steht nur im Log).
async fn arc_challenge(
    headers: &http::HeaderMap,
    dir: &std::path::Path,
) -> Result<HeaderValue, UpstreamError> {
    let fail = |why: &'static str| {
        token_error(
            ErrorClass::Auth,
            format!("Managed Identity (Azure Arc): {why}"),
            None,
        )
    };
    let realm = headers
        .get(WWW_AUTHENTICATE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            let (scheme, rest) = v.trim().split_once(' ')?;
            let rest = rest.trim();
            let value = rest
                .get(..6)
                .filter(|p| p.eq_ignore_ascii_case("realm="))
                .map(|_| &rest[6..])?;
            scheme
                .eq_ignore_ascii_case("basic")
                .then(|| value.trim_matches('"').to_owned())
        })
        .filter(|p| !p.is_empty())
        .ok_or_else(|| fail("Challenge ohne WWW-Authenticate: Basic realm=<Datei>"))?;
    let path = PathBuf::from(realm);
    let dir = dir.to_path_buf();
    let read = tokio::task::spawn_blocking(move || read_arc_key(&path, &dir))
        .await
        .map_err(|e| token_error(ErrorClass::Internal, e.to_string(), Some(500)))?;
    let key = read.map_err(fail)?;
    let mut value = HeaderValue::from_str(&format!("Basic {key}"))
        .map_err(|_| fail("Schlüsseldatei enthält unzulässige Zeichen"))?;
    value.set_sensitive(true);
    Ok(value)
}

/// Prüft und liest die Arc-Schlüsseldatei (blockierend).
fn read_arc_key(path: &std::path::Path, dir: &std::path::Path) -> Result<String, &'static str> {
    let reject = |why: &'static str| {
        tracing::warn!(file = %path.display(), allowed = %dir.display(), "Azure Arc: {why}");
        why
    };
    if !path.is_absolute() || path.extension().and_then(|e| e.to_str()) != Some("key") {
        return Err(reject(
            "Schlüsseldatei muss ein absoluter Pfad auf .key sein",
        ));
    }
    if path.parent() != Some(dir) {
        return Err(reject("Schlüsseldatei liegt nicht im Arc-Verzeichnis"));
    }
    // Symbolische Links auflösen: auch das Ziel muss direkt im (aufgelösten) Verzeichnis liegen.
    let real = std::fs::canonicalize(path).map_err(|_| reject("Schlüsseldatei nicht lesbar"))?;
    let real_dir =
        std::fs::canonicalize(dir).map_err(|_| reject("Arc-Verzeichnis nicht vorhanden"))?;
    if real.parent() != Some(real_dir.as_path()) {
        return Err(reject("Schlüsseldatei liegt nicht im Arc-Verzeichnis"));
    }
    let meta = std::fs::metadata(&real).map_err(|_| reject("Schlüsseldatei nicht lesbar"))?;
    if !meta.is_file() {
        return Err(reject("Schlüsseldatei ist keine reguläre Datei"));
    }
    if meta.len() > ARC_MAX_KEY_FILE {
        return Err(reject("Schlüsseldatei größer als 4 KiB"));
    }
    let content =
        std::fs::read_to_string(&real).map_err(|_| reject("Schlüsseldatei nicht lesbar"))?;
    let key = content.trim();
    if key.is_empty() {
        return Err(reject("Schlüsseldatei ist leer"));
    }
    Ok(key.to_owned())
}

impl TokenFetcher for EntraFetcher {
    fn label(&self) -> &'static str {
        "Entra ID"
    }

    fn client_for(&self, deployment: &UpstreamClient) -> Result<UpstreamClient, UpstreamError> {
        let Grant::ManagedIdentity(_) = &self.method else {
            return Ok(deployment.clone());
        };
        // Nur genau der Metadaten-Endpunkt (IMDS bzw. IDENTITY_ENDPOINT aus der
        // Prozessumgebung) wird von den SSRF-Regeln ausgenommen.
        let host = self.token_url.host().unwrap_or_default();
        deployment
            .outbound()
            .with_exempt_host(host)
            .map(UpstreamClient::from_outbound)
            .map_err(|e| {
                token_error(
                    ErrorClass::Internal,
                    format!("HTTP-Client für Managed Identity: {e}"),
                    Some(500),
                )
            })
    }

    fn fetch<'a>(&'a self, client: &'a UpstreamClient) -> FetchFuture<'a> {
        Box::pin(async move {
            let internal = |m: String| token_error(ErrorClass::Internal, m, Some(500));
            match &self.method {
                Grant::ClientSecret(secret) => {
                    let body = format!(
                        "{}&client_secret={}",
                        self.grant(),
                        uri_encode(secret.expose(), false)
                    );
                    post_token_form(client, &self.token_url, body).await
                }
                Grant::ClientCertificate(cert) => {
                    let assertion = cert
                        .assertion(self.client_id(), &self.token_url.to_string(), unix_now())
                        .map_err(internal)?;
                    post_token_form(client, &self.token_url, self.with_assertion(&assertion)).await
                }
                Grant::WorkloadIdentity(file) => {
                    // Das föderierte Token rotiert (kubelet): bei jedem Abruf neu lesen.
                    let path = file.clone();
                    let read = tokio::task::spawn_blocking(move || std::fs::read_to_string(path))
                        .await
                        .map_err(|e| internal(e.to_string()))?;
                    let assertion = read.map_err(|e| {
                        tracing::warn!(file = %file.display(), error = %e, "Workload Identity: Token-Datei nicht lesbar");
                        token_error(
                            ErrorClass::Auth,
                            "Workload Identity: föderiertes Token nicht lesbar",
                            None,
                        )
                    })?;
                    let assertion = assertion.trim();
                    if assertion.is_empty() {
                        return Err(token_error(
                            ErrorClass::Auth,
                            "Workload Identity: föderiertes Token ist leer",
                            None,
                        ));
                    }
                    post_token_form(client, &self.token_url, self.with_assertion(assertion)).await
                }
                Grant::ManagedIdentity(ep) => self.managed_identity(client, ep).await,
            }
        })
    }
}

/// Tenant- bzw. Client-ID (GUID oder Domänenname): landet in URL-Pfad bzw. Formular.
fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 256
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_')
        && s != "."
        && s != ".."
}

/// Merkmale einer Token-Quelle als Cache-Schlüssel (SHA-256; enthält Geheimnisse nur gehasht).
fn fingerprint(parts: &[&[u8]]) -> [u8; 32] {
    let mut ctx = digest::Context::new(&digest::SHA256);
    for p in parts {
        ctx.update(&(p.len() as u64).to_be_bytes());
        ctx.update(p);
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(ctx.finish().as_ref());
    out
}

static SOURCES: LazyLock<Mutex<HashMap<[u8; 32], Weak<EntraTokenSource>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Gemeinsame Token-Quelle für `key` (eine je Credential), sonst `make()`.
fn shared(key: [u8; 32], make: impl FnOnce() -> EntraTokenSource) -> Arc<EntraTokenSource> {
    let mut map = SOURCES.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(src) = map.get(&key).and_then(Weak::upgrade) {
        return src;
    }
    map.retain(|_, w| w.strong_count() > 0);
    let src = Arc::new(make());
    map.insert(key, Arc::downgrade(&src));
    src
}

/// Baut (bzw. teilt) die Entra-ID-Token-Quelle eines Azure-Deployments mit
/// `settings.auth: entraId` und den Werten der Prozessumgebung; Workload/Managed Identity nur
/// mit [`set_ambient_identity`]`(true)`.
pub fn token_source(dep: &DeploymentCtx) -> Result<Arc<EntraTokenSource>, ConfigError> {
    static ENV: LazyLock<EntraEnv> = LazyLock::new(EntraEnv::from_process);
    if ambient_identity() {
        let env = EntraEnv {
            ambient_identity: true,
            ..ENV.clone()
        };
        token_source_with_env(dep, &env)
    } else {
        token_source_with_env(dep, &ENV)
    }
}

/// Wie [`token_source`], mit vorgegebener Umgebung (Tests).
pub fn token_source_with_env(
    dep: &DeploymentCtx,
    env: &EntraEnv,
) -> Result<Arc<EntraTokenSource>, ConfigError> {
    let s = Settings::of(dep);
    let cred = |name: &str| dep.credential.as_ref().and_then(|c| c.get(name));
    let secret = cred("clientSecret");
    let certificate = cred("clientCertificate");
    let method = match s.str("entraCredential")? {
        Some(m) => m,
        None if secret.is_some() => "clientSecret",
        None if certificate.is_some() => "clientCertificate",
        None => {
            return Err(s.missing(
                "settings.entraCredential (clientSecret, clientCertificate, workloadIdentity, \
                 managedIdentity) bzw. Zugangsdaten clientSecret/clientCertificate",
            ));
        }
    };
    let ambient = matches!(method, "workloadIdentity" | "managedIdentity");
    let scope = s.str("entraScope")?.unwrap_or(COGNITIVE_SERVICES_SCOPE);
    if scope.is_empty() || scope.len() > 512 || scope.chars().any(|c| c.is_control()) {
        return Err(s.invalid("settings.entraScope ungültig"));
    }
    if ambient && !AMBIENT_SCOPES.contains(&scope) {
        return Err(s.invalid(format!(
            "settings.entraScope: bei {method} nur {}",
            AMBIENT_SCOPES.join(", ")
        )));
    }
    // AIGW-90: die Identität des Gateway-Prozesses nur mit ausdrücklicher Freigabe.
    if ambient && !env.ambient_identity {
        return Err(s.invalid(format!(
            "{method} nutzt die Identität des Gateway-Prozesses und ist nicht freigegeben – \
             in der Startkonfiguration {AMBIENT_IDENTITY_KEY}: true setzen \
             (ENV AIGW_UPSTREAM__ENTRA__AMBIENT_IDENTITY=true)"
        )));
    }
    // IDs: Zugangsdaten vor Einstellungen vor Prozessumgebung (letztere nur für Workload
    // Identity, deren Webhook sie setzt).
    let id = |cred_field: &str, setting: &str, from_env: Option<&String>| {
        let v = cred(cred_field)
            .map(|v| v.expose().trim().to_owned())
            .or(s.str(setting)?.map(str::to_owned))
            .or_else(|| {
                (method == "workloadIdentity")
                    .then(|| from_env.cloned())
                    .flatten()
            });
        match v {
            Some(v) if !valid_id(&v) => Err(s.invalid(format!(
                "{cred_field} ungültig (GUID oder Domänenname erwartet)"
            ))),
            v => Ok(v),
        }
    };
    let tenant = id("tenantId", "entraTenantId", env.tenant_id.as_ref())?;
    let client_id = id("clientId", "entraClientId", env.client_id.as_ref())?;

    let authority = if method == "workloadIdentity" {
        if s.str("entraAuthority")?.is_some() {
            return Err(s.invalid(
                "settings.entraAuthority: bei workloadIdentity nicht erlaubt \
                 (Authority kommt aus AZURE_AUTHORITY_HOST)",
            ));
        }
        env.authority_host.as_deref().unwrap_or(DEFAULT_AUTHORITY)
    } else {
        s.str("entraAuthority")?.unwrap_or(DEFAULT_AUTHORITY)
    };
    let token_url = |tenant: &str| -> Result<Uri, ConfigError> {
        let url = format!(
            "{}/{tenant}/oauth2/v2.0/token",
            authority.trim_end_matches('/')
        );
        let uri = crate::openai::parse_uri(&s, &url)?;
        if uri.query().is_some() || uri.path() != format!("/{tenant}/oauth2/v2.0/token") {
            return Err(s.invalid(
                "settings.entraAuthority: nur Schema und Host (z. B. https://login.microsoftonline.us)",
            ));
        }
        Ok(uri)
    };
    let need = |v: Option<String>, what: &'static str| v.ok_or_else(|| s.missing(what));

    if method != "clientCertificate" {
        if s.bool("entraSendX5c")?.is_some() {
            return Err(s.invalid(format!(
                "settings.entraSendX5c: nur bei clientCertificate (nicht {method})"
            )));
        }
        if cred("clientCertificatePassword").is_some() {
            return Err(s.invalid(format!(
                "Zugangsdaten clientCertificatePassword: nur bei clientCertificate (nicht {method})"
            )));
        }
    }
    let (fetcher, key) = match method {
        "clientSecret" => {
            let secret = secret.ok_or_else(|| s.missing("Zugangsdaten (clientSecret)"))?;
            let tenant = need(tenant, "Zugangsdaten (tenantId)")?;
            let client_id = need(client_id, "Zugangsdaten (clientId)")?;
            let url = token_url(&tenant)?;
            let key = fingerprint(&[
                b"secret",
                url.to_string().as_bytes(),
                client_id.as_bytes(),
                scope.as_bytes(),
                secret.expose().as_bytes(),
            ]);
            (
                EntraFetcher {
                    method: Grant::ClientSecret(secret.clone()),
                    token_url: url,
                    client_id: Some(client_id),
                    scope: scope.to_owned(),
                },
                key,
            )
        }
        "clientCertificate" => {
            let pem = certificate.ok_or_else(|| s.missing("Zugangsdaten (clientCertificate)"))?;
            let password = cred("clientCertificatePassword");
            let send_x5c = s.bool("entraSendX5c")?.unwrap_or(false);
            let cert = ClientCertificate::from_pem(
                pem.expose(),
                password.map(SecretString::expose),
                send_x5c,
            )
            .map_err(|e| s.invalid(e))?;
            let tenant = need(tenant, "Zugangsdaten (tenantId)")?;
            let client_id = need(client_id, "Zugangsdaten (clientId)")?;
            let url = token_url(&tenant)?;
            let key = fingerprint(&[
                b"certificate",
                url.to_string().as_bytes(),
                client_id.as_bytes(),
                scope.as_bytes(),
                pem.expose().as_bytes(),
                password
                    .map(SecretString::expose)
                    .unwrap_or_default()
                    .as_bytes(),
                &[u8::from(send_x5c)],
            ]);
            (
                EntraFetcher {
                    method: Grant::ClientCertificate(cert),
                    token_url: url,
                    client_id: Some(client_id),
                    scope: scope.to_owned(),
                },
                key,
            )
        }
        "workloadIdentity" => {
            let file = env
                .federated_token_file
                .clone()
                .ok_or_else(|| s.missing("AZURE_FEDERATED_TOKEN_FILE (Workload Identity)"))?;
            let tenant = need(
                tenant,
                "tenantId (Zugangsdaten, settings.entraTenantId oder AZURE_TENANT_ID)",
            )?;
            let client_id = need(
                client_id,
                "clientId (Zugangsdaten, settings.entraClientId oder AZURE_CLIENT_ID)",
            )?;
            let url = token_url(&tenant)?;
            let key = fingerprint(&[
                b"workload",
                url.to_string().as_bytes(),
                client_id.as_bytes(),
                scope.as_bytes(),
                file.to_string_lossy().as_bytes(),
            ]);
            (
                EntraFetcher {
                    method: Grant::WorkloadIdentity(file),
                    token_url: url,
                    client_id: Some(client_id),
                    scope: scope.to_owned(),
                },
                key,
            )
        }
        "managedIdentity" => {
            let (endpoint, header_digest) = match (&env.identity_endpoint, &env.identity_header) {
                (Some(ep), None) if env.arc_imds_endpoint.is_some() => {
                    // Azure Arc: nur systemzugewiesene Identität.
                    if client_id.is_some() {
                        return Err(s.invalid(
                            "Managed Identity über Azure Arc: benutzerzugewiesene Identität \
                             (clientId) wird nicht unterstützt",
                        ));
                    }
                    let dir = env
                        .arc_token_dir
                        .clone()
                        .unwrap_or_else(|| PathBuf::from(ARC_TOKEN_DIR));
                    (
                        MiEndpoint::Arc(crate::openai::parse_uri(&s, ep)?, dir),
                        "arc".to_owned(),
                    )
                }
                (Some(ep), Some(h)) => {
                    let uri = crate::openai::parse_uri(&s, ep)?;
                    let mut hv = HeaderValue::from_str(h.expose())
                        .map_err(|_| s.invalid("IDENTITY_HEADER enthält unzulässige Zeichen"))?;
                    hv.set_sensitive(true);
                    (
                        MiEndpoint::AppService(uri, hv),
                        codec::hex(digest::digest(&digest::SHA256, h.expose().as_bytes()).as_ref()),
                    )
                }
                _ => {
                    let ep = env.imds_endpoint.as_deref().unwrap_or(IMDS_ENDPOINT);
                    (
                        MiEndpoint::Imds(crate::openai::parse_uri(&s, ep)?),
                        String::new(),
                    )
                }
            };
            let url = match &endpoint {
                MiEndpoint::Imds(u) | MiEndpoint::AppService(u, _) | MiEndpoint::Arc(u, _) => {
                    u.clone()
                }
            };
            if url.query().is_some() {
                return Err(s.invalid("Managed-Identity-Endpunkt darf keine Abfrage enthalten"));
            }
            let key = fingerprint(&[
                b"managed",
                url.to_string().as_bytes(),
                client_id.as_deref().unwrap_or_default().as_bytes(),
                scope.as_bytes(),
                header_digest.as_bytes(),
            ]);
            (
                EntraFetcher {
                    method: Grant::ManagedIdentity(endpoint),
                    token_url: url,
                    client_id,
                    scope: scope.to_owned(),
                },
                key,
            )
        }
        other => {
            return Err(s.invalid(format!(
                "settings.entraCredential = {other:?} unbekannt (clientSecret, \
                 clientCertificate, workloadIdentity, managedIdentity)"
            )));
        }
    };
    Ok(shared(key, || RefreshingToken::with_fetcher(fetcher)))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use aws_lc_rs::encoding::AsDer as _;
    use aws_lc_rs::rsa::KeySize;
    use aws_lc_rs::signature::{KeyPair as _, RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};
    use serde_json::Value;

    use super::*;
    use crate::codec::base64;

    fn test_pem() -> (String, Vec<u8>, Vec<u8>) {
        let kp = RsaKeyPair::generate(KeySize::Rsa2048).unwrap();
        let der = kp.as_der().unwrap();
        // Kein echtes X.509 nötig: x5t ist der SHA-1 über die DER-Bytes.
        let cert = vec![0x30, 0x03, 0x02, 0x01, 0x07];
        let pem = format!(
            "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n\
             -----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            base64(&cert),
            base64(der.as_ref())
        );
        (pem, cert, kp.public_key().as_ref().to_vec())
    }

    #[test]
    fn client_assertion_is_valid_rs256_with_x5t() {
        let (pem, cert, public) = test_pem();
        let c = ClientCertificate::from_pem(&pem, None, false).unwrap();
        let jwt = c
            .assertion(
                "cid",
                "https://login.example/t/oauth2/v2.0/token",
                1_700_000_000,
            )
            .unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        let header: Value = serde_json::from_slice(&base64_decode(parts[0]).unwrap()).unwrap();
        assert_eq!(header["alg"], "RS256");
        let thumb = digest::digest(&digest::SHA1_FOR_LEGACY_USE_ONLY, &cert);
        assert_eq!(header["x5t"], base64url(thumb.as_ref()));
        let claims: Value = serde_json::from_slice(&base64_decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["iss"], "cid");
        assert_eq!(claims["sub"], "cid");
        assert_eq!(claims["aud"], "https://login.example/t/oauth2/v2.0/token");
        assert_eq!(claims["exp"], 1_700_000_600u64);
        assert!(claims["jti"].as_str().is_some_and(|j| j.len() >= 16));
        let pk = UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, public);
        pk.verify(
            format!("{}.{}", parts[0], parts[1]).as_bytes(),
            &base64_decode(parts[2]).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn rejects_bad_certificates() {
        assert!(ClientCertificate::from_pem("kein pem", None, false).is_err());
        let (pem, _, _) = test_pem();
        let only_cert = pem.split("-----BEGIN PRIVATE KEY").next().unwrap();
        assert!(ClientCertificate::from_pem(only_cert, None, false).is_err());
        let enc = "-----BEGIN CERTIFICATE-----\nMAMCAQc=\n-----END CERTIFICATE-----\n\
                   -----BEGIN ENCRYPTED PRIVATE KEY-----\nAAAA\n-----END ENCRYPTED PRIVATE KEY-----";
        // Ohne Passwort: Hinweis auf das Feld; mit Passwort: Strukturfehler ohne Geheimnis.
        let e = ClientCertificate::from_pem(enc, None, false).unwrap_err();
        assert!(e.contains("clientCertificatePassword"), "{e}");
        let e = ClientCertificate::from_pem(enc, Some("pw-geheim"), false).unwrap_err();
        assert!(!e.contains("pw-geheim"), "{e}");
        // Passwort zu unverschlüsseltem Schlüssel: Konfigurationsfehler.
        let e = ClientCertificate::from_pem(&pem, Some("pw"), false).unwrap_err();
        assert!(e.contains("nicht verschlüsselt"), "{e}");
    }

    #[test]
    fn x5c_carries_the_chain_in_order() {
        let (pem, cert, _) = test_pem();
        let second = vec![0x30, 0x03, 0x02, 0x01, 0x08];
        let chain = format!(
            "{pem}-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
            base64(&second)
        );
        let c = ClientCertificate::from_pem(&chain, None, true).unwrap();
        let jwt = c.assertion("cid", "https://login.example/t", 1).unwrap();
        let header: Value =
            serde_json::from_slice(&base64_decode(jwt.split('.').next().unwrap()).unwrap())
                .unwrap();
        assert_eq!(header["x5c"], json!([base64(&cert), base64(&second)]));
        // x5t bleibt der Fingerabdruck des Blattzertifikats.
        let thumb = digest::digest(&digest::SHA1_FOR_LEGACY_USE_ONLY, &cert);
        assert_eq!(header["x5t"], base64url(thumb.as_ref()));
        let without = ClientCertificate::from_pem(&chain, None, false).unwrap();
        let jwt = without
            .assertion("cid", "https://login.example/t", 1)
            .unwrap();
        assert!(
            !String::from_utf8(base64_decode(jwt.split('.').next().unwrap()).unwrap())
                .unwrap()
                .contains("x5c")
        );
    }

    #[test]
    fn arc_key_file_rules() {
        let base = std::env::temp_dir().join(format!("aigw-arc-unit-{}", std::process::id()));
        let dir = base.join("tokens");
        let outside = base.join("anderswo");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(dir.join("ok.key"), "geheim-123\n").unwrap();
        assert_eq!(
            read_arc_key(&dir.join("ok.key"), &dir).unwrap(),
            "geheim-123"
        );
        // Falsche Endung, relativer Pfad, anderes Verzeichnis, Traversal, zu groß, leer.
        std::fs::write(dir.join("x.txt"), "a").unwrap();
        assert!(read_arc_key(&dir.join("x.txt"), &dir).is_err());
        assert!(read_arc_key(std::path::Path::new("ok.key"), &dir).is_err());
        std::fs::write(outside.join("o.key"), "a").unwrap();
        assert!(read_arc_key(&outside.join("o.key"), &dir).is_err());
        assert!(read_arc_key(&dir.join("..").join("anderswo").join("o.key"), &dir).is_err());
        std::fs::write(dir.join("big.key"), "a".repeat(5000)).unwrap();
        assert!(read_arc_key(&dir.join("big.key"), &dir).is_err());
        std::fs::write(dir.join("empty.key"), " \n").unwrap();
        assert!(read_arc_key(&dir.join("empty.key"), &dir).is_err());
        std::fs::create_dir_all(dir.join("d.key")).unwrap();
        assert!(read_arc_key(&dir.join("d.key"), &dir).is_err());
        // Symbolischer Link im Verzeichnis, der hinaus zeigt.
        #[cfg(unix)]
        {
            let link = dir.join("link.key");
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(outside.join("o.key"), &link).unwrap();
            assert!(read_arc_key(&link, &dir).is_err());
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn ids_are_checked() {
        assert!(valid_id("72f988bf-86f1-41af-91ab-2d7cd011db47"));
        assert!(valid_id("contoso.onmicrosoft.com"));
        assert!(!valid_id("../x"));
        assert!(!valid_id("a/b"));
        assert!(!valid_id(""));
    }
}
