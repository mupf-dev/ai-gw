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

/**
 * Pflichtfelder der Zugangsdaten je Anbieter (08-admin-api.md §2.4): eine Alternative muss vollständig sein.
 * Leere Liste = beliebige Felder. `secretLong` = mehrzeiliger Wert (z. B. Service-Account-JSON).
 */
export const CREDENTIAL_FIELDS: Record<ProviderKind, { alternatives: { label: string; fields: string[] }[]; optional: string[] }> = {
  openai: { alternatives: [{ label: 'API-Key', fields: ['apiKey'] }], optional: ['organization', 'project'] },
  anthropic: { alternatives: [{ label: 'API-Key', fields: ['apiKey'] }], optional: [] },
  mistral: { alternatives: [{ label: 'API-Key', fields: ['apiKey'] }], optional: [] },
  'azure-openai': {
    alternatives: [
      { label: 'API-Key', fields: ['apiKey'] },
      { label: 'Entra ID (Service Principal)', fields: ['tenantId', 'clientId', 'clientSecret'] },
    ],
    optional: [],
  },
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
export const LONG_FIELDS = new Set(['serviceAccountJson', 'privateKey']);
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
/** Verbindungszustand der MCP-Upstreams (GET /mcp-servers:status). */
export const mcpStatus = () => unwrap<S['McpStatusList']>(api.GET('/admin/api/v1/mcp-servers:status'));
export type PluginBinding = S['PluginBinding'];
export const pluginBindingsRes = resource<PluginBinding, S['PluginBindingInput'], { q?: string; plugin?: string; level?: string; target?: string }>(
  'plugin-bindings',
);

// ------------------------------------------------------------------ AIGW-52: Editoren (Plugins, MCP-Freigaben)

export type McpGrant = S['McpGrant'];
export const mcpGrants = resource<McpGrant, S['McpGrantInput'], { q?: string; server?: string; team?: string; apiKey?: string }>('mcp-grants');
export type PluginInfo = S['PluginInfo'];
/** Registrierte Plugins mit Standard-Fehlermodus/-Zeitlimit (GET /plugins). */
export const pluginCatalog = async () => (await unwrap<S['PluginCatalog']>(api.GET('/admin/api/v1/plugins'))).items;
/** Testlauf einer ungespeicherten Plugin-Konfiguration (nur lokale Plugins, schreibt nichts). */
export const testPlugin = (body: S['PluginTestRequest']) => unwrap<S['PluginTestResult']>(api.POST('/admin/api/v1/plugin-bindings:test', { body }));
