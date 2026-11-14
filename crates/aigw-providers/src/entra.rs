// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Microsoft Entra ID als [`TokenSource`](crate::auth::TokenSource) für Azure OpenAI und Claude in
//! Azure AI Foundry (`provider: anthropic`)
//! (`settings.auth: entraId`, AIGW-82).
//!
//! | `settings.entraCredential` | Ablauf | Herkunft |
//! |---|---|---|
//! | `clientSecret` | OAuth 2.0 Client Credentials mit `client_secret` | Zugangsdaten `tenantId`, `clientId`, `clientSecret` |
//! | `clientCertificate` | Client Credentials mit signierter JWT-Assertion (RS256, `x5t`) | Zugangsdaten `tenantId`, `clientId`, `clientCertificate` (PEM: Zertifikat + privater RSA-Schlüssel) |
//! | `workloadIdentity` | Client Credentials mit föderiertem Token (`client_assertion` aus `AZURE_FEDERATED_TOKEN_FILE`) | Prozessumgebung (AKS), `tenantId`/`clientId` auch aus Zugangsdaten/Einstellungen |
//! | `managedIdentity` | Azure-Metadatendienst (IMDS `169.254.169.254`) bzw. App Service (`IDENTITY_ENDPOINT`) | optional `clientId` (benutzerzugewiesen) |
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
use std::sync::{Arc, LazyLock, Mutex, PoisonError, Weak};

use aigw_core::ir::{ErrorClass, UpstreamError};
use aigw_core::resource::SecretString;
use aigw_core::snapshot::DeploymentCtx;
use aws_lc_rs::digest;
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use bytes::Bytes;
use http::header::ACCEPT;
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
        }
    }
}

/// Zertifikat (DER des ersten `CERTIFICATE`-Blocks) und privater RSA-Schlüssel.
struct ClientCertificate {
    /// `x5t`: Base64url(SHA-1(DER)) – von Entra zur Zuordnung des Zertifikats verlangt.
    x5t: String,
    key: RsaKeyPair,
}

impl fmt::Debug for ClientCertificate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientCertificate")
            .field("x5t", &self.x5t)
            .finish_non_exhaustive()
    }
}

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
    fn from_pem(pem: &str) -> Result<Self, String> {
        let pem = pem.replace("\\n", "\n");
        if pem.contains("-----BEGIN ENCRYPTED PRIVATE KEY-----") {
            return Err(
                "clientCertificate: verschlüsselte Schlüssel werden nicht unterstützt \
                 (PEM ohne Passwort ablegen)"
                    .into(),
            );
        }
        let cert = pem_blocks(&pem, "CERTIFICATE")?
            .into_iter()
            .next()
            .ok_or("clientCertificate: kein CERTIFICATE-Block")?;
        if cert.first() != Some(&0x30) {
            return Err("clientCertificate: Zertifikat ist kein DER-Objekt".into());
        }
        let key = if let Some(der) = pem_blocks(&pem, "PRIVATE KEY")?.into_iter().next() {
            RsaKeyPair::from_pkcs8(&der)
        } else if let Some(der) = pem_blocks(&pem, "RSA PRIVATE KEY")?.into_iter().next() {
            RsaKeyPair::from_der(&der)
        } else {
            return Err(
                "clientCertificate: kein privater Schlüssel (PRIVATE KEY bzw. RSA PRIVATE KEY)"
                    .into(),
            );
        }
        .map_err(|_| "clientCertificate: privater Schlüssel ist kein gültiger RSA-Schlüssel")?;
        let thumb = digest::digest(&digest::SHA1_FOR_LEGACY_USE_ONLY, &cert);
        Ok(Self {
            x5t: base64url(thumb.as_ref()),
            key,
        })
    }

    /// Signierte Client-Assertion (RFC 7523) für `client_id` an `audience` (Token-URL).
    fn assertion(&self, client_id: &str, audience: &str, now: u64) -> Result<String, String> {
        let header = json!({ "alg": "RS256", "typ": "JWT", "x5t": self.x5t });
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
        };
        let mut url = format!(
            "{base}?api-version={version}&resource={}",
            uri_encode(resource, false)
        );
        if let Some(id) = &self.client_id {
            url.push_str("&client_id=");
            url.push_str(&uri_encode(id, false));
        }
        let mut req = http::Request::builder()
            .method(Method::GET)
            .uri(url)
            .header(ACCEPT, HeaderValue::from_static("application/json"));
        req = match endpoint {
            MiEndpoint::Imds(_) => req.header("metadata", HeaderValue::from_static("true")),
            MiEndpoint::AppService(_, h) => {
                req.header(HeaderName::from_static("x-identity-header"), h.clone())
            }
        };
        let req = req
            .body(Bytes::new())
            .map_err(|e| token_error(ErrorClass::Internal, e.to_string(), Some(500)))?;
        let reply = send_token_request(client, req).await?;
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
/// `settings.auth: entraId` und den Werten der Prozessumgebung.
pub fn token_source(dep: &DeploymentCtx) -> Result<Arc<EntraTokenSource>, ConfigError> {
    static ENV: LazyLock<EntraEnv> = LazyLock::new(EntraEnv::from_process);
    token_source_with_env(dep, &ENV)
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
            let cert = ClientCertificate::from_pem(pem.expose()).map_err(|e| s.invalid(e))?;
            let tenant = need(tenant, "Zugangsdaten (tenantId)")?;
            let client_id = need(client_id, "Zugangsdaten (clientId)")?;
            let url = token_url(&tenant)?;
            let key = fingerprint(&[
                b"certificate",
                url.to_string().as_bytes(),
                client_id.as_bytes(),
                scope.as_bytes(),
                pem.expose().as_bytes(),
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
                MiEndpoint::Imds(u) | MiEndpoint::AppService(u, _) => u.clone(),
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
        let c = ClientCertificate::from_pem(&pem).unwrap();
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
        assert!(ClientCertificate::from_pem("kein pem").is_err());
        let (pem, _, _) = test_pem();
        let only_cert = pem.split("-----BEGIN PRIVATE KEY").next().unwrap();
        assert!(ClientCertificate::from_pem(only_cert).is_err());
        assert!(
            ClientCertificate::from_pem(
                "-----BEGIN CERTIFICATE-----\nMAMCAQc=\n-----END CERTIFICATE-----\n\
                 -----BEGIN ENCRYPTED PRIVATE KEY-----\nAAAA\n-----END ENCRYPTED PRIVATE KEY-----"
            )
            .unwrap_err()
            .contains("verschlüsselt")
        );
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
