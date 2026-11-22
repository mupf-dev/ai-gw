<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // Formularfelder Payload-Modus (F30) und Reasoning-Ausgabe (F185) für Organisation/Team/Key: „erben“ zeigt den
  // Wert, der dann gilt (von der übergeordneten Ebene bzw. global). Payload-Richtlinie (S-09): Stufen über dem
  // Maximum der Ebenen darüber sind nicht wählbar; hat eine Ebene darüber den Modus gesperrt, ist das Feld deaktiviert.
  // Mit `policy` (Organisation/Team) zusätzlich Maximum und Sperre für die Ebenen darunter.
  import type { S } from '../api/resources';
  import Field from '../ui/Field.svelte';
  import Select from '../ui/Select.svelte';
  import { aboveMax, effective, effectivePayload, PAYLOAD_LABEL, PAYLOAD_ORDER, payloadPolicy, type Parent } from './InheritedSettings.svelte';

  let {
    payloadMode = $bindable(''),
    exposeReasoning = $bindable(''),
    payloadModeMax = $bindable(''),
    payloadModeLocked = $bindable('false'),
    policy = false,
    parents = [],
  }: {
    payloadMode?: string;
    exposeReasoning?: string;
    /** Eigenes Maximum ('' = keines), nur mit `policy`. */
    payloadModeMax?: string;
    /** 'true' | 'false', nur mit `policy`. */
    payloadModeLocked?: string;
    policy?: boolean;
    parents?: Parent[];
  } = $props();

  const HINTS: Record<S['PayloadMode'], string> = {
    off: 'nichts speichern',
    metadata: 'Tokens, Kosten, Status',
    masked: 'Inhalte mit PII-Maskierung',
    full: 'Inhalte im Klartext',
  };
  const pol = $derived(payloadPolicy(parents));
  const pm = $derived(effectivePayload({}, parents));
  const er = $derived(effective('exposeReasoning', {}, parents));
  const from = (f: string | null) => (f === 'global' || f === null ? 'global' : f);
  const pmText = $derived(pm.value ? `${PAYLOAD_LABEL[pm.value]} (${from(pm.from)})` : pm.cap ? `global, höchstens ${PAYLOAD_LABEL[pm.cap]}` : 'global: observe.payload');
  const erText = $derived(er.value === false ? `nein (${from(er.from)})` : er.value === true ? `ja (${from(er.from)})` : 'ja (Standard)');
  // Engstes Maximum für den eigenen Modus: übergeordnete Ebenen und (Organisation/Team) das eigene.
  const ownMax = $derived((policy && payloadModeMax ? payloadModeMax : null) as S['PayloadMode'] | null);
  const modeMax = $derived(pol.max && ownMax ? (aboveMax(ownMax, pol.max) ? pol.max : ownMax) : (pol.max ?? ownMax));
  const modeMaxFrom = $derived(modeMax && modeMax === pol.max ? pol.maxFrom : 'eigenes Maximum');
  const locked = $derived(pol.lockedBy !== null);
  const payloadHint = $derived(
    locked
      ? `Gesperrt durch ${pol.lockedBy} – es gilt der dort festgelegte Wert.${payloadMode ? ' Eigenen Wert auf „erben“ zurücksetzen.' : ''}`
      : modeMax
        ? `Höchstens „${PAYLOAD_LABEL[modeMax]}“ (Maximum: ${modeMaxFrom}). Ohne Angabe gilt der geerbte Wert.`
        : 'Was das Request-Log von Anfrage/Antwort speichert. Ohne Angabe gilt der geerbte Wert.',
  );
  const modeItems = $derived([
    { value: '', label: `erben – ${pmText}` },
    ...PAYLOAD_ORDER.map((m) => {
      const over = aboveMax(m, modeMax);
      return { value: m, label: PAYLOAD_LABEL[m], hint: locked ? 'gesperrt' : over ? `über dem Maximum (${modeMaxFrom})` : HINTS[m], disabled: locked || over };
    }),
  ]);
  const maxItems = $derived([
    { value: '', label: pol.max ? `keines – es gilt ${PAYLOAD_LABEL[pol.max]} (${pol.maxFrom})` : 'keines' },
    ...PAYLOAD_ORDER.map((m) => {
      const over = aboveMax(m, pol.max);
      return { value: m, label: `höchstens ${PAYLOAD_LABEL[m]}`, hint: over ? `über dem Maximum (${pol.maxFrom})` : undefined, disabled: over };
    }),
  ]);
</script>

<Field label="Payload-Logging" optional hint={payloadHint}>
  {#snippet children({ id, describedby })}
    <span class="contents" data-testid="payload-mode-field" data-locked={locked || undefined}>
      <Select {id} {describedby} bind:value={payloadMode} disabled={locked && !payloadMode} items={modeItems} />
    </span>
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
{#if policy}
  <Field label="Payload-Maximum" optional hint="Obergrenze für diese und alle Ebenen darunter (aus < Metadaten < maskiert < vollständig). Wird auch zur Laufzeit erzwungen.">
    {#snippet children({ id, describedby })}
      <span class="contents" data-testid="payload-max-field">
        <Select {id} {describedby} bind:value={payloadModeMax} items={maxItems} />
      </span>
    {/snippet}
  </Field>
  <Field label="Payload-Modus darunter" hint="Gesperrt: Teams und Keys darunter können den Payload-Modus nicht setzen (auch nicht absenken).">
    {#snippet children({ id, describedby })}
      <span class="contents" data-testid="payload-locked-field">
        <Select
          {id}
          {describedby}
          bind:value={payloadModeLocked}
          items={[
            { value: 'false', label: 'änderbar' },
            { value: 'true', label: 'gesperrt' },
          ]}
        />
      </span>
    {/snippet}
  </Field>
{/if}
