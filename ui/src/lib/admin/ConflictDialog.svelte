<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // Konflikt bei optimistischer Sperre (412 precondition-failed): jemand hat die Ressource zwischenzeitlich geändert.
  import Button from '../ui/Button.svelte';
  import Dialog from '../ui/Dialog.svelte';
  import Icon from '../ui/Icon.svelte';
  import { conflict } from './errors.svelte';

  function reload() {
    conflict.open = false;
    conflict.reload?.();
  }
</script>

<Dialog top bind:open={conflict.open} title="Zwischenzeitlich geändert" size="sm" testid="conflict-dialog">
  <div class="flex gap-3 text-base text-fg-muted">
    <Icon name="alert" size={18} class="mt-0.5 text-warning-fg" />
    <p>
      „{conflict.what}“ wurde nicht ausgeführt: Die Ressource wurde seit dem Laden von jemand anderem geändert. Laden Sie den aktuellen Stand neu
      und wiederholen Sie die Änderung – Ihre Eingaben werden dabei verworfen.
    </p>
  </div>
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (conflict.open = false)}>Abbrechen</Button>
    {#if conflict.reload}
      <Button variant="primary" icon="refresh" onclick={reload} data-testid="conflict-reload">Neu laden</Button>
    {/if}
  {/snippet}
</Dialog>
