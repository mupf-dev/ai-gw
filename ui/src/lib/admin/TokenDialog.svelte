<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Anmeldung ohne OIDC: Bootstrap-Token (Server-Log beim Start bzw. AIGW_BOOTSTRAP_TOKEN) oder Admin-API-Key.
  // Gespeichert nur im sessionStorage dieses Tabs; geprüft über /admin/api/v1/auth/info.
  import { setMockMode } from '../prefs.svelte';
  import { auth, submitToken } from '../session.svelte';
  import Button from '../ui/Button.svelte';
  import Dialog from '../ui/Dialog.svelte';
  import Field from '../ui/Field.svelte';
  import Input from '../ui/Input.svelte';
  import FormError from './FormError.svelte';

  let token = $state('');
  let busy = $state(false);

  $effect(() => {
    if (auth.tokenDialog) token = '';
  });

  async function submit(e: SubmitEvent) {
    e.preventDefault();
    if (!token.trim()) {
      auth.tokenError = 'Bitte ein Token eingeben.';
      return;
    }
    busy = true;
    try {
      await submitToken(token);
    } finally {
      busy = false;
    }
  }
  function useMock() {
    setMockMode('on');
    auth.tokenDialog = false;
  }
</script>

<Dialog top bind:open={auth.tokenDialog} title="Anmelden" description="Dieses Gateway hat kein OIDC konfiguriert." size="md" testid="token-dialog">
  <form id="token-form" class="flex flex-col gap-4" onsubmit={submit} novalidate>
    <p class="text-base text-fg-muted">
      Geben Sie das <strong class="text-fg">Bootstrap-Token</strong> (Server-Log beim Start bzw. <code class="font-mono text-sm">AIGW_BOOTSTRAP_TOKEN</code>)
      oder einen <strong class="text-fg">Admin-API-Key</strong> (<code class="font-mono text-sm">sk-aigw-admin-…</code>) ein. Das Token bleibt nur in diesem
      Browser-Tab gespeichert.
    </p>
    <Field label="Token" required>
      {#snippet children({ id, describedby, invalid })}
        <Input {id} type="password" bind:value={token} autocomplete="off" spellcheck={false} aria-describedby={describedby} aria-invalid={invalid || undefined} data-testid="token-input" />
      {/snippet}
    </Field>
    <FormError message={auth.tokenError} />
  </form>
  {#snippet footer()}
    <Button variant="ghost" class="mr-auto" onclick={useMock}>Mock-Daten ansehen</Button>
    <Button variant="secondary" onclick={() => (auth.tokenDialog = false)}>Abbrechen</Button>
    <Button variant="primary" type="submit" form="token-form" icon="lock" loading={busy} disabled={busy} data-testid="token-submit">Anmelden</Button>
  {/snippet}
</Dialog>
