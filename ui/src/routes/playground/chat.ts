// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Playground (AIGW-54): Anfrage bauen, Antwort lesen (JSON oder SSE über fetch-Stream, POST – daher kein
// EventSource), Kennzahlen messen (TTFT/Latenz im Browser) und Code-Beispiele erzeugen.
import { playgroundChat } from '../../lib/api/client';

export interface Message {
  role: 'system' | 'user' | 'assistant';
  content: string;
}

export interface Params {
  model: string;
  system: string;
  temperature: string;
  maxTokens: string;
  n: number;
  stream: boolean;
}

export interface ChoiceOut {
  content: string;
  reasoning: string;
  finish: string | null;
}

export interface Usage {
  prompt_tokens?: number;
  completion_tokens?: number;
  total_tokens?: number;
  completion_tokens_details?: { reasoning_tokens?: number } | null;
  prompt_tokens_details?: { cached_tokens?: number } | null;
}

export interface Outcome {
  requestId: string | null;
  deployment: string | null;
  model: string | null;
  cache: string | null;
  attempts: string | null;
  fallback: string | null;
  status: number;
  choices: ChoiceOut[];
  usage: Usage | null;
  /** Zeit bis zum ersten Inhalt (Browser, ms). */
  ttftMs: number | null;
  /** Gesamtdauer (Browser, ms). */
  latencyMs: number | null;
  error: { message: string; code: string | null; type: string | null } | null;
  done: boolean;
}

/** Request-Körper im OpenAI-Format. */
export function buildBody(p: Params, history: Message[]): Record<string, unknown> {
  const messages: Message[] = [];
  if (p.system.trim()) messages.push({ role: 'system', content: p.system });
  messages.push(...history);
  const body: Record<string, unknown> = { model: p.model, messages };
  const t = Number(p.temperature);
  if (p.temperature.trim() !== '' && Number.isFinite(t)) body.temperature = t;
  const m = Number(p.maxTokens);
  if (p.maxTokens.trim() !== '' && Number.isInteger(m) && m > 0) body.max_tokens = m;
  if (p.n > 1) body.n = p.n;
  if (p.stream) {
    body.stream = true;
    body.stream_options = { include_usage: true };
  }
  return body;
}

function emptyOutcome(): Outcome {
  return {
    requestId: null,
    deployment: null,
    model: null,
    cache: null,
    attempts: null,
    fallback: null,
    status: 0,
    choices: [],
    usage: null,
    ttftMs: null,
    latencyMs: null,
    error: null,
    done: false,
  };
}

function choice(out: Outcome, i: number): ChoiceOut {
  while (out.choices.length <= i) out.choices.push({ content: '', reasoning: '', finish: null });
  return out.choices[i];
}

type Json = Record<string, unknown>;

function dataError(v: unknown): Outcome['error'] {
  const e = (v as Json | null)?.error as Json | undefined;
  if (e && typeof e === 'object') {
    return { message: String(e.message ?? 'Fehler'), code: (e.code as string) ?? null, type: (e.type as string) ?? null };
  }
  return { message: typeof v === 'string' && v ? v.slice(0, 500) : 'Unbekannter Fehler', code: null, type: null };
}

/** Wendet einen JSON-Chunk (SSE) bzw. die ganze Antwort (JSON) an. */
function apply(out: Outcome, v: Json, stream: boolean) {
  if (v.error) {
    out.error = dataError(v);
    return;
  }
  if (typeof v.model === 'string' && !out.model) out.model = v.model;
  if (v.usage) out.usage = v.usage as Usage;
  for (const c of (v.choices as Json[] | undefined) ?? []) {
    const idx = typeof c.index === 'number' ? c.index : 0;
    const ch = choice(out, idx);
    const part = (stream ? c.delta : c.message) as Json | undefined;
    if (part) {
      if (typeof part.content === 'string') ch.content += part.content;
      const r = part.reasoning_content ?? part.reasoning;
      if (typeof r === 'string') ch.reasoning += r;
    }
    if (typeof c.finish_reason === 'string') ch.finish = c.finish_reason;
  }
}

/**
 * Führt eine Anfrage aus und meldet Zwischenstände (`onUpdate`, z. B. je SSE-Chunk). Fehler der Admin-API
 * (Anmeldung, Rechte, Key) werfen `ApiError`; Fehler der Datenebene stehen in `Outcome.error`.
 */
export async function run(key: string, body: Record<string, unknown>, onUpdate: (o: Outcome) => void, signal?: AbortSignal): Promise<Outcome> {
  const out = emptyOutcome();
  const t0 = performance.now();
  const reply = await playgroundChat(key, body, signal);
  const h = reply.response.headers;
  out.status = reply.response.status;
  out.requestId = h.get('x-aigw-request-id');
  out.deployment = h.get('x-aigw-deployment');
  out.model = h.get('x-aigw-model');
  out.cache = h.get('x-aigw-cache');
  out.attempts = h.get('x-aigw-attempts');
  out.fallback = h.get('x-aigw-fallback');
  if (!reply.body) {
    out.error = dataError(reply.error);
    out.latencyMs = performance.now() - t0;
    out.done = true;
    onUpdate(out);
    return out;
  }
  const stream = (h.get('content-type') ?? '').startsWith('text/event-stream');
  const reader = reply.body.getReader();
  const decoder = new TextDecoder();
  let buf = '';
  let text = '';
  const mark = () => {
    if (out.ttftMs === null && out.choices.some((c) => c.content || c.reasoning)) out.ttftMs = performance.now() - t0;
  };
  try {
    for (;;) {
      const { value: bytes, done } = await reader.read();
      if (done) break;
      const value = decoder.decode(bytes, { stream: true });
      if (!stream) {
        text += value;
        continue;
      }
      buf += value;
      let sep: number;
      while ((sep = buf.search(/\r?\n\r?\n/)) >= 0) {
        const event = buf.slice(0, sep);
        buf = buf.slice(sep).replace(/^\r?\n\r?\n/, '');
        const data = event
          .split(/\r?\n/)
          .filter((l) => l.startsWith('data:'))
          .map((l) => l.slice(5).trimStart())
          .join('\n');
        if (!data || data === '[DONE]') continue;
        try {
          apply(out, JSON.parse(data) as Json, true);
        } catch {
          /* unvollständiges/fremdes Ereignis ignorieren */
        }
        mark();
        onUpdate(out);
      }
    }
  } finally {
    reader.releaseLock();
  }
  if (!stream) {
    try {
      apply(out, JSON.parse(text) as Json, false);
    } catch {
      out.error = dataError(text);
    }
    mark();
  }
  out.latencyMs = performance.now() - t0;
  out.done = true;
  onUpdate(out);
  return out;
}

// ------------------------------------------------------------------ Code-Beispiele

export type SnippetLang = 'curl' | 'openai' | 'anthropic';

/** Beispiel für die Datenebene (`$AIGW_URL`, `$AIGW_API_KEY` statt echter Werte – der Key-Klartext ist nie bekannt). */
export function snippet(lang: SnippetLang, body: Record<string, unknown>): string {
  const msgs = (body.messages as Message[]) ?? [];
  if (lang === 'curl') {
    return `curl "$AIGW_URL/v1/chat/completions" \\\n  -H "Authorization: Bearer $AIGW_API_KEY" \\\n  -H "Content-Type: application/json" \\\n  ${body.stream ? '-N ' : ''}-d '${JSON.stringify(body, null, 2).replace(/'/g, "'\\''")}'`;
  }
  const py = (v: unknown) =>
    JSON.stringify(v, null, 4)
      .replace(/\btrue\b/g, 'True')
      .replace(/\bfalse\b/g, 'False')
      .replace(/\bnull\b/g, 'None');
  if (lang === 'openai') {
    const args = Object.entries(body)
      .map(([k, v]) => `    ${k}=${py(v).replace(/\n/g, '\n    ')},`)
      .join('\n');
    const tail = body.stream
      ? 'for chunk in resp:\n    if chunk.choices:\n        print(chunk.choices[0].delta.content or "", end="")'
      : 'print(resp.choices[0].message.content)';
    return `import os\nfrom openai import OpenAI\n\nclient = OpenAI(base_url=os.environ["AIGW_URL"] + "/v1", api_key=os.environ["AIGW_API_KEY"])\nresp = client.chat.completions.create(\n${args}\n)\n${tail}\n`;
  }
  // Anthropic Messages (ai-gw nimmt beide Formate an; system als eigenes Feld).
  const system = msgs.find((m) => m.role === 'system')?.content;
  const rest = msgs.filter((m) => m.role !== 'system');
  const a: Record<string, unknown> = { model: body.model, max_tokens: body.max_tokens ?? 1024, messages: rest };
  if (system) a.system = system;
  if (body.temperature !== undefined) a.temperature = body.temperature;
  const args = Object.entries(a)
    .map(([k, v]) => `    ${k}=${py(v).replace(/\n/g, '\n    ')},`)
    .join('\n');
  return `import os\nimport anthropic\n\nclient = anthropic.Anthropic(base_url=os.environ["AIGW_URL"], api_key=os.environ["AIGW_API_KEY"])\nmsg = client.messages.create(\n${args}\n)\nprint(msg.content[0].text)\n`;
}
