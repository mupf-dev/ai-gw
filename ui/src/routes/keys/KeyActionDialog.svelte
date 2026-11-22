<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // Bestätigung der Zustandsaktionen eines Keys (F37): Sperren (umkehrbar), Entsperren, endgültig Widerrufen (mit
  // Eingabe des Namens) und Löschen. Liste und Detailseite nutzen denselben Dialog.
  import { apiKeys } from '../../lib/api/resources';
  import ConfirmDialog from '../../lib/admin/ConfirmDialog.svelte';
  import { formErrorOf } from '../../lib/admin/errors.svelte';
  import { toast } from '../../lib/toast.svelte';
  import type { KeyAction } from './keyState';

  let {
    open = $bindable(false),
    action,
    name,
    ondone,
  }: { open?: boolean; action: KeyAction | null; name: string | null; ondone: (a: KeyAction) => void } = $props();

  const TEXT: Record<KeyAction, { title: string; label: string; done: string; icon: 'ban' | 'check-circle' | 'x-circle' | 'trash'; danger: boolean }> = {
    suspend: { title: 'Key sperren?', label: 'Sperren', done: 'Key gesperrt', icon: 'ban', danger: true },
    resume: { title: 'Key entsperren?', label: 'Entsperren', done: 'Key entsperrt', icon: 'check-circle', danger: false },
    revoke: { title: 'Key endgültig widerrufen?', label: 'Endgültig widerrufen', done: 'Key widerrufen', icon: 'x-circle', danger: true },
    delete: { title: 'Key löschen?', label: 'Löschen', done: 'Key gelöscht', icon: 'trash', danger: true },
  };
  const t = $derived(action ? TEXT[action] : TEXT.suspend);

  async function run(): Promise<string | null> {
    if (!action || !name) return null;
    try {
      const { etag } = await apiKeys.get(name);
      if (action === 'suspend') await apiKeys.suspend(name, etag);
      else if (action === 'resume') await apiKeys.resume(name, etag);
      else if (action === 'revoke') await apiKeys.revokePermanently(name, etag);
      else await apiKeys.remove(name, etag);
      toast('success', t.done, `„${name}“`);
      ondone(action);
      return null;
    } catch (e) {
      return formErrorOf(e, t.label);
    }
  }
</script>

<ConfirmDialog bind:open title={t.title} confirmLabel={t.label} icon={t.icon} danger={t.danger} onconfirm={run} requireText={action === 'revoke' ? (name ?? undefined) : undefined} testid="key-action-dialog">
  {#if action === 'suspend'}
    <p>„<span class="font-medium text-fg">{name}</span>“ wird nach dem Hot Reload sofort abgewiesen (HTTP 401). <strong class="text-fg">Entsperren ist jederzeit möglich.</strong></p>
  {:else if action === 'resume'}
    <p>„<span class="font-medium text-fg">{name}</span>“ wird wieder angenommen (sofern nicht abgelaufen).</p>
  {:else if action === 'revoke'}
    <p>„<span class="font-medium text-fg">{name}</span>“ wird <strong class="text-danger-fg">endgültig</strong> widerrufen: Er lässt sich weder entsperren noch ändern oder rotieren. Für eine vorübergehende Sperre „Sperren“ wählen.</p>
  {:else if action === 'delete'}
    <p>„<span class="font-medium text-fg">{name}</span>“ wird gelöscht und sofort abgewiesen. Das Audit-Log behält die Einträge. Das lässt sich nicht rückgängig machen.</p>
  {/if}
</ConfirmDialog>
