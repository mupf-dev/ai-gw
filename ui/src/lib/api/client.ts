// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2
//
// Typisierter Client der Admin-API (AIGW-49). Die Typen in schema.d.ts werden aus der OpenAPI-
// Spezifikation erzeugt (scripts/gen-api.sh → ui/openapi.json → pnpm gen:api).
//
// Einheitlich für alle Seiten:
// - Anmeldung: Bearer-Token (Bootstrap-Token oder Admin-API-Key, nur sessionStorage) bzw. Session-Cookie;
//   schreibende Anfragen mit Session tragen das CSRF-Token (Header X-CSRF-Token, AIGW-32).
// - Fehler: RFC 9457 Problem Details → `ApiError` (Status, Code, Titel, Detail); 401 meldet sich beim
//   Sitzungsmodul (Login-Weiterleitung bzw. Token-Dialog), siehe session.svelte.ts.
// - Optimistische Sperre: `get` liefert das ETag, `update`/`remove`/Aktionen senden es als If-Match.
//   If-Match ist Pflicht (F38, sonst 428): ohne übergebenes ETag holt der Client vorher den aktuellen
//   Stand (GET) und nutzt dessen ETag (`*` nur, wenn der Server keines liefert).
// - Cursor-Pagination: `listAll` folgt `nextCursor`, `listPage` liefert eine Seite.
// - Mock-Modus: dieselben Aufrufe gehen an ein In-Memory-Backend (mock/backend.ts, nachgeladen).
import createClient from 'openapi-fetch';
import type { components, paths } from './schema';

export type Schemas = components['schemas'];
export type Problem = Schemas['Problem'];
export type VersionInfo = Schemas['VersionInfo'];
export type ObjectMeta = Schemas['ObjectMeta'];

/** Fehler der Admin-API (aus Problem Details) oder des Transports (status 0). */
export class ApiError extends Error {
  readonly status: number;
  readonly code: string;
  readonly title: string;
  readonly detail: string | null;
  /** Vollständige Problem Details (z. B. `issues` von GitOps mit Zeilenbezug), falls geliefert. */
  readonly problem: Partial<Problem> | null;
  constructor(status: number, code: string, title: string, detail: string | null = null, problem: Partial<Problem> | null = null) {
    super(detail ? `${title}: ${detail}` : title);
    this.status = status;
    this.code = code;
    this.title = title;
    this.detail = detail;
    this.problem = problem;
  }
}

const SAFE = new Set(['GET', 'HEAD', 'OPTIONS']);
/** Ohne Anmeldung erreichbar – ein 401 dort ist keine abgelaufene Anmeldung. */
const PUBLIC = /\/admin\/api\/v1\/(version|auth\/info)$/;
/** Pfade, die im Mock-Modus das In-Memory-Backend bedient (nicht Version/Anmeldung). */
const MOCKABLE = /^\/admin\/api\/v1\/(?!version$|auth\/|session$)/;

const state: {
  csrf: string | null;
  bearer: string | null;
  mock: () => boolean;
  onUnauthorized: (() => void) | null;
} = { csrf: null, bearer: null, mock: () => false, onUnauthorized: null };

/** Setzt das CSRF-Token der aktuellen Session (aus /admin/api/v1/auth/info). */
export function setCsrfToken(token: string | null) {
  state.csrf = token;
}
/** Bearer-Token (Bootstrap-Token oder Admin-API-Key) für alle Anfragen. */
export function setBearer(token: string | null) {
  state.bearer = token;
}
/** Entscheidet je Anfrage, ob das Mock-Backend antwortet. */
export function setMockResolver(f: () => boolean) {
  state.mock = f;
}
/** Wird bei 401 einer geschützten Route aufgerufen. */
export function onUnauthorized(f: () => void) {
  state.onUnauthorized = f;
}

async function transport(req: Request): Promise<Response> {
  const path = new URL(req.url, location.href).pathname;
  if (state.mock() && MOCKABLE.test(path)) {
    const { mockFetch } = await import('../mock/backend');
    return mockFetch(req);
  }
  return fetch(req);
}

export const api = createClient<paths>({ baseUrl: '', fetch: transport });

api.use({
  onRequest({ request }) {
    if (state.bearer && !request.headers.has('Authorization')) request.headers.set('Authorization', `Bearer ${state.bearer}`);
    if (state.csrf && !SAFE.has(request.method)) request.headers.set('X-CSRF-Token', state.csrf);
    return request;
  },
  onResponse({ request, response }) {
    if (response.status === 401 && !PUBLIC.test(new URL(request.url, location.href).pathname)) state.onUnauthorized?.();
    return response;
  },
});

// ------------------------------------------------------------------ Ergebnisse auspacken

type FetchResult = { data?: unknown; error?: unknown; response: Response };

function toError(error: unknown, response: Response): ApiError {
  const p = (error ?? {}) as Partial<Problem>;
  if (typeof p === 'object' && typeof p.code === 'string') {
    return new ApiError(response.status, p.code, p.title ?? `HTTP ${response.status}`, p.detail ?? null, p);
  }
  return new ApiError(response.status, `http-${response.status}`, `HTTP ${response.status}`, typeof error === 'string' && error ? error.slice(0, 300) : null);
}

async function settle(p: Promise<FetchResult>): Promise<FetchResult> {
  try {
    return await p;
  } catch (e) {
    throw new ApiError(0, 'network', 'Gateway nicht erreichbar', e instanceof Error ? e.message : String(e));
  }
}

/** Daten oder `ApiError`. */
export async function unwrap<T>(p: Promise<FetchResult>): Promise<T> {
  const r = await settle(p);
  if (!r.response.ok) throw toError(r.error, r.response);
  return r.data as T;
}

export interface Versioned<T> {
  data: T;
  /** Starkes ETag der gelesenen Version (für If-Match), falls geliefert. */
  etag: string | null;
}

/** Daten samt ETag oder `ApiError`. */
export async function unwrapVersioned<T>(p: Promise<FetchResult>): Promise<Versioned<T>> {
  const r = await settle(p);
  if (!r.response.ok) throw toError(r.error, r.response);
  return { data: r.data as T, etag: r.response.headers.get('ETag') };
}

// ------------------------------------------------------------------ generische Ressourcen

// openapi-fetch typisiert Pfade als Literale; die generischen Abläufe unten setzen den Pfad zur Laufzeit
// zusammen. Die Ein-/Ausgabetypen kommen trotzdem aus schema.d.ts (Typparameter je Ressource).
type Init = {
  params?: { path?: Record<string, string>; query?: Record<string, unknown>; header?: Record<string, string> };
  body?: unknown;
  headers?: Record<string, string>;
  parseAs?: 'json' | 'text' | 'stream';
  signal?: AbortSignal;
  bodySerializer?: (b: unknown) => BodyInit;
};
type Method = (path: string, init?: Init) => Promise<FetchResult>;
const loose = api as unknown as { GET: Method; POST: Method; PUT: Method; PATCH: Method; DELETE: Method };

export type Query = Record<string, string | number | boolean | null | undefined>;

function cleanQuery(q: Query = {}): Record<string, string | number | boolean> {
  const out: Record<string, string | number | boolean> = {};
  for (const [k, v] of Object.entries(q)) if (v !== undefined && v !== null && v !== '') out[k] = v;
  return out;
}

const ifMatch = (etag: string): Record<string, string> => ({ 'If-Match': etag });

/**
 * ETag für If-Match (F38: Pflicht bei PUT/PATCH/DELETE/Aktionen). Ohne gelesenes ETag wird der aktuelle
 * Stand geholt – die Prüfung schützt dann zumindest vor Änderungen zwischen diesem Lesen und dem Schreiben.
 */
export async function etagFor(itemPath: string, name: string, etag?: string | null): Promise<string> {
  if (etag) return etag;
  const r = await unwrapVersioned<unknown>(loose.GET(itemPath, { params: { path: { name } } }));
  return r.etag ?? '*';
}

export interface Page<T> {
  items: T[];
  nextCursor: string | null;
}

/** Eine Seite einer Liste (`limit` 1–500, `cursor` aus der vorigen Seite). */
export async function listPage<T>(path: string, query: Query = {}, limit = 100, cursor?: string): Promise<Page<T>> {
  const page = await unwrap<{ items: T[]; nextCursor?: string | null }>(
    loose.GET(path, { params: { query: { ...cleanQuery(query), limit, ...(cursor ? { cursor } : {}) } } }),
  );
  return { items: page.items, nextCursor: page.nextCursor ?? null };
}

/** Alle Seiten einer Liste (folgt `nextCursor`; Obergrenze schützt vor Endlosschleifen). */
export async function listAll<T>(path: string, query: Query = {}, max = 10_000): Promise<T[]> {
  const items: T[] = [];
  let cursor: string | undefined;
  do {
    const page = await listPage<T>(path, query, 500, cursor);
    items.push(...page.items);
    cursor = page.nextCursor ?? undefined;
  } while (cursor && items.length < max);
  return items;
}

export interface Resource<T, I, Q extends Query> {
  path: string;
  list(query?: Q): Promise<T[]>;
  get(name: string): Promise<Versioned<T>>;
  create(input: I): Promise<Versioned<T>>;
  update(name: string, input: I, etag?: string | null): Promise<Versioned<T>>;
  remove(name: string, etag?: string | null): Promise<void>;
}

/** CRUD-Abläufe einer Ressource unter /admin/api/v1/<segment> (Admin-API §2/§3). */
export function resource<T, I, Q extends Query = Query>(segment: string): Resource<T, I, Q> {
  const path = `/admin/api/v1/${segment}`;
  const one = `${path}/{name}`;
  const at = (name: string) => ({ path: { name } });
  return {
    path,
    list: (query) => listAll<T>(path, query),
    get: (name) => unwrapVersioned<T>(loose.GET(one, { params: at(name) })),
    create: (input) => unwrapVersioned<T>(loose.POST(path, { body: input })),
    update: async (name, input, etag) =>
      unwrapVersioned<T>(loose.PUT(one, { params: at(name), body: input, headers: ifMatch(await etagFor(one, name, etag)) })),
    remove: async (name, etag) => {
      await unwrap<void>(loose.DELETE(one, { params: at(name), headers: ifMatch(await etagFor(one, name, etag)) }));
    },
  };
}

/** POST auf eine Aktion einer Ressource (z. B. api-keys/{name}/revoke) mit If-Match. */
export async function action<T>(segment: string, name: string, verb: string, body?: unknown, etag?: string | null): Promise<Versioned<T>> {
  const tag = await etagFor(`/admin/api/v1/${segment}/{name}`, name, etag);
  return unwrapVersioned<T>(
    loose.POST(`/admin/api/v1/${segment}/{name}/${verb}`, { params: { path: { name } }, body: body ?? {}, headers: ifMatch(tag) }),
  );
}

/** PATCH (JSON Merge Patch) mit If-Match. */
export async function patch<T>(segment: string, name: string, body: unknown, etag?: string | null): Promise<Versioned<T>> {
  const one = `/admin/api/v1/${segment}/{name}`;
  const tag = await etagFor(one, name, etag);
  return unwrapVersioned<T>(
    loose.PATCH(one, {
      params: { path: { name } },
      body,
      headers: { 'Content-Type': 'application/merge-patch+json', ...ifMatch(tag) },
    }),
  );
}

/** Auswahl für einen Teil-Export (AIGW-55); leer = alles. */
export interface ExportSelection {
  /** Arten (YAML-Schreibweise, z. B. `ModelGroup`). */
  kinds?: string[];
  name?: string;
  organization?: string;
  team?: string;
}

/** GitOps-Export als YAML-Text (gesamt oder Auswahl). */
export function exportConfig(apiKeys: Schemas['ApiKeyMode'], sel: ExportSelection = {}): Promise<string> {
  const query = cleanQuery({ apiKeys, kinds: sel.kinds?.join(','), name: sel.name, organization: sel.organization, team: sel.team });
  return unwrap<string>(loose.GET('/admin/api/v1/config:export', { params: { query }, parseAs: 'text' }));
}

/** GitOps-Apply bzw. Trockenlauf (`dryRun`). */
export function applyConfig(yaml: string, opts: { dryRun: boolean; prune: boolean; apiKeys: Schemas['ApiKeyMode'] }): Promise<Schemas['ApplyReport']> {
  return unwrap<Schemas['ApplyReport']>(
    loose.POST('/admin/api/v1/config:apply', {
      params: { query: { dryRun: opts.dryRun, prune: opts.prune, apiKeys: opts.apiKeys } },
      body: yaml,
      bodySerializer: (b) => b as string,
      headers: { 'Content-Type': 'application/yaml' },
    }),
  );
}

// ------------------------------------------------------------------ Playground (AIGW-54)

/** Modellgruppen, die der Key im Playground nutzen darf. */
export function playgroundModels(key: string): Promise<Schemas['PlaygroundModels']> {
  return unwrap<Schemas['PlaygroundModels']>(loose.GET('/admin/api/v1/playground/models', { params: { query: { key } } }));
}

/** Antwort des Playground-Chats: Strom (Erfolg) bzw. Fehlerobjekt der Datenebene. */
export interface PlaygroundReply {
  response: Response;
  /** Körper bei Erfolg (JSON oder SSE). */
  body: ReadableStream<Uint8Array> | null;
  /** Fehlerkörper der Datenebene (`{error: {message, type, code}}`), falls nicht ok. */
  error: unknown;
}

/**
 * Chat im Namen des Keys (Pipeline der Datenebene). Fehler der Admin-API selbst (Problem Details: 401/403/404)
 * werfen `ApiError`; Fehler der Datenebene (Modell nicht freigegeben, Limit, Budget, Guardrail …) stehen in `error`.
 */
export async function playgroundChat(key: string, body: unknown, signal?: AbortSignal): Promise<PlaygroundReply> {
  const r = await settle(loose.POST('/admin/api/v1/playground/chat', { params: { query: { key } }, body, parseAs: 'stream', signal }));
  const e = r.error as Partial<Problem> | undefined;
  if (!r.response.ok && e && typeof e === 'object' && typeof e.code === 'string' && typeof e.status === 'number') throw toError(e, r.response);
  return { response: r.response, body: r.response.ok ? (r.data as ReadableStream<Uint8Array>) : null, error: r.error };
}
