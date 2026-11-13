<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts" module>
  export interface TreeItem {
    key: string;
    label: string;
    value: number;
    text: string;
  }
</script>

<script lang="ts">
  // Kosten-Treemap (F18, LayerChart – nur per dynamic import geladen): Fläche = Anteil an den Kosten im Zeitraum.
  // Rechtecke und Beschriftung als SVG-Attribute (keine Inline-Styles, CSP), Farben aus den Diagramm-Tokens in
  // fester Reihenfolge; die Tabelle daneben trägt dieselben Werte für Bildschirmleser.
  import { hierarchy, type HierarchyRectangularNode } from 'd3-hierarchy';
  import { Chart, Svg } from 'layerchart';
  import { Treemap } from 'layerchart/hierarchy';

  let { items, label }: { items: TreeItem[]; label: string } = $props();

  type Node = { key: string; label: string; value: number; text: string; children?: Node[] };
  const root = $derived(
    hierarchy<Node>({ key: '__root__', label, value: 0, text: '', children: items.filter((i) => i.value > 0) })
      .sum((d) => (d.children ? 0 : d.value))
      .sort((a, b) => (b.value ?? 0) - (a.value ?? 0)),
  );
  const FILL = ['fill-chart-1', 'fill-chart-2', 'fill-chart-3', 'fill-chart-4', 'fill-chart-5', 'fill-chart-6'];
  const fillOf = (key: string) => FILL[Math.max(0, items.findIndex((i) => i.key === key)) % FILL.length];
</script>

<figure class="h-56 w-full" aria-label={label} data-testid="cost-treemap">
  <Chart>
    <Svg>
      <Treemap hierarchy={root} padding={2}>
        {#snippet children({ nodes }: { nodes: HierarchyRectangularNode<Node>[] })}
          {#each nodes.filter((n) => n.depth === 1) as n (n.data.key)}
            {@const w = n.x1 - n.x0}
            {@const h = n.y1 - n.y0}
            <g transform="translate({n.x0},{n.y0})">
              <rect width={Math.max(0, w)} height={Math.max(0, h)} rx="3" class={[fillOf(n.data.key), 'opacity-85']}><title>{n.data.label}: {n.data.text}</title></rect>
              {#if w > 64 && h > 30}
                <text x="6" y="16" class="fill-on-accent text-xs font-medium">{n.data.label.length > w / 7 ? `${n.data.label.slice(0, Math.floor(w / 7) - 1)}…` : n.data.label}</text>
                <text x="6" y="30" class="num fill-on-accent text-2xs">{n.data.text}</text>
              {/if}
            </g>
          {/each}
        {/snippet}
      </Treemap>
    </Svg>
  </Chart>
</figure>
