<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts" module>
  export interface StackSeries {
    key: string;
    label: string;
    values: number[];
  }
</script>

<script lang="ts">
  // Gestapelte Flächen (F18, LayerChart – nur per dynamic import geladen), z. B. Tokens nach Art (Eingabe/Ausgabe).
  // Stapel werden hier berechnet (y0/y1 je Serie); Farben aus den Diagramm-Tokens in fester Reihenfolge.
  // Achsenbeschriftung über die lc-Klassen (app.css), keine Inline-Styles (CSP).
  import { Area, Axis, Chart, Svg } from 'layerchart';
  import { clockTime, short } from '../format';

  let { t, series, label, height = 'h-56' }: { t: number[]; series: StackSeries[]; label: string; height?: string } = $props();

  type Row = { x: number; y0: number[]; y1: number[]; top: number };
  const rows = $derived<Row[]>(
    t.map((x, i) => {
      let acc = 0;
      const y0: number[] = [];
      const y1: number[] = [];
      for (const s of series) {
        y0.push(acc);
        acc += s.values[i] ?? 0;
        y1.push(acc);
      }
      return { x: x * 1000, y0, y1, top: acc };
    }),
  );
  const FILL = ['fill-chart-1', 'fill-chart-2', 'fill-chart-3', 'fill-chart-4', 'fill-chart-5', 'fill-chart-6'];
  const BG = ['bg-chart-1', 'bg-chart-2', 'bg-chart-3', 'bg-chart-4', 'bg-chart-5', 'bg-chart-6'];
  const span = $derived(t.length > 1 ? (t[t.length - 1] - t[0]) * 1000 : 0);
  const fmtX = (v: number) => (span > 2 * 86_400_000 ? new Date(v).toLocaleDateString('de-DE', { day: '2-digit', month: '2-digit' }) : clockTime(v).slice(0, 5));
  const totals = $derived(series.map((s) => s.values.reduce((a, b) => a + (b ?? 0), 0)));
</script>

<figure class="flex flex-col gap-2" aria-label={label} data-testid="stacked-area">
  <div class={['w-full', height]}>
    <Chart data={rows} x="x" y="top" yDomain={[0, null]} yNice padding={{ left: 44, bottom: 22, top: 6, right: 6 }}>
      <Svg>
        <Axis placement="left" format={(v: number) => short(v)} grid rule ticks={4} />
        <Axis placement="bottom" format={(v: number) => fmtX(v)} ticks={6} rule />
        {#each series as s, i (s.key)}
          <Area y0={(d: Row) => d.y0[i]} y1={(d: Row) => d.y1[i]} class="{FILL[i % FILL.length]} opacity-80" line={false} />
        {/each}
      </Svg>
    </Chart>
  </div>
  <figcaption class="flex flex-wrap gap-x-4 gap-y-1 text-xs text-fg-muted">
    {#each series as s, i (s.key)}
      <span class="flex items-center gap-1.5"><span class={['size-2.5 rounded-xs', BG[i % BG.length]]} aria-hidden="true"></span>{s.label} <span class="num text-fg">{short(totals[i])}</span></span>
    {/each}
  </figcaption>
</figure>
