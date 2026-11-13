<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // MCP-Server (AIGW-41/78/77): angebundene Upstreams mit Verbindungszustand (GET /mcp-servers:status: up, down,
  // idle, disabled; letzter Fehler, Fehlversuche) und Konfiguration (Transport, URL, Zugangsdaten/Secret).
  // Nur lesen; Pflege per GitOps bzw. Admin-API.
  import { listPage } from '../lib/api/client';
  import { mcpServers, mcpStatus, type McpServer, type S } from '../lib/api/resources';
  import AsyncState from '../lib/admin/AsyncState.svelte';
  import { Loader } from '../lib/admin/errors.svelte';
  import { Paged } from '../lib/admin/paged.svelte';
  import Badge from '../lib/ui/Badge.svelte';
  import Button from '../lib/ui/Button.svelte';
  import Card from '../lib/ui/Card.svelte';
  import DataTable, { type Column } from '../lib/ui/DataTable.svelte';
  import EmptyState from '../lib/ui/EmptyState.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';

  const list = new Paged<McpServer>((q, l, c) => listPage(mcpServers.path, q, l, c));
  const status = new Loader(() => mcpStatus());
  const stateOf = (n: string) => status.data?.items.find((s) => s.name === n);
  const TONE: Record<S['McpConnectionState'], 'success' | 'danger' | 'neutral' | 'info'> = { up: 'success', down: 'danger', idle: 'info', disabled: 'neutral' };
  const LABEL: Record<S['McpConnectionState'], string> = { up: 'verbunden', down: 'gestört', idle: 'unbenutzt', disabled: 'deaktiviert' };

  const columns: Column<McpServer>[] = [
    { id: 'name', label: 'Server', cell: nameCell },
    { id: 'state', label: 'Zustand', cell: stateCell },
    { id: 'transport', label: 'Transport', cell: transportCell, from: 'md' },
    { id: 'auth', label: 'Anmeldung', cell: authCell, from: 'lg' },
  ];
  function reload() {
    void list.reload();
    void status.reload();
  }
</script>

{#snippet nameCell(m: McpServer)}
  <span class="block font-mono text-sm font-medium">{m.metadata.name}</span>
  <span class="block truncate font-mono text-xs text-fg-muted">{m.spec.url}</span>
{/snippet}
{#snippet stateCell(m: McpServer)}
  {@const st = stateOf(m.metadata.name)}
  {#if st}
    <span class="flex flex-col gap-0.5">
      <span class="flex items-center gap-1.5"><Badge tone={TONE[st.state]} dot>{LABEL[st.state]}</Badge>{#if st.failures}<span class="num text-xs text-fg-muted">{st.failures} Fehlversuche</span>{/if}</span>
      {#if st.error}<span class="max-w-80 truncate text-xs text-danger-fg" title={st.error}>{st.error}</span>{/if}
    </span>
  {:else}<span class="text-sm text-fg-subtle">–</span>{/if}
{/snippet}
{#snippet transportCell(m: McpServer)}<Badge mono>{m.spec.transport ?? 'streamable-http'}</Badge>{/snippet}
{#snippet authCell(m: McpServer)}<span class="font-mono text-xs text-fg-muted">{m.spec.secret ? `Secret ${m.spec.secret}` : m.spec.credential ? `Zugangsdaten ${m.spec.credential}` : 'ohne'}</span>{/snippet}

<PageHeader title="MCP-Server" description="Angebundene MCP-Server mit Verbindungszustand. Clients erreichen die freigegebenen Tools über /mcp des Gateways.">
  {#snippet actions()}<Button icon="refresh" onclick={reload} loading={list.loading || status.loading}>Aktualisieren</Button>{/snippet}
</PageHeader>

{#if status.data && !status.data.gatewayEnabled}
  <p class="mb-4 rounded-md border border-info-fg/30 bg-info-bg px-3 py-2 text-sm text-info-fg" role="status">Der MCP-Endpunkt des Gateways ist abgeschaltet – Server werden nicht verbunden.</p>
{/if}
<Card flush>
  <AsyncState loader={list}>
    {#snippet children(rows)}
      <DataTable {rows} {columns} server={list} key={(m) => m.metadata.name} caption="MCP-Server" search={() => ''} searchPlaceholder="Name …" testid="mcp-table">
        {#snippet empty()}<EmptyState icon="network" title="Noch keine MCP-Server" text="MCP-Server werden per GitOps bzw. Admin-API (/mcp-servers) angelegt." />{/snippet}
      </DataTable>
    {/snippet}
  </AsyncState>
</Card>
