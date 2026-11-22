<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // Audit-Verlauf (GET /audit-log, AIGW-66): neueste zuerst, serverseitig paginiert; je Eintrag aufklappbar mit
  // den geänderten Feldern (Vorher → Nachher). Auf Detailseiten mit festem Objektfilter, auf /audit mit Filterleiste.
  import { untrack } from 'svelte';
  import { auditLog, type AuditEntry, type AuditQuery } from '../api/observe';
  import { dateTime, ago } from '../format';
  import Badge from '../ui/Badge.svelte';
  import CodeBlock from '../ui/CodeBlock.svelte';
  import EmptyState from '../ui/EmptyState.svelte';
  import Icon from '../ui/Icon.svelte';
  import { btn } from '../ui/styles';
  import AsyncState from './AsyncState.svelte';
  import { Paged } from './paged.svelte';

  let { query, showObject = true, testid = 'audit-trail', pageSize = 25 }: { query: AuditQuery; showObject?: boolean; testid?: string; pageSize?: number } = $props();

  const paged = new Paged<AuditEntry>((_q, limit, cursor) => auditLog(query, limit, cursor), () => ({}), { pageSize });
  let first = true;
  $effect(() => {
    void JSON.stringify(query);
    if (first) {
      first = false;
      return;
    }
    untrack(() => void paged.first());
  });

  let openId = $state<string | null>(null);

  const ACTION_TONE: Record<string, 'success' | 'warning' | 'danger' | 'info' | 'neutral' | 'accent'> = {
    create: 'success',
    update: 'info',
    delete: 'danger',
    revoke: 'danger',
    suspend: 'warning',
    resume: 'success',
    rotate: 'accent',
    'login-failed': 'danger',
    'cascade-remove': 'warning',
    force: 'warning',
    clamp: 'warning',
  };
  const ACTION_LABEL: Record<string, string> = {
    create: 'angelegt',
    update: 'geändert',
    delete: 'gelöscht',
    rotate: 'rotiert',
    suspend: 'gesperrt',
    resume: 'entsperrt',
    revoke: 'widerrufen',
    clamp: 'begrenzt',
    force: 'erzwungen',
    'cascade-remove': 'Verweise entfernt',
    login: 'Anmeldung',
    'login-failed': 'Anmeldung fehlgeschlagen',
    logout: 'Abmeldung',
    'first-use': 'erste Nutzung',
    disable: 'abgeschaltet',
  };

  type Change = { path: string; before: unknown; after: unknown };
  const IGNORE = /^(metadata\.(updatedAt|createdAt)|status\.lastUsedAt)$/;
  function flatten(v: unknown, prefix = '', out: Record<string, unknown> = {}): Record<string, unknown> {
    if (v && typeof v === 'object' && !Array.isArray(v)) {
      const entries = Object.entries(v as Record<string, unknown>);
      if (entries.length === 0 && prefix) out[prefix] = {};
      for (const [k, x] of entries) flatten(x, prefix ? `${prefix}.${k}` : k, out);
    } else if (prefix) out[prefix] = v;
    return out;
  }
  function changes(e: AuditEntry): Change[] {
    const a = flatten(e.before ?? {});
    const b = flatten(e.after ?? {});
    const keys = [...new Set([...Object.keys(a), ...Object.keys(b)])].filter((k) => !IGNORE.test(k)).sort();
    return keys.filter((k) => JSON.stringify(a[k]) !== JSON.stringify(b[k])).map((k) => ({ path: k, before: a[k], after: b[k] }));
  }
  const show = (v: unknown) => (v === undefined ? '–' : typeof v === 'string' ? v : JSON.stringify(v));
</script>

<div data-testid={testid}>
  <AsyncState loader={paged}>
    {#snippet children(items)}
      {#if items.length === 0}
        <EmptyState compact icon="clipboard" title="Keine Einträge" text="Für diese Auswahl gibt es (noch) keine Einträge im Audit-Log." />
      {:else}
        <ol class="divide-y divide-line" aria-label="Audit-Einträge">
          {#each items as e (e.id)}
            {@const open = openId === e.id}
            <li data-testid="audit-entry">
              <button type="button" class="flex w-full flex-wrap items-center gap-x-3 gap-y-1 px-4 py-2.5 text-left hover:bg-surface-hover" aria-expanded={open} onclick={() => (openId = open ? null : e.id)}>
                <Icon name={open ? 'chevron-down' : 'chevron-right'} size={14} class="text-fg-subtle" />
                <Badge tone={ACTION_TONE[e.action] ?? 'neutral'}>{ACTION_LABEL[e.action] ?? e.action}</Badge>
                {#if showObject}
                  <span class="min-w-0 truncate text-base"><span class="text-fg-muted">{e.kind}</span> <span class="font-mono text-sm font-medium text-fg">{e.objectName ?? e.objectId ?? ''}</span></span>
                {/if}
                <span class="min-w-0 flex-1 truncate font-mono text-sm text-fg-muted">{e.actor}</span>
                <span class="num text-xs whitespace-nowrap text-fg-subtle" title={dateTime(Date.parse(e.at))}>{ago(Date.parse(e.at))}</span>
              </button>
              {#if open}
                {@const list = changes(e)}
                <div class="flex flex-col gap-3 px-4 pb-4 pl-11">
                  <dl class="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
                    <dt class="text-fg-muted">Zeitpunkt</dt><dd class="num">{dateTime(Date.parse(e.at))}</dd>
                    <dt class="text-fg-muted">Konfigurationsversion</dt><dd class="num">{e.configVersion}</dd>
                    {#if e.organization || e.team}<dt class="text-fg-muted">Bereich</dt><dd class="font-mono">{[e.organization, e.team].filter(Boolean).join(' / ')}{#if (e.organizationBefore && e.organizationBefore !== e.organization) || (e.teamBefore && e.teamBefore !== e.team)} (vorher {[e.organizationBefore, e.teamBefore].filter(Boolean).join(' / ')}){/if}</dd>{/if}
                  </dl>
                  {#if list.length}
                    <div class="scrollbar-thin overflow-x-auto rounded-md border border-line">
                      <table class="w-full text-left text-sm" data-testid="audit-diff">
                        <thead><tr class="border-b border-line bg-surface-2 text-fg-muted"><th class="px-3 py-1.5 font-medium">Feld</th><th class="px-3 py-1.5 font-medium">Vorher</th><th class="px-3 py-1.5 font-medium">Nachher</th></tr></thead>
                        <tbody>
                          {#each list as c (c.path)}
                            <tr class="border-b border-line last:border-0 align-top">
                              <td class="px-3 py-1.5 font-mono text-xs text-fg-muted">{c.path}</td>
                              <td class="px-3 py-1.5 font-mono text-xs break-all text-danger-fg">{show(c.before)}</td>
                              <td class="px-3 py-1.5 font-mono text-xs break-all text-success-fg">{show(c.after)}</td>
                            </tr>
                          {/each}
                        </tbody>
                      </table>
                    </div>
                  {:else if e.after && !e.before}
                    <CodeBlock title="Einzelheiten" code={e.after} maxHeight="max-h-48" />
                  {:else}
                    <p class="text-sm text-fg-muted">Keine inhaltlichen Änderungen.</p>
                  {/if}
                </div>
              {/if}
            </li>
          {/each}
        </ol>
      {/if}
      {#if items.length > 0 || paged.index > 0}
        <div class="flex items-center gap-2 border-t border-line px-3 py-2 text-sm text-fg-muted">
          <span class="num">Seite {paged.index + 1}</span>
          <div class="ml-auto flex gap-1">
            <button type="button" class={btn('ghost', 'sm')} disabled={!paged.hasPrev || paged.loading} onclick={() => paged.prevPage()}><Icon name="chevron-left" size={14} /> Neuer</button>
            <button type="button" class={btn('ghost', 'sm')} disabled={!paged.hasNext || paged.loading} onclick={() => paged.nextPage()} data-testid="audit-next">Älter <Icon name="chevron-right" size={14} /></button>
          </div>
        </div>
      {/if}
    {/snippet}
  </AsyncState>
</div>
