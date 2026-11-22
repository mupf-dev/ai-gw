<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts" module>
  import type { S } from '../api/resources';

  export interface Inheritable {
    payloadMode?: S['PayloadMode'] | null;
    exposeReasoning?: boolean | null;
    /** Obergrenze für payloadMode (S-09; nur Organisation/Team). */
    payloadModeMax?: S['PayloadMode'] | null;
    /** payloadMode darunter gesperrt (S-09; nur Organisation/Team). */
    payloadModeLocked?: boolean;
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

  /** Stufen in Reihenfolge off < metadata < masked < full (S-09). */
  export const PAYLOAD_ORDER: S['PayloadMode'][] = ['off', 'metadata', 'masked', 'full'];
  const rank = (m: S['PayloadMode']) => PAYLOAD_ORDER.indexOf(m);

  /** Payload-Richtlinie der übergeordneten Ebenen (parents: nächste zuerst): engstes Maximum und Sperre. */
  export function payloadPolicy(parents: Parent[]): { max: S['PayloadMode'] | null; maxFrom: string | null; lockedBy: string | null } {
    let max: S['PayloadMode'] | null = null;
    let maxFrom: string | null = null;
    let lockedBy: string | null = null;
    for (const p of parents) {
      const m = p.spec?.payloadModeMax;
      if (m && (max === null || rank(m) < rank(max))) {
        max = m;
        maxFrom = p.label;
      }
    }
    // Die oberste sperrende Ebene gewinnt.
    for (const p of [...parents].reverse()) if (p.spec?.payloadModeLocked) return { max, maxFrom, lockedBy: p.label };
    return { max, maxFrom, lockedBy };
  }

  /** `true`, wenn `mode` über `max` liegt. */
  export function aboveMax(mode: S['PayloadMode'], max: S['PayloadMode'] | null | undefined): boolean {
    return max != null && rank(mode) > rank(max);
  }

  /**
   * Wirksamer Payload-Modus wie im Gateway (S-09): von oben nach unten der spezifischste Wert bis zur ersten gesperrten
   * Ebene, begrenzt auf das engste Maximum. `value: null` = global (observe.payload), ggf. mit `cap`.
   */
  export function effectivePayload(
    spec: Inheritable,
    parents: Parent[],
  ): { value: S['PayloadMode'] | null; from: string | null; cap: S['PayloadMode'] | null; capFrom: string | null; clamped: boolean } {
    const chain = [...[...parents].reverse(), { label: null as string | null, spec }];
    let value: S['PayloadMode'] | null = null;
    let from: string | null = 'global';
    let cap: S['PayloadMode'] | null = null;
    let capFrom: string | null = null;
    let locked = false;
    for (const l of chain) {
      const s = l.spec;
      if (!s) continue;
      if (!locked && s.payloadMode) {
        value = s.payloadMode;
        from = l.label;
      }
      if (s.payloadModeMax && (cap === null || rank(s.payloadModeMax) < rank(cap))) {
        cap = s.payloadModeMax;
        capFrom = l.label;
      }
      locked ||= !!s.payloadModeLocked;
    }
    const clamped = value !== null && aboveMax(value, cap);
    return { value: clamped ? cap : value, from, cap, capFrom, clamped };
  }

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
  const pm = $derived(effectivePayload(spec, parents));
  const er = $derived(effective('exposeReasoning', spec, parents));
</script>

<dt class="text-fg-muted">Payload-Logging</dt>
<dd class="text-sm" data-testid="eff-payload">
  {#if pm.value}{PAYLOAD_LABEL[pm.value]}{:else}Standard (<span class="font-mono">observe.payload</span>){/if}
  <span class="text-fg-subtle">{pm.from === null ? '· eigener Wert' : pm.from === 'global' ? '· global' : `· geerbt von ${pm.from}`}</span>
  {#if pm.clamped}<span class="text-warning-fg" data-testid="eff-payload-clamped"> · begrenzt durch Maximum{pm.capFrom ? ` von ${pm.capFrom}` : ''}</span>{/if}
  {#if pm.cap || spec.payloadModeLocked}
    <span class="block text-xs text-fg-muted" data-testid="eff-payload-policy">
      {#if pm.cap}Maximum: {PAYLOAD_LABEL[pm.cap]}{pm.capFrom ? ` (${pm.capFrom})` : ''}{/if}{#if pm.cap && spec.payloadModeLocked} · {/if}{#if spec.payloadModeLocked}darunter gesperrt{/if}
    </span>
  {/if}
</dd>
<dt class="text-fg-muted">Reasoning ausgeben</dt>
<dd class="text-sm" data-testid="eff-reasoning">
  {er.value === false ? 'nein' : er.value === true ? 'ja' : 'ja (Standard, Modellgruppe entscheidet)'}
  <span class="text-fg-subtle">{er.from === null ? '· eigener Wert' : er.from === 'global' ? '' : `· geerbt von ${er.from}`}</span>
</dd>
