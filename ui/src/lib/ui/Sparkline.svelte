<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // Mini-Verlauf als SVG (Geometrie nur über Attribute → CSP-konform). Dekorativ: Wert steht als Text daneben.
  let { values, class: cls = '' }: { values: number[]; class?: string } = $props();
  const W = 120;
  const H = 32;
  const path = $derived.by(() => {
    if (values.length < 2) return { line: '', area: '' };
    const min = Math.min(...values);
    const max = Math.max(...values);
    const span = max - min || 1;
    const pts = values.map((v, i) => [(i / (values.length - 1)) * W, H - 2 - ((v - min) / span) * (H - 4)] as const);
    const line = pts.map(([x, y], i) => `${i ? 'L' : 'M'}${x.toFixed(1)},${y.toFixed(1)}`).join('');
    return { line, area: `${line}L${W},${H}L0,${H}Z` };
  });
</script>

<svg viewBox="0 0 {W} {H}" preserveAspectRatio="none" class={['block h-7 w-full', cls]} aria-hidden="true">
  <path d={path.area} class="fill-current opacity-10" />
  <path d={path.line} class="fill-none stroke-current" stroke-width="1.5" vector-effect="non-scaling-stroke" stroke-linejoin="round" />
</svg>
