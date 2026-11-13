<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Einmalige Klartext-Anzeige nach Anlegen/Rotieren (API-Key, Admin-API-Key). Schließen erst nach Bestätigung;
  // danach ist der Klartext verworfen (der Server speichert nur Präfix und Hash).
  import Button from '../ui/Button.svelte';
  import CodeBlock from '../ui/CodeBlock.svelte';
  import Dialog from '../ui/Dialog.svelte';
  import SecretReveal from '../ui/SecretReveal.svelte';

  let {
    secret = $bindable(null),
    title,
    description,
    snippet,
    ondone,
  }: { secret?: string | null; title: string; description?: string; snippet?: { title: string; code: string } | null; ondone?: () => void } = $props();
  let confirmed = $state(false);
  let open = $state(false);
  $effect(() => {
    open = !!secret;
    if (secret) confirmed = false;
  });

  function done() {
    secret = null;
    open = false;
    ondone?.();
  }
</script>

{#if secret}
  <Dialog bind:open {title} {description} dismissible={false} size="lg" testid="key-created-dialog">
    <SecretReveal {secret} bind:confirmed />
    {#if snippet}
      <div class="mt-4"><CodeBlock lang="bash" title={snippet.title} code={snippet.code} /></div>
    {/if}
    {#snippet footer()}
      <Button variant="primary" disabled={!confirmed} onclick={done} data-testid="key-done">Fertig</Button>
    {/snippet}
  </Dialog>
{/if}
