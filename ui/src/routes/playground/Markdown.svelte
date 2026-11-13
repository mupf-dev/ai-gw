<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Sichere Markdown-Darstellung (AIGW-54): Baum aus markdown.ts als normale Elemente, Text immer als Text
  // (kein {@html}). Links nur http/https/mailto, in neuem Tab mit noopener/noreferrer.
  import { parse, type Block, type Inline } from './markdown';

  let { text }: { text: string } = $props();
  const blocks = $derived(parse(text));
</script>

{#snippet inlines(items: Inline[])}
  {#each items as it, i (i)}
    {#if it.t === 'text'}{it.v}{:else if it.t === 'code'}<code class="rounded bg-surface-2 px-1 py-0.5 font-mono text-[0.9em]">{it.v}</code>
    {:else if it.t === 'strong'}<strong class="font-semibold">{@render inlines(it.c)}</strong>
    {:else if it.t === 'em'}<em>{@render inlines(it.c)}</em>
    {:else if it.t === 'del'}<del>{@render inlines(it.c)}</del>
    {:else if it.t === 'link'}<a href={it.href} target="_blank" rel="noopener noreferrer nofollow" class="text-accent-text underline underline-offset-2">{@render inlines(it.c)}</a>
    {:else if it.t === 'br'}<br />{/if}
  {/each}
{/snippet}

{#snippet render(list: Block[])}
  {#each list as b, i (i)}
    {#if b.t === 'p'}<p>{@render inlines(b.c)}</p>
    {:else if b.t === 'h'}
      <p class={['font-semibold', b.level <= 2 ? 'text-lg' : 'text-base']} role="heading" aria-level={Math.min(b.level + 2, 6)}>{@render inlines(b.c)}</p>
    {:else if b.t === 'code'}
      <pre class="scheme-dark overflow-x-auto rounded-md bg-code p-3 font-mono text-sm text-code-fg"><code data-lang={b.lang || undefined}>{b.v}</code></pre>
    {:else if b.t === 'ul'}
      <ul class="list-disc pl-5">{#each b.items as item, j (j)}<li>{@render render(item)}</li>{/each}</ul>
    {:else if b.t === 'ol'}
      <ol class="list-decimal pl-5" start={b.start}>{#each b.items as item, j (j)}<li>{@render render(item)}</li>{/each}</ol>
    {:else if b.t === 'quote'}
      <blockquote class="border-l-2 border-line-strong pl-3 text-fg-muted">{@render render(b.c)}</blockquote>
    {:else if b.t === 'hr'}<hr class="border-line" />
    {:else if b.t === 'table'}
      <div class="overflow-x-auto">
        <table class="text-sm">
          <thead><tr>{#each b.head as c, j (j)}<th class="border-b border-line px-2 py-1 text-left font-semibold">{@render inlines(c)}</th>{/each}</tr></thead>
          <tbody>
            {#each b.rows as row, r (r)}<tr>{#each row as c, j (j)}<td class="border-b border-line px-2 py-1">{@render inlines(c)}</td>{/each}</tr>{/each}
          </tbody>
        </table>
      </div>
    {/if}
  {/each}
{/snippet}

<div class="md flex min-w-0 flex-col gap-2 break-words text-base leading-relaxed" data-testid="markdown">{@render render(blocks)}</div>
