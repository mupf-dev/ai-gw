<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts" module>
  /** Eine Zeile des Editors (Formularzustand). */
  export interface IdentityRow {
    issuer: string;
    subject: string;
    description: string;
  }
</script>

<script lang="ts">
  // Workload-/Agent-Identitäten eines API-Keys (AIGW-115): Aussteller aus `auth.jwt.issuers` (auth/info
  // `workloadIssuers`) und Kennung, z. B. die Anwendungs-ID (azp/appid) einer Microsoft-Entra-Agent-Identität.
  // Leere Kennung = Zeile wird beim Speichern verworfen.
  import Icon from '../ui/Icon.svelte';
  import Select from '../ui/Select.svelte';
  import { btn, input } from '../ui/styles';

  let { value = $bindable([]), issuers = [] }: { value?: IdentityRow[]; issuers?: string[] } = $props();
  const items = $derived([...new Set([...issuers, ...value.map((r) => r.issuer).filter(Boolean)])].map((i) => ({ value: i, label: i })));
</script>

<div class="flex flex-col gap-2" data-testid="workload-identities">
  {#if issuers.length === 0}
    <p class="text-sm text-fg-muted">Keine JWT-Aussteller konfiguriert (<code class="font-mono text-xs">auth.jwt.issuers</code>) – Identitäten wirken erst danach.</p>
  {/if}
  {#each value as r, i (i)}
    <div class="grid gap-2 rounded-lg border border-line bg-surface-2 p-2 sm:grid-cols-[9rem_minmax(19rem,2fr)_1fr_auto]">
      {#if items.length}
        <Select label="Aussteller {i + 1}" bind:value={r.issuer} {items} />
      {:else}
        <input class={input} type="text" aria-label="Aussteller {i + 1}" bind:value={r.issuer} placeholder="entra-prod" autocomplete="off" />
      {/if}
      <input class="{input} font-mono" type="text" aria-label="Kennung {i + 1}" bind:value={r.subject} placeholder="Anwendungs-ID (azp/appid)" autocomplete="off" spellcheck="false" />
      <input class={input} type="text" aria-label="Beschreibung {i + 1}" bind:value={r.description} placeholder="z. B. Support-Agent" autocomplete="off" />
      <button type="button" class={btn('ghost', 'icon')} aria-label="Identität {i + 1} entfernen" onclick={() => (value = value.filter((_, j) => j !== i))}><Icon name="x" size={14} /></button>
    </div>
  {/each}
  <div>
    <button type="button" class={btn('secondary', 'sm')} onclick={() => (value = [...value, { issuer: issuers[0] ?? '', subject: '', description: '' }])} data-testid="add-workload-identity">
      <Icon name="plus" size={14} /> Identität hinzufügen
    </button>
  </div>
</div>
