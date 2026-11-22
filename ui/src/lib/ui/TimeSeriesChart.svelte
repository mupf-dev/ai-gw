<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts" module>
  export interface ChartSeries {
    label: string;
    values: number[];
    /** Token-Name, z. B. '--color-chart-1' (feste kategoriale Reihenfolge, nie zyklisch). */
    color: string;
  }
</script>

<script lang="ts">
  // Zeitreihen-Diagramm auf uPlot (Canvas, ~20 KiB gzip, MIT). Farben kommen aus den Design-Tokens und werden
  // über ein Probe-Element aufgelöst (light-dark() → rgb), bei Theme-/Akzentwechsel wird neu gezeichnet.
  // Eine y-Achse; Fadenkreuz + Tooltip beim Überfahren; Zusammenfassung für Screenreader.
  // CSP: uPlot setzt Positionen per CSSOM (element.style.*) – erlaubt unter style-src 'self'.
  import uPlot from 'uplot';
  import 'uplot/dist/uPlot.min.css';
  import { env, prefs } from '../prefs.svelte';

  let {
    t,
    series,
    format,
    label,
    height = 260,
    area = true,
  }: {
    /** Zeitstempel in Sekunden */
    t: number[];
    series: ChartSeries[];
    format: (v: number) => string;
    label: string;
    height?: number;
    area?: boolean;
  } = $props();

  let host: HTMLDivElement;
  let probe: HTMLSpanElement;
  let tip: HTMLDivElement;

  function resolve(token: string): string {
    probe.style.color = `var(${token})`;
    return getComputedStyle(probe).color;
  }
  function alpha(rgb: string, a: number): string {
    const m = rgb.match(/[\d.]+/g);
    return m ? `rgba(${m[0]}, ${m[1]}, ${m[2]}, ${a})` : rgb;
  }

  const spanSec = $derived(t.length > 1 ? t[t.length - 1] - t[0] : 0);
  const fmtTick = (s: number) => {
    const d = new Date(s * 1000);
    return spanSec <= 2 * 86400
      ? d.toLocaleTimeString('de-DE', { hour: '2-digit', minute: '2-digit' })
      : d.toLocaleDateString('de-DE', { day: '2-digit', month: '2-digit' });
  };
  const fmtFull = (s: number) =>
    new Date(s * 1000).toLocaleString('de-DE', { weekday: 'short', day: '2-digit', month: '2-digit', hour: '2-digit', minute: '2-digit' });

  const summary = $derived.by(() => {
    const v = series[0]?.values ?? [];
    if (!v.length) return '';
    return `${label}: Minimum ${format(Math.min(...v))}, Maximum ${format(Math.max(...v))}, zuletzt ${format(v[v.length - 1])}.`;
  });

  function tooltipPlugin(): uPlot.Plugin {
    return {
      hooks: {
        setCursor: (u) => {
          const i = u.cursor.idx;
          if (!tip || !host) return; // Komponente bereits entfernt
          if (i == null || u.cursor.left == null || u.cursor.left < 0) {
            tip.hidden = true;
            return;
          }
          tip.hidden = false;
          // Inhalt ohne innerHTML aufbauen (Trusted-Types-freundlich, keine Injektion)
          tip.replaceChildren();
          const head = document.createElement('div');
          head.className = 'mb-1 text-xs text-fg-muted';
          head.textContent = fmtFull(t[i]);
          tip.append(head);
          series.forEach((s, k) => {
            const row = document.createElement('div');
            row.className = 'flex items-center gap-2 text-sm';
            const dot = document.createElement('span');
            dot.className = 'inline-block size-2 shrink-0 rounded-full';
            dot.style.background = colors[k];
            const name = document.createElement('span');
            name.className = 'text-fg-muted';
            name.textContent = s.label;
            const val = document.createElement('span');
            val.className = 'num ml-auto pl-4 font-medium text-fg';
            val.textContent = format(s.values[i]);
            row.append(dot, name, val);
            tip.append(row);
          });
          const left = u.cursor.left + u.over.offsetLeft;
          const top = u.over.offsetTop + 8;
          const w = tip.offsetWidth;
          const x = left + 12 + w > host.clientWidth ? left - 12 - w : left + 12;
          tip.style.transform = `translate(${Math.round(x)}px, ${top}px)`;
        },
      },
    };
  }

  let colors: string[] = [];
  let chart: uPlot | undefined;

  function build() {
    chart?.destroy();
    colors = series.map((s) => resolve(s.color));
    const axis = resolve('--color-chart-axis');
    const grid = resolve('--color-chart-grid');
    const surface = resolve('--color-surface');
    const font = `11px ${getComputedStyle(host).fontFamily}`;
    const opts: uPlot.Options = {
      width: host.clientWidth,
      height,
      padding: [12, 24, 0, 4],
      legend: { show: false },
      cursor: {
        y: false,
        drag: { x: false, y: false },
        points: { size: 8, width: 2, stroke: (u, si) => colors[si - 1], fill: () => surface },
      },
      scales: { x: { time: true }, y: { range: (_u, _min, max) => [0, max > 0 ? max * 1.12 : 1] } },
      axes: [
        { stroke: axis, font, grid: { show: false }, ticks: { show: false }, gap: 8, values: (_u, splits) => splits.map(fmtTick) },
        {
          stroke: axis,
          font,
          size: 64,
          gap: 8,
          ticks: { show: false },
          grid: { stroke: grid, width: 1 },
          values: (_u, splits) => splits.map((v) => format(v)),
        },
      ],
      series: [
        {},
        ...series.map((s, k) => ({
          label: s.label,
          stroke: colors[k],
          width: 2,
          points: { show: false },
          fill:
            area && series.length === 1
              ? (u: uPlot) => {
                  const g = u.ctx.createLinearGradient(0, u.bbox.top, 0, u.bbox.top + u.bbox.height);
                  g.addColorStop(0, alpha(colors[k], 0.22));
                  g.addColorStop(1, alpha(colors[k], 0));
                  return g;
                }
              : undefined,
        })),
      ],
      plugins: [tooltipPlugin()],
    };
    chart = new uPlot(opts, [t, ...series.map((s) => s.values)], host);
  }

  $effect(() => {
    // Abhängigkeiten: Daten, Schema, Akzent
    void [t, series, prefs.theme, env.systemDark];
    build();
  });

  $effect(() => {
    const ro = new ResizeObserver(() => chart?.setSize({ width: host.clientWidth, height }));
    ro.observe(host);
    return () => {
      ro.disconnect();
      chart?.destroy();
      chart = undefined;
    };
  });
</script>

<figure class="relative m-0" aria-label={label}>
  <span bind:this={probe} class="hidden" aria-hidden="true"></span>
  <div bind:this={host} class="gw-chart w-full" role="img" aria-label={summary}></div>
  <div
    bind:this={tip}
    hidden
    class="pointer-events-none absolute top-0 left-0 z-10 min-w-40 rounded-md border border-line bg-surface px-2.5 py-2 shadow-pop"
    aria-hidden="true"
  ></div>
  {#if series.length > 1}
    <figcaption class="mt-2 flex flex-wrap gap-x-4 gap-y-1 text-sm text-fg-muted">
      {#each series as s (s.label)}
        <span class="inline-flex items-center gap-1.5"><span class="legend-dot size-2 rounded-full" data-token={s.color}></span>{s.label}</span>
      {/each}
    </figcaption>
  {/if}
</figure>

<style>
  /* uPlot-Grundstil an Tokens anpassen */
  .gw-chart :global(.u-over),
  .gw-chart :global(.u-under) {
    cursor: crosshair;
  }
  .gw-chart :global(.u-cursor-x) {
    border-right: 1px dashed var(--color-line-strong);
  }
  .legend-dot[data-token='--color-chart-1'] {
    background: var(--color-chart-1);
  }
  .legend-dot[data-token='--color-chart-2'] {
    background: var(--color-chart-2);
  }
  .legend-dot[data-token='--color-chart-3'] {
    background: var(--color-chart-3);
  }
  .legend-dot[data-token='--color-chart-4'] {
    background: var(--color-chart-4);
  }
</style>
