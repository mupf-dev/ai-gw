<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  import { Button, Separator } from 'bits-ui';
  import { nav } from './lib/nav';
  import Overview from './routes/Overview.svelte';

  let current = $state(nav[0].id);
</script>

<div class="flex min-h-screen">
  <aside class="flex w-56 shrink-0 flex-col border-r border-zinc-200 bg-zinc-50 dark:border-zinc-800 dark:bg-zinc-900">
    <div class="px-4 py-4 text-lg font-semibold">ai-gw</div>
    <Separator.Root class="h-px bg-zinc-200 dark:bg-zinc-800" />
    <nav class="flex flex-col gap-1 p-2" aria-label="Hauptnavigation">
      {#each nav as item (item.id)}
        <Button.Root
          class="rounded-md px-3 py-2 text-left text-sm hover:bg-zinc-200 aria-[current=page]:bg-zinc-200 aria-[current=page]:font-medium dark:hover:bg-zinc-800 dark:aria-[current=page]:bg-zinc-800"
          aria-current={current === item.id ? 'page' : undefined}
          onclick={() => (current = item.id)}
        >
          {item.label}
        </Button.Root>
      {/each}
    </nav>
  </aside>
  <main class="flex-1 p-6">
    {#if current === 'overview'}
      <Overview />
    {/if}
  </main>
</div>
