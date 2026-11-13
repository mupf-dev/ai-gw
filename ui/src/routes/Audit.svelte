<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Audit (AIGW-66/77): Änderungsprotokoll der Steuerungsebene (GET /audit-log) mit Filtern (Art, Aktion,
  // Akteursart, Akteur, Objekt, Zeitraum), Vorher/Nachher je Eintrag und Export als JSON Lines (SIEM).
  // Filter stehen in der URL, damit Links (z. B. aus Detailseiten) die Auswahl mitbringen.
  import { api, unwrap } from '../lib/api/client';
  import { auditExport, type AuditQuery } from '../lib/api/observe';
  import type { S } from '../lib/api/resources';
  import { roles } from '../lib/rbac.svelte';
  import AuditTrail from '../lib/admin/AuditTrail.svelte';
  import { download, fromLocalInput, handleError, toLocalInput } from '../lib/admin/errors.svelte';
  import { navigate, route } from '../lib/router.svelte';
  import { toast } from '../lib/toast.svelte';
  import Button from '../lib/ui/Button.svelte';
  import Card from '../lib/ui/Card.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import Select from '../lib/ui/Select.svelte';
  import { input } from '../lib/ui/styles';

  const p = new URLSearchParams(route.search);
  let f = $state({
    kind: p.get('kind') ?? '',
    action: p.get('action') ?? '',
    actorType: p.get('actorType') ?? '',
    actor: p.get('actor') ?? '',
    object: p.get('object') ?? '',
    from: toLocalInput(p.get('from')),
    to: toLocalInput(p.get('to')),
  });
  // Textfelder erst beim Verlassen/Enter übernehmen (keine Abfrage je Tastendruck).
  let applied = $state({ actor: f.actor, object: f.object });

  const query = $derived<AuditQuery>({
    kind: f.kind || undefined,
    action: f.action || undefined,
    actorType: f.actorType || undefined,
    actor: applied.actor.trim() || undefined,
    object: applied.object.trim() || undefined,
    from: fromLocalInput(f.from) ?? undefined,
    to: fromLocalInput(f.to) ?? undefined,
  });
  $effect(() => {
    const qs = new URLSearchParams(Object.entries(query).filter(([, v]) => v) as [string, string][]).toString();
    navigate(`/audit${qs ? `?${qs}` : ''}`, true);
  });

  // Arten in kebab-case (F115).
  const KINDS = ['organization', 'team', 'user', 'api-key', 'provider-credential', 'secret', 'model-group', 'deployment', 'price', 'plugin-binding', 'mcp-server', 'mcp-grant', 'admin-api-key', 'admin-session', 'admin-invitation', 'bootstrap-token', 'audit-log'];
  const ACTIONS = ['create', 'update', 'delete', 'rotate', 'suspend', 'resume', 'revoke', 'clamp', 'force', 'cascade-remove', 'login', 'login-failed', 'logout', 'first-use', 'disable'];
  const ACTOR_TYPES = [
    { value: 'user', label: 'Nutzer' },
    { value: 'admin-key', label: 'Admin-API-Key' },
    { value: 'bootstrap', label: 'Bootstrap-Token' },
    { value: 'gitops', label: 'GitOps' },
    { value: 'oidc-login', label: 'OIDC-Login' },
    { value: 'system', label: 'System' },
  ];

  // Hash-Kette prüfen (F116, GET /audit-log/verify) – nur globale admin/auditor.
  const canVerify = $derived(roles().some((b) => (b.role === 'admin' || b.role === 'auditor') && !b.organization && !b.team));
  let verifying = $state(false);
  let report = $state<S['AuditChainReport'] | null>(null);
  async function verify() {
    verifying = true;
    try {
      report = await unwrap<S['AuditChainReport']>(api.GET('/admin/api/v1/audit-log/verify'));
    } catch (e) {
      handleError(e, { action: 'Audit-Kette prüfen' });
    } finally {
      verifying = false;
    }
  }

  let exporting = $state(false);
  async function exportNdjson() {
    exporting = true;
    try {
      const text = await auditExport(query);
      const n = text.split('\n').filter(Boolean).length;
      download(`aigw-audit-${new Date().toISOString().slice(0, 19).replace(/:/g, '')}.ndjson`, text, 'application/x-ndjson');
      toast('success', 'Export erstellt', `${n} Einträge als JSON Lines`);
    } catch (e) {
      handleError(e, { action: 'Audit-Export' });
    } finally {
      exporting = false;
    }
  }
  function reset() {
    f = { kind: '', action: '', actorType: '', actor: '', object: '', from: '', to: '' };
    applied = { actor: '', object: '' };
  }
  const applyText = () => (applied = { actor: f.actor, object: f.object });
</script>

<PageHeader title="Audit" description="Wer hat wann was geändert – Konfiguration, Keys, Anmeldungen. Einträge sind unveränderlich; Geheimnisse sind redigiert.">
  {#snippet actions()}
    {#if canVerify}<Button icon="shield" onclick={verify} loading={verifying} data-testid="audit-verify">Kette prüfen</Button>{/if}
    <Button icon="download" onclick={exportNdjson} loading={exporting} data-testid="audit-export">Export (JSON Lines)</Button>
  {/snippet}
</PageHeader>

{#if report}
  <div class={['mb-4 rounded-md border px-3 py-2 text-sm', report.ok ? 'border-success-fg/30 bg-success-bg text-success-fg' : 'border-danger-fg/30 bg-danger-bg text-danger-fg']} role="status" data-testid="audit-verify-result">
    <strong class="font-semibold">{report.ok ? 'Audit-Kette unversehrt' : 'Audit-Kette beschädigt'}</strong>
    – <span class="num">{report.checked}</span> Einträge geprüft (bis Nr. <span class="num">{report.headSeq}</span>{report.anchorSeq ? `, ab Anker ${report.anchorSeq}` : ''}{report.unchained ? `, ${report.unchained} ältere ohne Kette` : ''}).
    <span class="block font-mono text-xs break-all opacity-80">Kopf-Hash {report.headHash}</span>
    {#if report.issues.length}
      <ul class="mt-1 list-disc pl-5">{#each report.issues.slice(0, 10) as i (i.seq)}<li><span class="num">Nr. {i.seq}</span>: {i.problem}</li>{/each}</ul>
    {/if}
  </div>
{/if}
<Card flush>
  <div class="flex flex-wrap items-end gap-2 border-b border-line px-3 py-2.5" data-testid="audit-filters">
    <div class="w-44"><Select size="sm" label="Art" bind:value={f.kind} items={[{ value: '', label: 'Alle Arten' }, ...KINDS.map((k) => ({ value: k, label: k }))]} /></div>
    <div class="w-40"><Select size="sm" label="Aktion" bind:value={f.action} items={[{ value: '', label: 'Alle Aktionen' }, ...ACTIONS.map((a) => ({ value: a, label: a }))]} /></div>
    <div class="w-40"><Select size="sm" label="Akteursart" bind:value={f.actorType} items={[{ value: '', label: 'Alle Akteure' }, ...ACTOR_TYPES]} /></div>
    <input class={[input, 'h-8 w-40 text-sm']} placeholder="Akteur (user:anna)" aria-label="Akteur" bind:value={f.actor} onchange={applyText} onkeydown={(e) => e.key === 'Enter' && applyText()} />
    <input class={[input, 'h-8 w-40 text-sm']} placeholder="Objekt (Name/ID)" aria-label="Objekt" bind:value={f.object} onchange={applyText} onkeydown={(e) => e.key === 'Enter' && applyText()} data-testid="audit-object" />
    <label class="flex items-center gap-1 text-xs text-fg-muted">ab <input class={[input, 'h-8 w-48 text-sm']} type="datetime-local" bind:value={f.from} /></label>
    <label class="flex items-center gap-1 text-xs text-fg-muted">bis <input class={[input, 'h-8 w-48 text-sm']} type="datetime-local" bind:value={f.to} /></label>
    <Button size="sm" variant="ghost" onclick={reset}>Zurücksetzen</Button>
  </div>
  <AuditTrail {query} testid="audit-log" pageSize={50} />
</Card>
