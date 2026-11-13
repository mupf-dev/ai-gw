<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Detailansicht einer Ressource im Seitenpanel: Kopf, frei belegbarer Inhalt, Metadaten und Spezifikation (JSON).
  import type { Snippet } from 'svelte';
  import type { ObjectMeta } from '../api/client';
  import { dateTime } from '../format';
  import Badge from '../ui/Badge.svelte';
  import CodeBlock from '../ui/CodeBlock.svelte';
  import Sheet from '../ui/Sheet.svelte';

  let {
    open = $bindable(false),
    title,
    description,
    object,
    children,
    footer,
  }: {
    open?: boolean;
    title: string;
    description?: string;
    object: { id?: string; metadata?: ObjectMeta; spec?: unknown } | null;
    children?: Snippet;
    footer?: Snippet;
  } = $props();
</script>

<Sheet bind:open {title} {description} size="md" testid="detail-sheet" {footer}>
  {#if object}
    <div class="flex flex-col gap-5 px-5 py-4">
      {@render children?.()}
      <dl class="grid grid-cols-[auto_1fr] gap-x-6 gap-y-2 text-base">
        {#if object.id}<dt class="text-fg-muted">ID</dt><dd class="font-mono text-sm break-all">{object.id}</dd>{/if}
        {#if object.metadata?.createdAt}<dt class="text-fg-muted">Angelegt</dt><dd class="num">{dateTime(Date.parse(object.metadata.createdAt))}</dd>{/if}
        {#if object.metadata?.updatedAt}<dt class="text-fg-muted">Geändert</dt><dd class="num">{dateTime(Date.parse(object.metadata.updatedAt))}</dd>{/if}
        <dt class="text-fg-muted">Labels</dt>
        <dd class="flex flex-wrap gap-1">
          {#each Object.entries(object.metadata?.labels ?? {}) as [k, v] (k)}<Badge mono tone="outline">{k}={v}</Badge>{:else}<span class="text-fg-muted">–</span>{/each}
        </dd>
      </dl>
      {#if object.spec}<CodeBlock title="spec" code={object.spec} />{/if}
    </div>
  {/if}
</Sheet>
