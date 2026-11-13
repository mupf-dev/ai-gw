<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Modaler Dialog (Bits UI Dialog: Fokusfalle, Esc, aria-modal). Kopf, Inhalt (scrollt), Fußzeile mit Aktionen.
  import { Dialog } from 'bits-ui';
  import type { Snippet } from 'svelte';
  import Icon from './Icon.svelte';
  import { btn, cx } from './styles';

  let {
    open = $bindable(false),
    title,
    description,
    size = 'md',
    dismissible = true,
    children,
    footer,
    testid,
    top = false,
  }: {
    open?: boolean;
    title: string;
    description?: string;
    size?: 'sm' | 'md' | 'lg';
    /** false: Schließen nur über die Aktionen (z. B. Key-Anzeige bis zur Bestätigung). */
    dismissible?: boolean;
    children: Snippet;
    footer?: Snippet;
    testid?: string;
    /** Über anderen offenen Dialogen (Konflikt, Anmeldung). */
    top?: boolean;
  } = $props();
  const width = { sm: 'max-w-sm', md: 'max-w-lg', lg: 'max-w-2xl' };
</script>

<Dialog.Root bind:open>
  <Dialog.Portal>
    <Dialog.Overlay class={cx('anim-fade fixed inset-0', top ? 'z-[60]' : 'z-40')} />
    <Dialog.Content
      data-testid={testid}
      interactOutsideBehavior={dismissible ? 'close' : 'ignore'}
      escapeKeydownBehavior={dismissible ? 'close' : 'ignore'}
      class={cx(
        top ? 'z-[61]' : 'z-50',
        'anim-dialog fixed top-1/2 left-1/2 flex max-h-[calc(100dvh-2rem)] w-[calc(100vw-2rem)] -translate-x-1/2 -translate-y-1/2 flex-col rounded-xl border border-line bg-surface shadow-dialog outline-none',
        width[size],
      )}
    >
      <header class="flex items-start gap-3 border-b border-line px-5 py-4">
        <div class="min-w-0 flex-1">
          <Dialog.Title class="text-md font-semibold text-fg">{title}</Dialog.Title>
          {#if description}<Dialog.Description class="mt-0.5 text-sm text-fg-muted">{description}</Dialog.Description>{/if}
        </div>
        {#if dismissible}
          <Dialog.Close class={btn('ghost', 'icon-sm', '-mr-1.5')} aria-label="Schließen"><Icon name="x" /></Dialog.Close>
        {/if}
      </header>
      <div class="scrollbar-thin min-h-0 flex-1 overflow-y-auto px-5 py-4">{@render children()}</div>
      {#if footer}
        <footer class="flex flex-wrap items-center justify-end gap-2 rounded-b-xl border-t border-line bg-surface-2 px-5 py-3">
          {@render footer()}
        </footer>
      {/if}
    </Dialog.Content>
  </Dialog.Portal>
</Dialog.Root>
