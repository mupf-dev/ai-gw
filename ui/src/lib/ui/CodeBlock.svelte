<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // Code-/JSON-Anzeige (immer dunkel, .scheme-dark) mit Sprache und Kopieren. JSON wird formatiert und
  // dezent hervorgehoben (eigener Mini-Tokenizer, keine Abhängigkeit, kein {@html}).
  import CopyButton from './CopyButton.svelte';

  let { code, lang = 'json', title, maxHeight = 'max-h-96' }: { code: unknown; lang?: string; title?: string; maxHeight?: string } = $props();

  const text = $derived(typeof code === 'string' ? code : JSON.stringify(code, null, 2));

  type Tok = { t: string; c: string };
  const tokens = $derived.by<Tok[]>(() => {
    if (lang !== 'json') return [{ t: text, c: '' }];
    const out: Tok[] = [];
    const re = /("(?:\\.|[^"\\])*")(\s*:)?|(-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?)|(true|false|null)/g;
    let last = 0;
    for (const m of text.matchAll(re)) {
      if (m.index > last) out.push({ t: text.slice(last, m.index), c: 'text-code-muted' });
      if (m[1]) {
        out.push({ t: m[1], c: m[2] ? 'text-code-key' : 'text-code-string' });
        if (m[2]) out.push({ t: m[2], c: 'text-code-muted' });
      } else if (m[3]) out.push({ t: m[3], c: 'text-code-number' });
      else out.push({ t: m[4], c: 'text-code-literal' });
      last = m.index + m[0].length;
    }
    if (last < text.length) out.push({ t: text.slice(last), c: 'text-code-muted' });
    return out;
  });
</script>

<div class="scheme-dark overflow-hidden rounded-lg border border-code-line bg-code text-code-fg">
  <div class="flex h-9 items-center gap-2 border-b border-code-line pr-1.5 pl-3">
    <span class="label-caps text-code-muted">{title ?? lang}</span>
    <span class="flex-1"></span>
    <CopyButton text={text} label="Code kopieren" />
  </div>
  <pre class={['scrollbar-thin overflow-auto p-3 text-xs leading-5', maxHeight]}><code>{#each tokens as tok, i (i)}<span class={tok.c}>{tok.t}</span>{/each}</code></pre>
</div>
