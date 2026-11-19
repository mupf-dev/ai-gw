// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Ressourcen der Admin-API mit den generierten Typen (docs/konzept/08-admin-api.md §2).
import { action, etagFor, listAll, patch, resource, unwrap, unwrapVersioned, type Schemas, type Versioned, api } from './client';

export type S = Schemas;
export type Organization = S['Organization'];
export type Team = S['Team'];
export type User = S['User'];
export type ApiKey = S['ApiKey'];
export type ModelGroup = S['ModelGroup'];
export type Deployment = S['Deployment'];
export type Credential = S['ProviderCredential'];
export type Price = S['Price'];
export type AdminApiKey = S['AdminApiKey'];
export type RoleBinding = S['RoleBinding'];
export type Role = S['Role'];
export type ProviderKind = S['ProviderKind'];

export const organizations = resource<Organization, S['OrganizationInput'], { q?: string }>('organizations');
export const teams = resource<Team, S['TeamInput'], { q?: string; organization?: string }>('teams');
export const users = resource<User, S['UserInput'], { q?: string; email?: string }>('users');
export const modelGroups = resource<ModelGroup, S['ModelGroupInput'], { q?: string }>('model-groups');
export const deployments = resource<Deployment, S['DeploymentInput'], { q?: string; modelGroup?: string; provider?: string; credential?: string; enabled?: boolean }>(
  'deployments',
);
export const prices = resource<Price, S['PriceInput'], { q?: string; model?: string; provider?: string; effectiveAt?: string }>('prices');

const keysBase = resource<ApiKey, S['ApiKeyInput'], { q?: string; team?: string; user?: string; organization?: string; state?: S['ApiKeyState'] }>(
  'api-keys',
);
export const apiKeys = {
  ...keysBase,
  /** Anlegen: Klartext nur in dieser Antwort. */
  createWithSecret: (input: S['ApiKeyInput']) => unwrapVersioned<S['CreatedApiKey']>(api.POST('/admin/api/v1/api-keys', { body: input })),
  /** Vorübergehend sperren (F37, umkehrbar mit `resume`). */
  suspend: (name: string, etag?: string | null) => action<ApiKey>('api-keys', name, 'suspend', undefined, etag),
  /** Vorübergehende Sperre aufheben (F37). */
  resume: (name: string, etag?: string | null) => action<ApiKey>('api-keys', name, 'resume', undefined, etag),
  /** Endgültig widerrufen (F37) – nicht umkehrbar. */
  revokePermanently: (name: string, etag?: string | null) => action<ApiKey>('api-keys', name, 'revoke', undefined, etag),
  rotate: (name: string, body: S['RotateApiKeyRequest'], etag?: string | null) => action<S['RotatedApiKey']>('api-keys', name, 'rotate', body, etag),
};

const credBase = resource<Credential, S['ProviderCredentialInput'], { q?: string; provider?: string }>('provider-credentials');
export const credentials = {
  ...credBase,
  patch: (name: string, body: S['ProviderCredentialPatch'], etag?: string | null) => patch<Credential>('provider-credentials', name, body, etag),
};

const ADMIN_KEY = '/admin/api/v1/admin-api-keys/{name}';
export const adminKeys = {
  list: async () => (await unwrap<S['AdminApiKeyList']>(api.GET('/admin/api/v1/admin-api-keys'))).items,
  create: (input: S['AdminApiKeyInput']) => unwrap<S['CreatedAdminApiKey']>(api.POST('/admin/api/v1/admin-api-keys', { body: input })),
  get: (name: string) => unwrapVersioned<AdminApiKey>(api.GET('/admin/api/v1/admin-api-keys/{name}', { params: { path: { name } } })),
  update: async (name: string, input: S['AdminApiKeyUpdate'], etag?: string | null) =>
    unwrap<AdminApiKey>(
      api.PUT('/admin/api/v1/admin-api-keys/{name}', {
        params: { path: { name }, header: { 'If-Match': await etagFor(ADMIN_KEY, name, etag) } },
        body: input,
      }),
    ),
  remove: async (name: string, etag?: string | null) =>
    unwrap<void>(
      api.DELETE('/admin/api/v1/admin-api-keys/{name}', {
        params: { path: { name }, header: { 'If-Match': await etagFor(ADMIN_KEY, name, etag) } },
      }),
    ),
};

export type { Versioned };
export { listAll };

// ------------------------------------------------------------------ Anbieter

export const PROVIDERS: { value: ProviderKind; label: string; endpoint: 'required' | 'optional' | 'none' }[] = [
  { value: 'openai', label: 'OpenAI', endpoint: 'optional' },
  { value: 'azure-openai', label: 'Azure OpenAI', endpoint: 'required' },
  { value: 'anthropic', label: 'Anthropic', endpoint: 'optional' },
  { value: 'bedrock', label: 'AWS Bedrock', endpoint: 'optional' },
  { value: 'vertex', label: 'Google Vertex AI', endpoint: 'optional' },
  { value: 'mistral', label: 'Mistral AI', endpoint: 'optional' },
  { value: 'ollama', label: 'Ollama', endpoint: 'optional' },
  { value: 'vllm', label: 'vLLM', endpoint: 'required' },
  { value: 'openai-compatible', label: 'OpenAI-kompatibel', endpoint: 'required' },
  { value: 'mock', label: 'Mock (Test)', endpoint: 'none' },
];
export const providerLabel = (p: string | null | undefined) => (p ? (PROVIDERS.find((x) => x.value === p)?.label ?? p) : 'alle Anbieter');

/** Entra-ID-Anmeldearten (AIGW-84) für Zugangsdaten von Azure OpenAI und Claude in Azure AI Foundry. */
export const ENTRA_KINDS: { value: S['EntraCredentialKind']; label: string; hint: string }[] = [
  { value: 'managedIdentity', label: 'Managed Identity', hint: 'Identität des Gateways (Container Apps, App Service, VM, Azure Arc); optional Client-ID einer benutzerzugewiesenen Identität' },
  { value: 'workloadIdentity', label: 'Workload Identity', hint: 'AKS: föderiertes Token aus AZURE_FEDERATED_TOKEN_FILE' },
  { value: 'clientSecret', label: 'Service Principal (Secret)', hint: 'Tenant-ID, Client-ID und Client-Secret' },
  { value: 'clientCertificate', label: 'Service Principal (Zertifikat)', hint: 'Tenant-ID, Client-ID und PEM mit Zertifikat + privatem RSA-Schlüssel (auch verschlüsselt mit Passwort)' },
];
export const entraLabel = (k: string | null | undefined) => ENTRA_KINDS.find((e) => e.value === k)?.label ?? k ?? 'Entra ID';

type CredAlt = { label: string; fields: string[]; optional?: string[]; entra?: S['EntraCredentialKind'] };
const ENTRA_ALTS: CredAlt[] = [
  { label: 'Entra ID – Managed Identity', fields: [], entra: 'managedIdentity' },
  { label: 'Entra ID – Workload Identity', fields: [], entra: 'workloadIdentity' },
  { label: 'Entra ID – Service Principal (Secret)', fields: ['clientSecret'], entra: 'clientSecret' },
  { label: 'Entra ID – Service Principal (Zertifikat)', fields: ['clientCertificate'], optional: ['clientCertificatePassword'], entra: 'clientCertificate' },
];

/**
 * Pflichtfelder der Zugangsdaten je Anbieter (08-admin-api.md §2.4): eine Alternative muss vollständig sein.
 * Leere Liste = beliebige Felder. `entra` = Anmeldung über Entra ID (`spec.entra`, nicht geheime IDs als Klartext).
 */
export const CREDENTIAL_FIELDS: Record<ProviderKind, { alternatives: CredAlt[]; optional: string[] }> = {
  openai: { alternatives: [{ label: 'API-Key', fields: ['apiKey'] }], optional: ['organization', 'project'] },
  anthropic: { alternatives: [{ label: 'API-Key', fields: ['apiKey'] }, ...ENTRA_ALTS], optional: [] },
  mistral: { alternatives: [{ label: 'API-Key', fields: ['apiKey'] }], optional: [] },
  'azure-openai': { alternatives: [{ label: 'API-Key', fields: ['apiKey'] }, ...ENTRA_ALTS], optional: [] },
  bedrock: {
    alternatives: [
      { label: 'IAM-Zugangsschlüssel', fields: ['accessKeyId', 'secretAccessKey'] },
      { label: 'API-Key', fields: ['apiKey'] },
    ],
    optional: ['sessionToken'],
  },
  vertex: {
    alternatives: [
      { label: 'Service-Account-JSON', fields: ['serviceAccountJson'] },
      { label: 'Client-E-Mail + Private Key', fields: ['clientEmail', 'privateKey'] },
      { label: 'Access-Token', fields: ['accessToken'] },
      { label: 'API-Key', fields: ['apiKey'] },
    ],
    optional: [],
  },
  ollama: { alternatives: [], optional: ['apiKey'] },
  vllm: { alternatives: [], optional: ['apiKey'] },
  'openai-compatible': { alternatives: [], optional: ['apiKey'] },
  mock: { alternatives: [], optional: ['apiKey'] },
};
export const LONG_FIELDS = new Set(['serviceAccountJson', 'privateKey', 'clientCertificate']);
/** Feldnamen: Buchstabe, dann Buchstaben/Ziffern/_ (max. 64). */
export const FIELD_NAME = /^[A-Za-z][A-Za-z0-9_]{0,63}$/;

export const ROUTING: { value: S['RoutingStrategy']; label: string; hint: string }[] = [
  { value: 'weighted', label: 'Gewichtet', hint: 'Verteilung nach Gewicht (Standard)' },
  { value: 'least-latency', label: 'Geringste Latenz', hint: 'schnellstes Deployment zuerst' },
  { value: 'least-busy', label: 'Geringste Last', hint: 'wenigste laufende Anfragen' },
  { value: 'cost', label: 'Kosten', hint: 'günstigstes Deployment zuerst' },
];
export const FALLBACK_TRIGGERS: { value: S['FallbackTrigger']; label: string }[] = [
  { value: 'any', label: 'jeder Fehler' },
  { value: 'rate-limit', label: 'Ratenlimit' },
  { value: 'unavailable', label: 'nicht verfügbar' },
  { value: 'context-length', label: 'Kontextlänge' },
  { value: 'content-filter', label: 'Inhaltsfilter' },
];

/** Name laut ObjectMeta: `a-z0-9-._`, max. 128 Zeichen. */
export const NAME_RE = /^[a-z0-9][a-z0-9._-]{0,127}$/;
export const NAME_HINT = 'Kleinbuchstaben, Ziffern, „-“, „.“, „_“; nicht änderbar.';

// ------------------------------------------------------------------ AIGW-78: Secrets, MCP, Plugin-Bindungen

export type Secret = S['Secret'];
const secretBase = resource<Secret, S['SecretInput'], { q?: string }>('secrets');
/** Secrets (F149): Werte nur schreibbar (PUT ersetzt alle, PATCH setzt/entfernt einzelne Felder). */
export const secrets = {
  ...secretBase,
  patch: (name: string, body: S['SecretPatch'], etag?: string | null) => patch<Secret>('secrets', name, body, etag),
};
export type McpServer = S['McpServer'];
export const mcpServers = resource<McpServer, S['McpServerInput'], { q?: string; transport?: string; enabled?: boolean }>('mcp-servers');
/** Anbieter-Übersicht je Anbieter und Endpunkt mit Anmeldeart und Laufzeitzustand (GET /providers, AIGW-84). */
export const providerOverview = () => unwrap<S['ProviderOverview']>(api.GET('/admin/api/v1/providers'));
export type ProviderEndpoint = S['ProviderEndpoint'];
export type AuthUse = S['AuthUse'];
const AUTH_LABEL: Record<S['AuthKind'], string> = {
  apiKey: 'API-Key',
  entraId: 'Entra ID',
  awsAccessKey: 'AWS-Zugangsschlüssel',
  serviceAccount: 'Service-Account',
  accessToken: 'Access-Token',
  credential: 'Zugangsdaten',
  none: 'ohne Anmeldung',
};
/** Anmeldeart als Text, z. B. „Entra ID · Managed Identity“. */
export const authLabel = (a: Pick<AuthUse, 'kind' | 'entraCredential'>) =>
  a.kind === 'entraId' && a.entraCredential ? `${AUTH_LABEL.entraId} · ${entraLabel(a.entraCredential)}` : AUTH_LABEL[a.kind];
export const HEALTH: Record<S['HealthState'], { label: string; tone: 'success' | 'warning' | 'danger' | 'neutral' | 'outline' }> = {
  ok: { label: 'in Ordnung', tone: 'success' },
  degraded: { label: 'gestört', tone: 'warning' },
  down: { label: 'ausgefallen', tone: 'danger' },
  unusable: { label: 'nicht nutzbar', tone: 'danger' },
  disabled: { label: 'abgeschaltet', tone: 'outline' },
  unknown: { label: 'unbekannt', tone: 'neutral' },
};

/** Verbindungszustand der MCP-Upstreams (GET /mcp-servers:status). */
export const mcpStatus = () => unwrap<S['McpStatusList']>(api.GET('/admin/api/v1/mcp-servers:status'));
export type PluginBinding = S['PluginBinding'];
export const pluginBindingsRes = resource<PluginBinding, S['PluginBindingInput'], { q?: string; plugin?: string; level?: string; target?: string }>(
  'plugin-bindings',
);

// ------------------------------------------------------------------ AIGW-52: Editoren (Plugins, MCP-Freigaben)

export type McpGrant = S['McpGrant'];
export const mcpGrants = resource<McpGrant, S['McpGrantInput'], { q?: string; server?: string; team?: string; apiKey?: string }>('mcp-grants');

// ------------------------------------------------------------------ Tools (AIGW-87)

export type ToolItem = S['ToolItem'];
export type ToolPolicy = S['ToolPolicy'];
export const toolPolicies = resource<ToolPolicy, S['ToolPolicyInput'], { q?: string; level?: string; target?: string; action?: string }>('tool-policies');
const toolPath = { params: { path: { id: '' } } };
const at = (id: string) => ({ params: { path: { id } } }) as typeof toolPath;
export const tools = {
  path: '/admin/api/v1/tools',
  get: (id: string) => unwrap<S['ToolDetail']>(api.GET('/admin/api/v1/tools/{id}', at(id))),
  approve: (id: string, body: S['ToolDecisionInput']) => unwrap<S['ToolDecision']>(api.POST('/admin/api/v1/tools/{id}/approve', { ...at(id), body })),
  block: (id: string, body: S['ToolDecisionInput']) => unwrap<S['ToolDecision']>(api.POST('/admin/api/v1/tools/{id}/block', { ...at(id), body })),
  acknowledge: (id: string) => unwrap<ToolItem>(api.POST('/admin/api/v1/tools/{id}/acknowledge', at(id))),
  remove: (id: string) => unwrap<void>(api.DELETE('/admin/api/v1/tools/{id}', at(id))),
};
export type PluginInfo = S['PluginInfo'];
/** Registrierte Plugins mit Standard-Fehlermodus/-Zeitlimit (GET /plugins). */
export const pluginCatalog = async () => (await unwrap<S['PluginCatalog']>(api.GET('/admin/api/v1/plugins'))).items;
/** Testlauf einer ungespeicherten Plugin-Konfiguration (nur lokale Plugins, schreibt nichts). */
export const testPlugin = (body: S['PluginTestRequest']) => unwrap<S['PluginTestResult']>(api.POST('/admin/api/v1/plugin-bindings:test', { body }));

// ------------------------------------------------------------------ AIGW-63: WASM-Module

export type WasmModule = S['WasmModule'];
const WASM_ITEM = '/admin/api/v1/wasm-modules/{name}';
/** SHA-256 (hex) im Browser (Web Crypto; nur in sicheren Kontexten – sonst den Wert von Hand angeben). */
export async function sha256Hex(data: ArrayBuffer): Promise<string> {
  if (!globalThis.crypto?.subtle) throw new Error('Web Crypto nicht verfügbar (kein HTTPS) – SHA-256 bitte angeben.');
  const d = await crypto.subtle.digest('SHA-256', data);
  return [...new Uint8Array(d)].map((x) => x.toString(16).padStart(2, '0')).join('');
}
/** WASM-Module (Komponenten für die Plugin-Art `wasm`): Schreiben nur globaler Admin. */
export const wasmModules = {
  list: async () => (await unwrap<S['WasmModuleList']>(api.GET('/admin/api/v1/wasm-modules'))).items,
  /** Liste samt Signatur-Richtlinie (AIGW-101: `signatureRequired`, `trustedKeys`). */
  overview: async () => unwrap<S['WasmModuleList']>(api.GET('/admin/api/v1/wasm-modules')),
  /** Hochladen bzw. Ersetzen (`exists`: If-Match mit dem aktuellen ETag; `keyId`+`signature`: Ed25519-Signatur, AIGW-101). */
  upload: async (name: string, content: ArrayBuffer, opts: { sha256?: string; description?: string; exists?: boolean; keyId?: string; signature?: string }) => {
    const sha256 = opts.sha256?.trim() || (await sha256Hex(content));
    const headers: Record<string, string> = { 'Content-Type': 'application/wasm' };
    if (opts.exists) headers['If-Match'] = await etagFor(WASM_ITEM, name);
    return unwrap<WasmModule>(
      api.PUT(WASM_ITEM, {
        params: {
          path: { name },
          query: { sha256, description: opts.description?.trim() || undefined, keyId: opts.keyId?.trim() || undefined, signature: opts.signature?.trim() || undefined },
        },
        body: content as unknown as string,
        bodySerializer: (b: unknown) => b as BodyInit,
        headers,
      }),
    );
  },
  remove: async (name: string) => {
    const tag = await etagFor(WASM_ITEM, name);
    return unwrap<void>(api.DELETE(WASM_ITEM, { params: { path: { name } }, headers: { 'If-Match': tag } }));
  },
};

// ------------------------------------------------------------------ AIGW-86: Standard-Guardrail-Profile

export type GuardrailProfile = S['GuardrailProfile'];
export type GuardrailProfileApplyResult = S['GuardrailProfileApplyResult'];
/** Eingebettete Profile (Basis, Streng, Offen) mit ihren Bindungen als PluginBinding-Specs. */
export const guardrailProfiles = async () => (await unwrap<S['GuardrailProfileList']>(api.GET('/admin/api/v1/guardrail-profiles'))).items;
/** Profil auf einen Scope anwenden; `dryRun: true` liefert nur den Diff. */
export const applyGuardrailProfile = (name: string, body: S['GuardrailProfileApply']) =>
  unwrap<GuardrailProfileApplyResult>(api.POST('/admin/api/v1/guardrail-profiles/{name}:apply', { params: { path: { name } }, body }));
/** Plugins, die als Guardrail zählen (Hinweis „keine Guardrails aktiv“). */
export const GUARDRAIL_PLUGINS = ['pii', 'prompt-filter', 'prompt-leak', 'content-safety'];

// Preisvorschläge (AIGW-85): Azure Retail Prices + Preiskatalog, Übernahme als neue Preisversionen.
export type PriceSuggestionReport = S['PriceSuggestionReport'];
export type PriceSuggestion = S['PriceSuggestion'];
export const priceSuggestions = {
  list: (refresh = false) => unwrap<PriceSuggestionReport>(api.GET('/admin/api/v1/price-suggestions', { params: { query: refresh ? { refresh: true } : {} } })),
  accept: (body: S['PriceSuggestionAccept']) => unwrap<S['PriceSuggestionAcceptResult']>(api.POST('/admin/api/v1/price-suggestions:accept', { body })),
};
