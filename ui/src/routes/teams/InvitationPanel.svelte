<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Einladung eines vorab angelegten Nutzers (F90, /users/{name}/invitation): Stand anzeigen, Link erzeugen
  // (einmalig sichtbar, ersetzt eine frühere Einladung), widerrufen. Nur mit OIDC und für Nutzer ohne Identität.
  import { ApiError } from '../../lib/api/client';
  import { invitations, type Invitation } from '../../lib/api/observe';
  import { handleError, problemText } from '../../lib/admin/errors.svelte';
  import { dateTime } from '../../lib/format';
  import { canAdminWrite } from '../../lib/rbac.svelte';
  import { auth } from '../../lib/session.svelte';
  import { toast } from '../../lib/toast.svelte';
  import Badge from '../../lib/ui/Badge.svelte';
  import Button from '../../lib/ui/Button.svelte';
  import CopyButton from '../../lib/ui/CopyButton.svelte';

  let { user, linked }: { user: string; linked: boolean } = $props();

  let inv = $state<Invitation | null>(null);
  let url = $state<string | null>(null);
  let loading = $state(true);
  let busy = $state(false);
  let error = $state<string | null>(null);

  async function load() {
    loading = true;
    error = null;
    try {
      inv = await invitations.get(user);
    } catch (e) {
      inv = null;
      if (!(e instanceof ApiError && e.status === 404)) error = problemText(e);
    } finally {
      loading = false;
    }
  }
  if (!linked) void load();
  else loading = false;

  async function create() {
    busy = true;
    try {
      const r = await invitations.create(user);
      inv = r.invitation;
      url = r.url;
    } catch (e) {
      handleError(e, { action: 'Einladung erzeugen', form: (m) => (error = m) });
    } finally {
      busy = false;
    }
  }
  async function revoke() {
    busy = true;
    try {
      await invitations.revoke(user);
      inv = null;
      url = null;
      toast('success', 'Einladung widerrufen', user);
    } catch (e) {
      handleError(e, { action: 'Einladung widerrufen', form: (m) => (error = m) });
    } finally {
      busy = false;
    }
  }
</script>

<section class="flex flex-col gap-2 rounded-lg border border-line p-3" data-testid="invitation">
  <h3 class="label-caps text-fg-subtle">Einladung</h3>
  {#if linked}
    <p class="text-sm text-fg-muted">Der Nutzer ist bereits mit einer OIDC-Identität verknüpft – keine Einladung nötig.</p>
  {:else if !auth.oidc}
    <p class="text-sm text-fg-muted">Einladungen brauchen einen OIDC-Login (admin.auth.oidc).</p>
  {:else if loading}
    <div class="h-6 animate-pulse rounded-md bg-skeleton"></div>
  {:else}
    {#if inv}
      <p class="flex flex-wrap items-center gap-2 text-sm">
        {#if inv.expired}<Badge tone="neutral" dot>abgelaufen</Badge>{:else}<Badge tone="info" dot>offen</Badge>{/if}
        <span class="num text-fg-muted">gültig bis {dateTime(Date.parse(inv.expiresAt))} · von {inv.createdBy}</span>
      </p>
    {:else}
      <p class="text-sm text-fg-muted">Keine offene Einladung.</p>
    {/if}
    {#if url}
      <div class="flex flex-col gap-1 rounded-md border border-warning-fg/30 bg-warning-bg p-2 text-sm text-warning-fg" role="status">
        <span>Link nur jetzt sichtbar – an den Nutzer weitergeben (einmalig einlösbar):</span>
        <span class="flex items-center gap-1"><code class="min-w-0 flex-1 truncate font-mono text-xs text-fg" data-testid="invitation-url">{url}</code><CopyButton text={url} label="Einladungslink kopieren" /></span>
      </div>
    {/if}
    {#if error}<p class="text-sm text-danger-fg" role="alert">{error}</p>{/if}
    {#if canAdminWrite()}
      <div class="flex flex-wrap gap-2">
        <Button size="sm" icon="external" onclick={create} loading={busy} data-testid="invitation-create">{inv ? 'Neuen Link erzeugen' : 'Einladungslink erzeugen'}</Button>
        {#if inv}<Button size="sm" variant="ghost" icon="x" onclick={revoke} disabled={busy} data-testid="invitation-revoke">Widerrufen</Button>{/if}
      </div>
    {/if}
  {/if}
</section>
