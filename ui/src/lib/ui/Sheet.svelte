<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // Seitenpanel (Bits UI Dialog am Rand): Detailansichten (Log-Eintrag), mobile Navigation (side="left").
  import { Dialog } from 'bits-ui';
  import type { Snippet } from 'svelte';
  import Icon from './Icon.svelte';
  import { btn, cx } from './styles';

  let {
    open = $bindable(false),
    title,
    description,
    side = 'right',
    size = 'md',
    header,
    children,
    footer,
    hideTitle = false,
    contentClass = '',
    testid,
  }: {
    contentClass?: string;
    open?: boolean;
    title: string;
    description?: string;
    side?: 'left' | 'right';
    size?: 'sm' | 'md' | 'lg';
    header?: Snippet;
    children: Snippet;
    footer?: Snippet;
    hideTitle?: boolean;
    testid?: string;
  } = $props();
  const width = { sm: 'max-w-xs', md: 'max-w-xl', lg: 'max-w-3xl' };
</script>

<Dialog.Root bind:open>
  <Dialog.Portal>
    <Dialog.Overlay class="anim-fade fixed inset-0 z-40" />
    <Dialog.Content
      data-testid={testid}
      class={cx(
        'fixed inset-y-0 z-50 flex w-full flex-col bg-surface shadow-dialog outline-none',
        side === 'right' ? 'anim-sheet-right right-0 border-l border-line' : 'anim-sheet-left left-0 border-r border-line',
        width[size],
        contentClass,
      )}
    >
      {#if hideTitle}
        <Dialog.Title class="sr-only">{title}</Dialog.Title>
        <Dialog.Close class={btn('ghost', 'icon-sm', 'absolute top-3.5 right-3 z-10')} aria-label="Schließen"><Icon name="x" /></Dialog.Close>
      {:else}
        <header class="flex items-start gap-3 border-b border-line px-5 py-4">
          <div class="min-w-0 flex-1">
            <Dialog.Title class="text-md font-semibold text-fg">{title}</Dialog.Title>
            {#if description}<Dialog.Description class="mt-0.5 text-sm text-fg-muted">{description}</Dialog.Description>{/if}
            {@render header?.()}
          </div>
          <Dialog.Close class={btn('ghost', 'icon-sm', '-mr-1.5')} aria-label="Schließen"><Icon name="x" /></Dialog.Close>
        </header>
      {/if}
      <div class="scrollbar-thin min-h-0 flex-1 overflow-y-auto">{@render children()}</div>
      {#if footer}
        <footer class="flex items-center justify-end gap-2 border-t border-line bg-surface-2 px-5 py-3">{@render footer()}</footer>
      {/if}
    </Dialog.Content>
  </Dialog.Portal>
</Dialog.Root>
