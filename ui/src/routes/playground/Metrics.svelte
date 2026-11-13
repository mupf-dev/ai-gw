<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Kennzahlen einer Playground-Antwort: sofort aus Antwort und Headern (TTFT/Latenz im Browser, Tokens,
  // Deployment, Cache), danach aus dem Request-Log (Kosten, Server-Latenz/TTFT, Guardrail-Treffer). Der
  // Request-Log-Eintrag wird asynchron geschrieben – daher kurz nachfragen. Request-ID mit Link zum Log-Explorer.
  import { amount, requestLogs, type RequestLogItem } from '../../lib/api/observe';
  import { ms, num } from '../../lib/format';
  import { isMock } from '../../lib/mode.svelte';
  import { href } from '../../lib/router.svelte';
  import Badge from '../../lib/ui/Badge.svelte';
  import CopyButton from '../../lib/ui/CopyButton.svelte';
  import type { Outcome } from './chat';

  let { outcome }: { outcome: Outcome } = $props();
  let log = $state<RequestLogItem | null>(null);
  // Nicht reaktiv: je Request-ID genau eine Abfrage-Schleife (Liste mit Filter – 200 auch solange der Eintrag
  // noch fehlt, daher keine 404-Meldungen in der Konsole).
  let polled: string | null = null;

  $effect(() => {
    const id = outcome.requestId;
    if (!outcome.done || !id || polled === id || isMock()) return;
    polled = id;
    void (async () => {
      for (let i = 0; i < 15 && polled === id; i++) {
        await new Promise((r) => setTimeout(r, 300 + i * 200));
        try {
          const page = await requestLogs({ requestId: id }, 1);
          if (page.items[0]) {
            log = page.items[0];
            return;
          }
        } catch {
          return;
        }
      }
    })();
  });

  const u = $derived(outcome.usage);
  const input = $derived(log?.inputTokens ?? u?.prompt_tokens ?? null);
  const output = $derived(log?.outputTokens ?? u?.completion_tokens ?? null);
  const cache = $derived(log?.cache ?? outcome.cache);
  const deployment = $derived(log?.deployment ?? outcome.deployment);
</script>

<dl class="flex flex-wrap items-center gap-x-4 gap-y-1 text-xs text-fg-muted" data-testid="pg-metrics">
  {#if outcome.ttftMs !== null}<div class="flex gap-1"><dt>TTFT</dt><dd class="num text-fg" data-testid="pg-ttft">{ms(outcome.ttftMs)}</dd></div>{/if}
  {#if outcome.latencyMs !== null}<div class="flex gap-1"><dt>Latenz</dt><dd class="num text-fg" data-testid="pg-latency">{ms(outcome.latencyMs)}</dd></div>{/if}
  {#if input !== null || output !== null}
    <div class="flex gap-1"><dt>Tokens</dt><dd class="num text-fg" data-testid="pg-tokens">{num(input ?? 0)} → {num(output ?? 0)}</dd></div>
  {/if}
  {#if u?.completion_tokens_details?.reasoning_tokens}<div class="flex gap-1"><dt>Reasoning</dt><dd class="num text-fg">{num(u.completion_tokens_details.reasoning_tokens)}</dd></div>{/if}
  {#if log}<div class="flex gap-1"><dt>Kosten</dt><dd class="num text-fg" data-testid="pg-cost">{amount(log.cost)}</dd></div>{/if}
  {#if log?.ttftMs != null}<div class="flex gap-1" title="vom Gateway gemessen"><dt>TTFT (Gateway)</dt><dd class="num text-fg">{ms(log.ttftMs)}</dd></div>{/if}
  {#if deployment}<div class="flex gap-1"><dt>Deployment</dt><dd class="font-mono text-fg" data-testid="pg-deployment">{deployment}</dd></div>{/if}
  {#if outcome.fallback}<div class="flex gap-1"><dt>Fallback</dt><dd class="font-mono text-fg">{outcome.fallback}</dd></div>{/if}
  {#if outcome.attempts && outcome.attempts !== '1'}<div class="flex gap-1"><dt>Versuche</dt><dd class="num text-fg">{outcome.attempts}</dd></div>{/if}
  {#if cache}<div><dt class="sr-only">Cache</dt><dd data-testid="pg-cache"><Badge tone={cache.startsWith('hit') ? 'success' : 'outline'} mono>cache: {cache}</Badge></dd></div>{/if}
  {#if log?.guardrails?.length}
    <div class="flex flex-wrap gap-1">
      <dt class="sr-only">Guardrails</dt>
      {#each log.guardrails as g, i (i)}
        <dd data-testid="pg-guardrail"><Badge tone={g.action === 'reject' ? 'danger' : 'warning'} mono>{g.plugin}: {g.category} ×{g.count} ({g.action})</Badge></dd>
      {/each}
    </div>
  {/if}
  {#if outcome.requestId}
    <div class="flex items-center gap-1">
      <dt class="sr-only">Request-ID</dt>
      <dd class="flex items-center gap-1">
        <a class="font-mono text-accent-text hover:underline" href={href(`/logs?requestId=${encodeURIComponent(outcome.requestId)}`)} data-testid="pg-request-link">{outcome.requestId}</a>
        <CopyButton text={outcome.requestId} label="Request-ID kopieren" />
      </dd>
    </div>
  {/if}
</dl>
