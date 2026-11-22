<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // Testlauf (AIGW-52): die ungespeicherte Konfiguration auf einem Beispieltext ausführen
  // (POST /plugin-bindings:test, nur lokale Plugins pii/prompt-filter) und das Ergebnis zeigen – unverändert,
  // maskiert/verändert, gefiltert oder abgelehnt, dazu die Treffer je Kategorie (nur Zähler).
  import { testPlugin, type S } from '../../api/resources';
  import { problemText } from '../errors.svelte';
  import Badge from '../../ui/Badge.svelte';
  import Button from '../../ui/Button.svelte';
  import Field from '../../ui/Field.svelte';
  import Select from '../../ui/Select.svelte';
  import { textarea } from '../../ui/styles';

  let {
    plugin,
    config,
    outputRejectMode = '',
  }: { plugin: string; config: () => Record<string, unknown> | null; outputRejectMode?: string } = $props();

  let text = $state('Bitte an max.mustermann@example.org schreiben, IBAN DE89 3704 0044 0532 0130 00.');
  let phase = $state<S['PluginTestPhase']>('request');
  let busy = $state(false);
  let result = $state<S['PluginTestResult'] | null>(null);
  let error = $state<string | null>(null);

  const OUTCOME: Record<S['PluginTestOutcome'], { label: string; tone: 'success' | 'info' | 'warning' | 'danger' }> = {
    pass: { label: 'unverändert durchgelassen', tone: 'success' },
    modified: { label: 'verändert', tone: 'info' },
    filtered: { label: 'Ausgabe gefiltert (content_filter)', tone: 'warning' },
    rejected: { label: 'abgelehnt', tone: 'danger' },
  };

  async function run() {
    const cfg = config();
    result = null;
    error = null;
    if (!cfg) {
      error = 'Konfiguration ist kein gültiges JSON-Objekt.';
      return;
    }
    busy = true;
    try {
      result = await testPlugin({ plugin, config: cfg as Record<string, never>, phase, text, outputRejectMode: outputRejectMode || null });
    } catch (e) {
      error = problemText(e);
    } finally {
      busy = false;
    }
  }
</script>

<section class="flex flex-col gap-3 rounded-md border border-line bg-surface-2 p-3 sm:col-span-2" data-testid="plugin-test" aria-label="Testlauf">
  <div class="flex flex-wrap items-end gap-3">
    <h3 class="text-sm font-medium">Testlauf</h3>
    <span class="text-xs text-fg-muted">mit der ungespeicherten Konfiguration; nichts wird gespeichert</span>
    <div class="ml-auto w-44"><Select size="sm" label="Phase" bind:value={phase} items={[{ value: 'request', label: 'Eingabe (Anfrage)' }, { value: 'response', label: 'Ausgabe (Antwort)' }]} /></div>
    <Button size="sm" icon="play" loading={busy} onclick={run} data-testid="plugin-test-run">Testen</Button>
  </div>
  <Field label="Beispieltext">
    {#snippet children({ id })}<textarea {id} class={[textarea, 'min-h-16']} bind:value={text} spellcheck="false" data-testid="plugin-test-input"></textarea>{/snippet}
  </Field>
  <div aria-live="polite" data-testid="plugin-test-result">
    {#if error}
      <p class="text-sm text-danger-fg">{error}</p>
    {:else if result}
      {@const o = OUTCOME[result.outcome]}
      <div class="flex flex-col gap-2">
        <div class="flex flex-wrap items-center gap-2">
          <Badge tone={o.tone} dot>{o.label}</Badge>
          {#if result.status}<span class="num text-xs text-fg-muted">HTTP {result.status}{result.code ? ` · ${result.code}` : ''}</span>{/if}
          {#each result.hits as h (h.category + h.action)}<Badge tone="outline" mono>{h.category} · {h.action} × {h.count}</Badge>{/each}
        </div>
        {#if result.message}<p class="text-sm text-fg-muted">{result.message}</p>{/if}
        {#if result.text != null}
          <pre class="max-h-40 overflow-auto whitespace-pre-wrap rounded-md border border-line bg-surface px-3 py-2 font-mono text-sm" data-testid="plugin-test-output">{result.text}</pre>
        {/if}
      </div>
    {/if}
  </div>
</section>
