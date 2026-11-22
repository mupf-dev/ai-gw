<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // Rotation (Admin-API §2.1): neuer Key mit gleicher Spezifikation, der alte läuft nach der Übergangszeit ab.
  // Liest den Key vorher (ETag) und rotiert mit If-Match.
  import { untrack } from 'svelte';
  import { apiKeys, NAME_RE, type S } from '../../lib/api/resources';
  import { handleError } from '../../lib/admin/errors.svelte';
  import FormError from '../../lib/admin/FormError.svelte';
  import Button from '../../lib/ui/Button.svelte';
  import Dialog from '../../lib/ui/Dialog.svelte';
  import Field from '../../lib/ui/Field.svelte';
  import Input from '../../lib/ui/Input.svelte';
  import Select from '../../lib/ui/Select.svelte';

  let {
    open = $bindable(false),
    name,
    onrotated,
  }: { open?: boolean; name: string | null; onrotated: (r: S['RotatedApiKey']) => void } = $props();

  let newName = $state('');
  let grace = $state('86400');
  let etag = $state<string | null>(null);
  let error = $state<string | null>(null);
  let busy = $state(false);

  $effect(() => {
    if (!open || !name) return;
    const n = name;
    untrack(() => {
      newName = '';
      grace = '86400';
      error = null;
      etag = null;
      apiKeys
        .get(n)
        .then((r) => (etag = r.etag))
        .catch((e) => handleError(e, { action: 'Key laden' }));
    });
  });

  async function submit(e: SubmitEvent) {
    e.preventDefault();
    if (!name) return;
    if (newName && !NAME_RE.test(newName)) {
      error = 'Ungültiger Name: a-z, 0-9, „-“, „.“, „_“.';
      return;
    }
    busy = true;
    try {
      const r = await apiKeys.rotate(name, { name: newName.trim() || null, gracePeriodSeconds: Number(grace) }, etag);
      open = false;
      onrotated(r.data);
    } catch (err) {
      handleError(err, { action: 'Key rotieren', form: (m) => (error = m), reload: () => (open = false) });
    } finally {
      busy = false;
    }
  }
</script>

<Dialog bind:open title="Key „{name}“ rotieren" description="Neuer Key mit gleicher Konfiguration; der alte bleibt bis zum Ende der Übergangszeit gültig." testid="rotate-dialog">
  <form id="rotate-form" class="flex flex-col gap-4" onsubmit={submit} novalidate>
    <Field label="Name des neuen Keys" optional hint="Leer = alter Name mit Zeitstempel (-JJJJMMTThhmmss).">
      {#snippet children({ id, describedby })}
        <Input {id} bind:value={newName} aria-describedby={describedby} autocomplete="off" placeholder="{name}-2" />
      {/snippet}
    </Field>
    <Field label="Übergangszeit für den alten Key">
      {#snippet children({ id })}
        <Select
          {id}
          bind:value={grace}
          items={[
            { value: '0', label: 'sofort ungültig', hint: 'laufende Clients schlagen fehl' },
            { value: '3600', label: '1 Stunde' },
            { value: '86400', label: '24 Stunden', hint: 'Standard' },
            { value: '604800', label: '7 Tage' },
          ]}
        />
      {/snippet}
    </Field>
    <FormError message={error} />
  </form>
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (open = false)}>Abbrechen</Button>
    <Button variant="primary" type="submit" form="rotate-form" icon="refresh" loading={busy} disabled={busy} data-testid="rotate-submit">Rotieren</Button>
  {/snippet}
</Dialog>
