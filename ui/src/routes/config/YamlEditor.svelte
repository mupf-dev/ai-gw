<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // YAML-Eingabe mit Zeilennummern (AIGW-55): Zeilen mit Validierungsfehlern sind in der Leiste markiert,
  // `gotoLine` springt zur Fundstelle (Auswahl der Zeile). Kein CodeMirror – ein Textfeld genügt, Scroll-Position
  // der Leiste per CSSOM (CSP: keine style-Attribute).
  import { textarea } from '../../lib/ui/styles';

  let {
    value = $bindable(''),
    errorLines = [],
    id = 'gitops-yaml',
    placeholder = '',
    disabled = false,
  }: { value?: string; errorLines?: number[]; id?: string; placeholder?: string; disabled?: boolean } = $props();

  let area = $state<HTMLTextAreaElement>();
  let gutter = $state<HTMLDivElement>();
  const count = $derived(Math.max(1, value.split('\n').length));
  const marked = $derived(new Set(errorLines));
  /** Zeilenhöhe in px (leading-5). */
  const LINE = 20;

  function sync() {
    if (area && gutter) gutter.scrollTop = area.scrollTop;
  }

  /** Markiert Zeile `n` (ab 1) und scrollt dorthin. */
  export function gotoLine(n: number) {
    if (!area) return;
    const lines = value.split('\n');
    const i = Math.min(Math.max(n, 1), lines.length) - 1;
    const start = lines.slice(0, i).reduce((s, l) => s + l.length + 1, 0);
    area.focus();
    area.setSelectionRange(start, start + lines[i].length);
    area.scrollTop = Math.max(0, i * LINE - area.clientHeight / 3);
    sync();
  }
</script>

<div class="flex min-h-64 overflow-hidden rounded-md border border-line focus-within:border-accent" data-testid="yaml-editor">
  <div bind:this={gutter} class="w-12 shrink-0 overflow-hidden border-r border-line bg-surface-2 py-2 text-right font-mono text-sm leading-5 text-fg-subtle select-none" aria-hidden="true">
    {#each { length: count } as _, i (i)}
      <div class={['pr-2', marked.has(i + 1) && 'bg-danger-bg font-semibold text-danger-fg']} data-line={i + 1} data-error={marked.has(i + 1) || undefined}>{i + 1}</div>
    {/each}
  </div>
  <textarea
    bind:this={area}
    {id}
    bind:value
    onscroll={sync}
    {placeholder}
    {disabled}
    spellcheck="false"
    wrap="off"
    class={[textarea, 'h-96 min-h-64 resize-y rounded-none border-0 leading-5 whitespace-pre focus-visible:outline-0']}
    data-testid="gitops-yaml"
  ></textarea>
</div>
