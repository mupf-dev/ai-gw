<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts" module>
  import type { S } from '../api/resources';

  export interface Inheritable {
    payloadMode?: S['PayloadMode'] | null;
    exposeReasoning?: boolean | null;
  }
  export interface Parent {
    label: string;
    spec: Inheritable | null;
  }
  export const PAYLOAD_LABEL: Record<S['PayloadMode'], string> = {
    off: 'aus',
    metadata: 'nur Metadaten',
    masked: 'maskiert',
    full: 'vollständig',
  };

  /** Wirksamer Wert samt Herkunft (Key → Team → Organisation → global). */
  export function effective<K extends keyof Inheritable>(key: K, spec: Inheritable, parents: Parent[]): { value: Inheritable[K] | null; from: string | null } {
    if (spec[key] != null) return { value: spec[key], from: null };
    for (const p of parents) if (p.spec?.[key] != null) return { value: p.spec[key], from: p.label };
    return { value: null, from: 'global' };
  }
</script>

<script lang="ts">
  // Payload-Modus (F30) und Reasoning-Ausgabe (F185) mit Vererbung: eigener Wert oder geerbter Wert mit Herkunft.
  // Rendert <dt>/<dd>-Paare für eine umgebende <dl>.
  let { spec, parents = [] }: { spec: Inheritable; parents?: Parent[] } = $props();
  const pm = $derived(effective('payloadMode', spec, parents));
  const er = $derived(effective('exposeReasoning', spec, parents));
</script>

<dt class="text-fg-muted">Payload-Logging</dt>
<dd class="text-sm" data-testid="eff-payload">
  {#if pm.value}{PAYLOAD_LABEL[pm.value as S['PayloadMode']]}{:else}Standard (<span class="font-mono">observe.payload</span>){/if}
  <span class="text-fg-subtle">{pm.from === null ? '· eigener Wert' : pm.from === 'global' ? '· global' : `· geerbt von ${pm.from}`}</span>
</dd>
<dt class="text-fg-muted">Reasoning ausgeben</dt>
<dd class="text-sm" data-testid="eff-reasoning">
  {er.value === false ? 'nein' : er.value === true ? 'ja' : 'ja (Standard, Modellgruppe entscheidet)'}
  <span class="text-fg-subtle">{er.from === null ? '· eigener Wert' : er.from === 'global' ? '' : `· geerbt von ${er.from}`}</span>
</dd>
