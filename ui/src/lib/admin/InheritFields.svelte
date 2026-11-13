<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Formularfelder Payload-Modus (F30) und Reasoning-Ausgabe (F185) für Organisation/Team/Key: „erben“ zeigt den
  // Wert, der dann gilt (von der übergeordneten Ebene bzw. global).
  import type { S } from '../api/resources';
  import Field from '../ui/Field.svelte';
  import Select from '../ui/Select.svelte';
  import { effective, PAYLOAD_LABEL, type Parent } from './InheritedSettings.svelte';

  let {
    payloadMode = $bindable(''),
    exposeReasoning = $bindable(''),
    parents = [],
  }: { payloadMode?: string; exposeReasoning?: string; parents?: Parent[] } = $props();

  const pm = $derived(effective('payloadMode', {}, parents));
  const er = $derived(effective('exposeReasoning', {}, parents));
  const from = (f: string | null) => (f === 'global' || f === null ? 'global' : f);
  const pmText = $derived(pm.value ? `${PAYLOAD_LABEL[pm.value as S['PayloadMode']]} (${from(pm.from)})` : 'global: observe.payload');
  const erText = $derived(er.value === false ? `nein (${from(er.from)})` : er.value === true ? `ja (${from(er.from)})` : 'ja (Standard)');
</script>

<Field label="Payload-Logging" optional hint="Was das Request-Log von Anfrage/Antwort speichert. Ohne Angabe gilt der geerbte Wert.">
  {#snippet children({ id, describedby })}
    <Select
      {id}
      {describedby}
      bind:value={payloadMode}
      items={[
        { value: '', label: `erben – ${pmText}` },
        { value: 'off', label: 'aus', hint: 'nichts speichern' },
        { value: 'metadata', label: 'nur Metadaten', hint: 'Tokens, Kosten, Status' },
        { value: 'masked', label: 'maskiert', hint: 'Inhalte mit PII-Maskierung' },
        { value: 'full', label: 'vollständig', hint: 'Inhalte im Klartext' },
      ]}
    />
  {/snippet}
</Field>
<Field label="Reasoning ausgeben" optional hint="reasoning_content an OpenAI-Clients; die Modellgruppe muss es zusätzlich erlauben.">
  {#snippet children({ id, describedby })}
    <Select
      {id}
      {describedby}
      bind:value={exposeReasoning}
      items={[
        { value: '', label: `erben – ${erText}` },
        { value: 'true', label: 'ja' },
        { value: 'false', label: 'nein' },
      ]}
    />
  {/snippet}
</Field>
