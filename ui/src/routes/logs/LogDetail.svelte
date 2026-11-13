<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Detail-Sheet eines Requests: Übersicht (Metadaten, Versuche/Fallbacks), Request, Response, Zeitleiste.
  import { Tabs } from 'bits-ui';
  import { dateTime, money, ms, num } from '../../lib/format';
  import { PROVIDERS, teamName } from '../../lib/mock/data';
  import type { LogEntry } from '../../lib/mock/types';
  import Badge from '../../lib/ui/Badge.svelte';
  import CodeBlock from '../../lib/ui/CodeBlock.svelte';
  import CopyButton from '../../lib/ui/CopyButton.svelte';
  import Icon from '../../lib/ui/Icon.svelte';
  import Sheet from '../../lib/ui/Sheet.svelte';
  import { tabsList, tabsTrigger } from '../../lib/ui/styles';

  let { entry, open = $bindable(false) }: { entry: LogEntry | null; open?: boolean } = $props();
  let tab = $state('overview');
  $effect(() => {
    if (entry) tab = 'overview';
  });

  const statusTone = (s: number) => (s < 300 ? 'success' : s === 429 ? 'warning' : 'danger') as 'success' | 'warning' | 'danger';
  const total = $derived(entry ? entry.stages.reduce((a, s) => a + s.ms, 0) : 1);
  const offsets = $derived.by(() => {
    let acc = 0;
    return (entry?.stages ?? []).map((s) => {
      const o = acc;
      acc += s.ms;
      return o;
    });
  });
</script>

<Sheet bind:open title="Request-Details" size="lg" testid="log-detail">
  {#snippet header()}
    {#if entry}
      <div class="mt-2 flex flex-wrap items-center gap-2">
        <Badge tone={statusTone(entry.status)} mono>{entry.status}</Badge>
        <span class="font-mono text-sm text-fg">{entry.model}</span>
        <span class="text-sm text-fg-muted">{dateTime(entry.ts)}</span>
      </div>
      <div class="mt-1 flex items-center gap-1 font-mono text-xs text-fg-muted">
        {entry.id}
        <CopyButton text={entry.id} label="Request-ID kopieren" />
      </div>
    {/if}
  {/snippet}
  {#if entry}
    <Tabs.Root bind:value={tab}>
      <Tabs.List class="{tabsList} sticky top-0 z-10 bg-surface px-5" aria-label="Detailansicht">
        <Tabs.Trigger value="overview" class={tabsTrigger}>Übersicht</Tabs.Trigger>
        <Tabs.Trigger value="request" class={tabsTrigger}>Request</Tabs.Trigger>
        <Tabs.Trigger value="response" class={tabsTrigger}>Response</Tabs.Trigger>
        <Tabs.Trigger value="timeline" class={tabsTrigger}>Zeitleiste</Tabs.Trigger>
      </Tabs.List>

      <Tabs.Content value="overview" class="flex flex-col gap-5 p-5">
        <dl class="grid grid-cols-2 gap-x-6 gap-y-3 text-base sm:grid-cols-3">
          {#each [['Endpunkt', entry.endpoint, true], ['Eingangsformat', entry.inputFormat === 'openai' ? 'OpenAI' : 'Anthropic Messages', false], ['Key', entry.keyName, true], ['Team', teamName(entry.teamId), false], ['Anbieter', PROVIDERS[entry.provider].name, false], ['Deployment', entry.deployment, true], ['Tokens (ein / aus)', `${num(entry.inTok)} / ${num(entry.outTok)}`, false], ['Kosten', money(entry.cost), false], ['Latenz gesamt', ms(entry.latency), false], ['Zeit bis 1. Token', ms(entry.ttfb), false], ['Cache', entry.cache === 'hit' ? 'Treffer (exakt)' : entry.cache === 'semantic' ? 'Treffer (semantisch)' : 'kein Treffer', false], ['Streaming', entry.stream ? 'ja (SSE)' : 'nein', false]] as [label, value, mono] (label)}
            <div class="min-w-0">
              <dt class="text-sm text-fg-muted">{label}</dt>
              <dd class={['mt-0.5 truncate', mono ? 'font-mono text-sm' : 'tabular-nums']}>{value}</dd>
            </div>
          {/each}
        </dl>

        <section>
          <h3 class="mb-2 text-base font-semibold">Versuche</h3>
          <ol class="flex flex-col">
            {#each entry.attempts as a, i (i)}
              <li class="relative flex gap-3 pb-3 last:pb-0">
                {#if i < entry.attempts.length - 1}<span class="absolute top-6 bottom-0 left-[11px] w-px bg-line" aria-hidden="true"></span>{/if}
                <span class={['grid size-6 shrink-0 place-items-center rounded-full', a.status < 300 ? 'bg-success-bg text-success-fg' : 'bg-danger-bg text-danger-fg']}>
                  <Icon name={a.status < 300 ? 'check' : 'x'} size={12} />
                </span>
                <div class="min-w-0 flex-1 rounded-md border border-line px-3 py-2">
                  <div class="flex flex-wrap items-center gap-2">
                    <span class="font-mono text-sm">{a.deployment}</span>
                    <Badge tone={statusTone(a.status)} mono>{a.status}</Badge>
                    <span class="num ml-auto text-sm text-fg-muted">{ms(a.latency)}</span>
                  </div>
                  <div class="mt-0.5 text-xs text-fg-muted">
                    {PROVIDERS[a.provider].name}{#if a.error} · <span class="text-danger-fg">{a.error}</span>{/if}
                    {#if i > 0} · Fallback/Retry {i}{/if}
                  </div>
                </div>
              </li>
            {/each}
          </ol>
        </section>
      </Tabs.Content>

      <Tabs.Content value="request" class="p-5">
        <CodeBlock code={entry.request} title="{entry.endpoint} · Eingang" maxHeight="max-h-[60vh]" />
        <p class="mt-2 text-xs text-fg-muted">Sensible Inhalte werden gemäß Aufbewahrungsrichtlinie maskiert oder nicht gespeichert.</p>
      </Tabs.Content>
      <Tabs.Content value="response" class="p-5">
        <CodeBlock code={entry.response} title="Antwort an den Client" maxHeight="max-h-[60vh]" />
      </Tabs.Content>

      <Tabs.Content value="timeline" class="p-5">
        <ul class="flex flex-col gap-2" aria-label="Zeitanteile der Verarbeitungsschritte">
          {#each entry.stages as s, i (s.name)}
            <li class="grid grid-cols-[minmax(0,11rem)_1fr_4.5rem] items-center gap-3 text-sm">
              <span class="truncate text-fg-muted">{s.name}</span>
              <svg viewBox="0 0 100 8" preserveAspectRatio="none" class="h-2 w-full" aria-hidden="true">
                <rect x="0" y="0" width="100" height="8" class="fill-surface-2" />
                <rect
                  x={(offsets[i] / total) * 100}
                  y="0"
                  width={Math.max((s.ms / total) * 100, 0.6)}
                  height="8"
                  class={s.name === 'Upstream' ? 'fill-chart-1' : 'fill-chart-3'}
                />
              </svg>
              <span class="num text-right text-fg">{ms(s.ms)}</span>
            </li>
          {/each}
        </ul>
        <p class="mt-4 flex items-center gap-4 text-xs text-fg-muted">
          <span class="inline-flex items-center gap-1.5"><span class="size-2 rounded-full bg-chart-1"></span>Upstream</span>
          <span class="inline-flex items-center gap-1.5"><span class="size-2 rounded-full bg-chart-3"></span>Gateway</span>
          <span class="num ml-auto">Gesamt {ms(entry.latency)}</span>
        </p>
      </Tabs.Content>
    </Tabs.Root>
  {/if}
</Sheet>
