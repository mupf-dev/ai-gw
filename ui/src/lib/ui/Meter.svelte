<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // Fortschritts-/Anteilsbalken als SVG (Breite als Attribut → keine Inline-Styles, CSP-konform).
  // tone 'auto': ab 80 % Warnung, ab 95 % kritisch (Budgets). Wert steht immer zusätzlich als Text daneben.
  let {
    value,
    label,
    tone = 'accent',
    class: cls = '',
  }: { value: number; label: string; tone?: 'accent' | 'chart' | 'auto'; class?: string } = $props();
  const v = $derived(Math.max(0, Math.min(1, value)));
  const fill = $derived(
    tone === 'auto' ? (v >= 0.95 ? 'fill-danger' : v >= 0.8 ? 'fill-warning' : 'fill-success') : tone === 'chart' ? 'fill-chart-1' : 'fill-accent',
  );
</script>

<svg
  viewBox="0 0 100 6"
  preserveAspectRatio="none"
  class={['h-1.5 w-full overflow-hidden rounded-full', cls]}
  role="meter"
  aria-label={label}
  aria-valuemin={0}
  aria-valuemax={100}
  aria-valuenow={Math.round(v * 100)}
>
  <rect x="0" y="0" width="100" height="6" class="fill-surface-hover" />
  <rect x="0" y="0" width={v * 100} height="6" class={fill} />
</svg>
