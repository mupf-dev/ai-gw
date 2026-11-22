<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // Zustandshinweise unter der Topbar (AIGW-77):
  // - Snapshot veraltet bzw. Abgleich mit der Datenbank schlägt fehl (F15, SnapshotInfo aus /version)
  // - Bootstrap-Token aktiv (F56, auth/info `bootstrap`) – Notfallzugang abschalten
  // - Nur-YAML-Modus (F56, auth/info `readOnly`) – Schreibaktionen sind ausgeblendet
  import { isMock } from '../mode.svelte';
  import { href } from '../router.svelte';
  import { auth } from '../session.svelte';
  import Icon from '../ui/Icon.svelte';
  import { gateway, loadVersion } from '../version.svelte';

  loadVersion();

  const snap = $derived(gateway.info?.snapshot ?? null);
  const age = (s: number) => (s < 120 ? `${s} s` : s < 7200 ? `${Math.round(s / 60)} min` : `${Math.round(s / 3600)} Std.`);
</script>

{#if snap && (snap.stale || snap.failing)}
  <div class="flex flex-wrap items-center gap-x-3 gap-y-1 border-b border-danger-fg/30 bg-danger-bg px-4 py-2 text-sm text-danger-fg sm:px-6 lg:px-8" role="alert" data-testid="snapshot-banner">
    <Icon name="alert" size={14} />
    <span class="min-w-0 flex-1">
      <strong class="font-semibold">Konfiguration {snap.stale ? 'veraltet' : 'nicht abgeglichen'}:</strong>
      {#if snap.source === 'cache'}
        Das Gateway läuft mit dem zwischengespeicherten Snapshot (Version <span class="num">{snap.version}</span>, Datenbank nicht erreichbar).
      {:else}
        Letzter erfolgreicher Abgleich mit der Datenbank vor <span class="num">{age(snap.ageSeconds)}</span> (Version <span class="num">{snap.version}</span>).
      {/if}
      Änderungen wirken erst nach dem nächsten Abgleich; Readiness meldet „degraded“.
    </span>
  </div>
{/if}
{#if auth.bootstrap && !isMock()}
  <div class="flex flex-wrap items-center gap-x-3 gap-y-1 border-b border-warning-fg/30 bg-warning-bg px-4 py-2 text-sm text-warning-fg sm:px-6 lg:px-8" role="status" data-testid="bootstrap-banner">
    <Icon name="lock" size={14} />
    <span class="min-w-0 flex-1">
      <strong class="font-semibold">Bootstrap-Token aktiv:</strong> Der Notfallzugang gilt, bis sich ein globaler Admin per OIDC anmeldet oder ein Admin-API-Key mit Rolle „Admin“ angelegt wird.
    </span>
    {#if auth.session}<a class="font-medium underline underline-offset-2 hover:no-underline" href={href('/admin-keys')}>Admin-API-Keys</a>{/if}
  </div>
{/if}
{#if auth.readOnly && !isMock()}
  <div class="flex flex-wrap items-center gap-x-3 gap-y-1 border-b border-info-fg/30 bg-info-bg px-4 py-2 text-sm text-info-fg sm:px-6 lg:px-8" role="status" data-testid="readonly-banner">
    <Icon name="file-text" size={14} />
    <span class="min-w-0 flex-1"><strong class="font-semibold">Nur-YAML-Modus:</strong> Änderungen nur über GitOps – Schreibaktionen sind ausgeblendet.</span>
    <a class="font-medium underline underline-offset-2 hover:no-underline" href={href('/settings')}>GitOps</a>
  </div>
{/if}
