<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // KPI-Kachel: Kennzahl groß, Veränderung zur Vorperiode (Pfeil + Text, Bewertung über `better`),
  // optional Mini-Verlauf. Die Veränderung ist immer auch als Text lesbar (nicht nur Farbe).
  import Icon, { type IconName } from './Icon.svelte';
  import Sparkline from './Sparkline.svelte';
  import { card } from './styles';

  let {
    label,
    value,
    unit,
    icon,
    delta,
    better = 'up',
    trend,
    hint,
    testid,
  }: {
    label: string;
    value: string;
    unit?: string;
    icon?: IconName;
    /** relative Änderung, z. B. 0.12 = +12 % */
    delta?: number;
    better?: 'up' | 'down';
    trend?: number[];
    hint?: string;
    testid?: string;
  } = $props();

  const good = $derived(delta === undefined ? null : delta === 0 ? null : (delta > 0) === (better === 'up'));
  const deltaText = $derived(
    delta === undefined
      ? ''
      : `${delta > 0 ? '+' : delta < 0 ? '−' : '±'}${Math.abs(delta * 100).toLocaleString('de-DE', { maximumFractionDigits: 1 })} %`,
  );
</script>

<div class={[card, 'flex min-w-0 flex-col gap-2 p-4']} data-testid={testid}>
  <div class="flex items-center gap-2 text-sm text-fg-muted">
    {#if icon}<Icon name={icon} size={14} />{/if}
    <span class="truncate">{label}</span>
  </div>
  <span class="flex min-w-0 items-baseline gap-1">
    <span class="num truncate text-2xl font-semibold xl:text-xl 2xl:text-2xl text-fg">{value}</span>
    {#if unit}<span class="text-sm text-fg-muted">{unit}</span>{/if}
  </span>
  {#if delta !== undefined}
    <span class="flex items-center gap-1 text-xs whitespace-nowrap">
      <span class={['inline-flex items-center gap-0.5 font-medium', good === null ? 'text-fg-muted' : good ? 'text-success-fg' : 'text-danger-fg']}>
        <Icon name={delta >= 0 ? 'arrow-up' : 'arrow-down'} size={12} />
        <span class="num">{deltaText}</span>
      </span>
      <span class="truncate text-fg-muted">ggü. Vorperiode</span>
    </span>
  {:else if hint}
    <span class="text-xs text-fg-muted">{hint}</span>
  {/if}
  {#if trend}
    <Sparkline values={trend} class={good === false ? 'mt-1 text-danger' : 'mt-1 text-chart-1'} />
  {/if}
</div>
