<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts" module>
  import type { Outcome } from './chat';
  export interface Turn {
    prompt: string;
    model: string;
    outcome: Outcome | null;
    /** Fehler der Admin-API (Anmeldung, Rechte, Key), nicht der Datenebene. */
    adminError: string | null;
  }
</script>

<script lang="ts">
  // Eine Spalte des Playgrounds: Modellwahl und Verlauf (Nutzer-Nachrichten, Antworten als sicheres Markdown,
  // Reasoning einklappbar, falls die Gruppe es ausgibt) mit Kennzahlen je Antwort.
  import type { Schemas } from '../../lib/api/client';
  import Button from '../../lib/ui/Button.svelte';
  import Card from '../../lib/ui/Card.svelte';
  import Icon from '../../lib/ui/Icon.svelte';
  import Select, { type SelectItem } from '../../lib/ui/Select.svelte';
  import Markdown from './Markdown.svelte';
  import Metrics from './Metrics.svelte';

  let {
    model = $bindable(''),
    turns,
    modelItems,
    models,
    removable,
    onremove,
    disabled,
  }: {
    model?: string;
    turns: Turn[];
    modelItems: SelectItem[];
    models: Schemas['PlaygroundModel'][];
    removable: boolean;
    onremove: () => void;
    disabled: boolean;
  } = $props();

  const info = $derived(models.find((m) => m.name === model));
</script>

<Card testid="pg-pane">
  <div class="flex min-w-0 flex-col gap-3">
    <div class="flex items-center gap-2">
      <div class="min-w-0 flex-1">
        <Select bind:value={model} items={modelItems} label="Modellgruppe" placeholder="Modellgruppe wählen …" icon="route" size="sm" />
      </div>
      {#if removable}<Button size="icon-sm" variant="ghost" icon="x" aria-label="Spalte entfernen" onclick={onremove} {disabled} />{/if}
    </div>
    {#if info}
      <p class="text-xs text-fg-muted">
        <span class="num">{info.deployments}</span> Deployment(s){#if info.exposeReasoning} · Reasoning sichtbar{:else} · Reasoning ausgeblendet{/if}
      </p>
    {/if}

    {#if !turns.length}
      <p class="py-6 text-center text-sm text-fg-subtle">Noch keine Nachrichten.</p>
    {/if}
    {#each turns as t, i (i)}
      <div class="flex flex-col gap-2" data-testid="pg-turn">
        <div class="self-end rounded-lg bg-accent-soft px-3 py-2 text-base whitespace-pre-wrap text-accent-soft-fg">{t.prompt}</div>
        {#if t.adminError}
          <p class="flex gap-2 rounded-md bg-danger-bg px-3 py-2 text-sm text-danger-fg" data-testid="pg-error"><Icon name="alert" size={14} class="mt-0.5" />{t.adminError}</p>
        {:else if t.outcome}
          {@const o = t.outcome}
          {#each o.choices.length ? o.choices : [{ content: '', reasoning: '', finish: null }] as c, ci (ci)}
            <div class="flex flex-col gap-2 rounded-lg border border-line bg-surface-2 px-3 py-2" data-testid="pg-answer">
              {#if o.choices.length > 1}<span class="label-caps text-fg-subtle">Variante {ci + 1}</span>{/if}
              {#if c.reasoning && info?.exposeReasoning !== false}
                <details class="text-sm text-fg-muted" data-testid="pg-reasoning">
                  <summary class="cursor-pointer select-none">Reasoning</summary>
                  <p class="mt-1 whitespace-pre-wrap">{c.reasoning}</p>
                </details>
              {/if}
              {#if c.content}
                <Markdown text={c.content} />
              {:else if !o.done}
                <span class="inline-flex items-center gap-2 text-sm text-fg-muted"><span class="size-2 animate-pulse rounded-full bg-accent"></span>wartet auf Antwort …</span>
              {/if}
              {#if c.finish && c.finish !== 'stop'}<span class="text-xs text-warning-fg">finish_reason: {c.finish}</span>{/if}
            </div>
          {/each}
          {#if o.error}
            <p class="flex gap-2 rounded-md bg-danger-bg px-3 py-2 text-sm text-danger-fg" data-testid="pg-error">
              <Icon name="alert" size={14} class="mt-0.5" /><span>{#if o.status}<span class="num">{o.status}</span> · {/if}{o.error.message}{#if o.error.code} <span class="font-mono text-xs">({o.error.code})</span>{/if}</span>
            </p>
          {/if}
          <Metrics outcome={o} />
        {:else}
          <span class="inline-flex items-center gap-2 text-sm text-fg-muted"><span class="size-2 animate-pulse rounded-full bg-accent"></span>wartet auf Antwort …</span>
        {/if}
      </div>
    {/each}
  </div>
</Card>
