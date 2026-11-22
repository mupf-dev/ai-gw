<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // Detail einer Anfrage (GET /request-logs/{id}): Übersicht, Fehlerklasse (F47), Trace-ID mit Link zum
  // Tracing-System (admin.ui.trace_url), Guardrail-Treffer (F161), Eltern-Anfrage (F191, z. B. Cache-Embedding),
  // Upstream-Versuche (Retries/Fallbacks) und Payload (nur admin/auditor).
  import { amount, requestLog, type RequestLogDetail } from '../../lib/api/observe';
  import { providerLabel } from '../../lib/api/resources';
  import AsyncState from '../../lib/admin/AsyncState.svelte';
  import { Loader } from '../../lib/admin/errors.svelte';
  import { dateTime, ms, num } from '../../lib/format';
  import { detailPath } from '../../lib/nav';
  import { href } from '../../lib/router.svelte';
  import { auth } from '../../lib/session.svelte';
  import Badge from '../../lib/ui/Badge.svelte';
  import CodeBlock from '../../lib/ui/CodeBlock.svelte';
  import Sheet from '../../lib/ui/Sheet.svelte';

  let { open = $bindable(false), id, onopen }: { open?: boolean; id: string | null; onopen: (id: string) => void } = $props();

  const detail = new Loader<RequestLogDetail | null>(async () => (id ? requestLog(id) : null));
  $effect(() => {
    void id;
    if (open && id) void detail.reload();
  });
  // AIGW-115: Art des Access-Tokens (Entra: idtyp/xms_sub_fct).
  const WORKLOAD_KIND: Record<string, string> = { app: 'App-only', delegated: 'für Nutzer (OBO)', 'agent-user': 'Agent-Nutzerkonto', unknown: 'Token' };
  const traceLink = (t: string) => (auth.traceUrl ? auth.traceUrl.replace('{traceId}', encodeURIComponent(t)) : null);
  const PAYLOAD: Record<string, string> = { forbidden: 'nur für Admin/Auditor', 'not-stored': 'nicht gespeichert (observe.payload)', available: 'verfügbar' };
</script>

<Sheet bind:open title="Anfrage" description={id ?? ''} size="lg" testid="request-detail">
  <div class="flex flex-col gap-5 px-5 py-4">
    <AsyncState loader={detail}>
      {#snippet children(r)}
        {#if r}
          <dl class="grid grid-cols-[auto_1fr] gap-x-6 gap-y-2 text-base">
            <dt class="text-fg-muted">Zeit</dt><dd class="num">{dateTime(Date.parse(r.at))}</dd>
            <dt class="text-fg-muted">Status</dt>
            <dd class="flex flex-wrap items-center gap-2">
              <Badge tone={r.status >= 500 ? 'danger' : r.status >= 400 ? 'warning' : 'success'} mono>{r.status}</Badge>
              {#if r.errorClass}<Badge tone="danger" mono>{r.errorClass}</Badge>{/if}
              {#if r.errorCode && r.errorCode !== r.errorClass}<span class="font-mono text-xs text-fg-muted">{r.errorCode}</span>{/if}
              {#if r.clientAborted}<Badge tone="neutral">vom Client abgebrochen</Badge>{/if}
            </dd>
            <dt class="text-fg-muted">Endpunkt</dt><dd class="font-mono text-sm">{r.endpoint}{r.stream ? ' · Stream' : ''}</dd>
            <dt class="text-fg-muted">Modell</dt>
            <dd class="text-sm">
              {#if r.modelGroup}<a class="font-mono hover:text-accent-text" href={href(detailPath('model-group', r.modelGroup))}>{r.modelGroup}</a>{:else}<span class="font-mono">{r.modelRequested ?? '–'}</span>{/if}
              {#if r.deployment} → <a class="font-mono hover:text-accent-text" href={href(detailPath('deployment', r.deployment))}>{r.deployment}</a>{/if}
              {#if r.provider}<span class="text-fg-muted"> ({providerLabel(r.provider)}{r.providerModel ? ` · ${r.providerModel}` : ''})</span>{/if}
            </dd>
            <dt class="text-fg-muted">Key / Team</dt>
            <dd class="text-sm">
              {#if r.apiKey}<a class="font-mono hover:text-accent-text" href={href(detailPath('key', r.apiKey))}>{r.apiKey}</a>{:else}–{/if}
              {#if r.team} · <a class="font-mono hover:text-accent-text" href={href(detailPath('team', r.team))}>{r.team}</a>{/if}
            </dd>
            {#if r.workload}
              {@const w = r.workload}
              <dt class="text-fg-muted">Identität</dt>
              <dd class="text-sm break-all" data-testid="request-workload">
                <span class="font-mono">{w.issuer}/{w.subject}</span>
                <span class="text-fg-muted"> · {WORKLOAD_KIND[w.kind] ?? w.kind}{w.agent ? ' · Agent' : ''}</span>
                {#if w.user}<span class="text-fg-muted"> · Nutzer </span><span class="font-mono">{w.user}</span>{/if}
                {#if w.blueprint}<span class="text-fg-muted"> · Blueprint </span><span class="font-mono">{w.blueprint}</span>{/if}
              </dd>
            {/if}
            <dt class="text-fg-muted">Tokens</dt>
            <dd class="num text-sm">{num(r.inputTokens)} → {num(r.outputTokens)}{r.reasoningTokens ? ` (Reasoning ${num(r.reasoningTokens)})` : ''}{r.cachedTokens ? ` · Cache ${num(r.cachedTokens)}` : ''}{r.usageEstimated ? ' · geschätzt' : ''}</dd>
            <dt class="text-fg-muted">Kosten</dt><dd class="num text-sm">{amount(r.cost)}</dd>
            <dt class="text-fg-muted">Latenz</dt><dd class="num text-sm">{ms(r.latencyMs)}{r.ttftMs != null ? ` · erstes Token ${ms(r.ttftMs)}` : ''}</dd>
            <dt class="text-fg-muted">Cache</dt><dd class="font-mono text-sm">{r.cache ?? 'none'}</dd>
            {#if r.clientRequestId}<dt class="text-fg-muted">Client-ID</dt><dd class="font-mono text-sm break-all">{r.clientRequestId}</dd>{/if}
            {#if r.traceId}
              {@const link = traceLink(r.traceId)}
              <dt class="text-fg-muted">Trace</dt>
              <dd class="font-mono text-sm break-all" data-testid="trace-id">
                {#if link}<a class="text-accent-text hover:underline" href={link} target="_blank" rel="noopener noreferrer" data-testid="trace-link">{r.traceId}</a>{:else}{r.traceId}{/if}
              </dd>
            {/if}
            {#if r.parentRequestId}
              <dt class="text-fg-muted">Ausgelöst von</dt>
              <dd><button type="button" class="font-mono text-sm text-accent-text hover:underline" onclick={() => onopen(r.parentRequestId ?? '')} data-testid="parent-request">{r.parentRequestId}</button></dd>
            {/if}
          </dl>

          {#if r.guardrails?.length}
            <section data-testid="guardrails">
              <h3 class="label-caps mb-1.5 text-fg-subtle">Guardrail-Treffer</h3>
              <ul class="flex flex-col gap-1 text-sm">
                {#each r.guardrails as g, i (i)}
                  <li class="flex flex-wrap items-center gap-2"><Badge mono>{g.plugin}</Badge><span>{g.category}</span><Badge tone={g.action === 'reject' ? 'danger' : 'warning'}>{g.action}</Badge><span class="num text-fg-muted">× {g.count}</span></li>
                {/each}
              </ul>
            </section>
          {/if}

          <section>
            <h3 class="label-caps mb-1.5 text-fg-subtle">Upstream-Versuche ({r.attemptCount})</h3>
            {#if r.attempts.length}
              <ol class="flex flex-col gap-1 text-sm" data-testid="attempts">
                {#each r.attempts as a, i (i)}
                  <li class="flex flex-wrap items-center gap-2">
                    <span class="num w-5 text-fg-subtle">{i + 1}.</span>
                    <span class="font-mono">{a.deployment}</span>
                    {#if a.modelGroup && a.modelGroup !== r.modelGroup}<Badge tone="info">Fallback → {a.modelGroup}</Badge>{/if}
                    {#if a.error}<Badge tone="danger" mono>{a.error}{a.upstreamStatus ? ` ${a.upstreamStatus}` : ''}</Badge>{:else}<Badge tone="success">ok</Badge>{/if}
                    <span class="num text-fg-muted">{ms(a.durationMs)}</span>
                  </li>
                {/each}
              </ol>
            {:else}
              <p class="text-sm text-fg-muted">Kein Upstream-Aufruf (z. B. Cache-Treffer, Ablehnung vor dem Routing).</p>
            {/if}
          </section>

          <section>
            <h3 class="label-caps mb-1.5 text-fg-subtle">Payload</h3>
            {#if r.payload}
              <CodeBlock title="Payload ({r.payload.mode})" code={r.payload.payload} />
            {:else}
              <p class="text-sm text-fg-muted">{PAYLOAD[r.payloadStatus] ?? r.payloadStatus}</p>
            {/if}
          </section>
        {/if}
      {/snippet}
    </AsyncState>
  </div>
</Sheet>
