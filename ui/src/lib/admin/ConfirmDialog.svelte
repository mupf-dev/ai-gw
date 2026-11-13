<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Bestätigung destruktiver Aktionen mit Klartext-Folgen; Aktion läuft asynchron (Ladezustand, Fehler im Dialog).
  import type { Snippet } from 'svelte';
  import Button from '../ui/Button.svelte';
  import Dialog from '../ui/Dialog.svelte';
  import type { IconName } from '../ui/Icon.svelte';
  import FormError from './FormError.svelte';
  import { input } from '../ui/styles';

  let {
    open = $bindable(false),
    title,
    confirmLabel,
    icon = 'trash',
    danger = true,
    onconfirm,
    children,
    testid = 'confirm-dialog',
    requireText,
  }: {
    /** Deutliche Bestätigung: Aktion erst nach Eingabe genau dieses Texts (z. B. Name beim endgültigen Widerruf). */
    requireText?: string;
    open?: boolean;
    title: string;
    confirmLabel: string;
    icon?: IconName;
    danger?: boolean;
    /** Liefert eine Fehlermeldung (bleibt offen) oder null (schließt). */
    onconfirm: () => Promise<string | null | void>;
    children: Snippet;
    testid?: string;
  } = $props();
  let busy = $state(false);
  let error = $state<string | null>(null);

  let typed = $state('');
  $effect(() => {
    if (open) {
      error = null;
      typed = '';
    }
  });

  async function confirm() {
    busy = true;
    try {
      const msg = await onconfirm();
      if (msg) error = msg;
      else open = false;
    } finally {
      busy = false;
    }
  }
</script>

<Dialog bind:open {title} size="sm" {testid}>
  <div class="flex flex-col gap-3 text-base text-fg-muted">
    {@render children()}
    {#if requireText}
      <label class="flex flex-col gap-1.5 text-sm">
        <span>Zur Bestätigung „<span class="font-mono text-fg">{requireText}</span>“ eingeben:</span>
        <input class={input} bind:value={typed} autocomplete="off" spellcheck="false" data-testid="confirm-text" />
      </label>
    {/if}
    <FormError message={error} />
  </div>
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (open = false)}>Abbrechen</Button>
    <Button variant={danger ? 'danger' : 'primary'} {icon} loading={busy} disabled={busy || (!!requireText && typed.trim() !== requireText)} onclick={confirm} data-testid="confirm-action">{confirmLabel}</Button>
  {/snippet}
</Dialog>
