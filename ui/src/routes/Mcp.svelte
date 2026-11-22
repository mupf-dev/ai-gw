<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // MCP-Server (AIGW-41/78/77/52): angebundene Upstreams mit Verbindungszustand (GET /mcp-servers:status: up, down,
  // idle, disabled; letzter Fehler, Fehlversuche) und Freigaben (Rechte: welche Teams/Keys welche Tools nutzen).
  // AIGW-52: Registrieren/Bearbeiten/Löschen von Servern (Transport, URL, Anmeldung nur als Verweis auf ein Secret
  // bzw. Anbieter-Zugangsdaten, Einstellungen) und Freigaben (Team/Key, Tool-Muster mit `!`-Ausschluss).
  // Schreiben: Server = globaler Admin (Katalog), Freigaben im Bereich des Teams (Server prüft, 403).
  import { DropdownMenu, Tabs } from 'bits-ui';
  import { listPage } from '../lib/api/client';
  import { credentials, mcpGrants, mcpServers, mcpStatus, NAME_HINT, NAME_RE, secrets, type McpGrant, type McpServer, type S } from '../lib/api/resources';
  import AsyncState from '../lib/admin/AsyncState.svelte';
  import ConfirmDialog from '../lib/admin/ConfirmDialog.svelte';
  import { formErrorOf, handleError, Loader, splitList } from '../lib/admin/errors.svelte';
  import FormDialog from '../lib/admin/FormDialog.svelte';
  import { Paged } from '../lib/admin/paged.svelte';
  import { catalog, orgOfTeam } from '../lib/context.svelte';
  import { canWrite, canWriteSomething } from '../lib/rbac.svelte';
  import { toast } from '../lib/toast.svelte';
  import Badge from '../lib/ui/Badge.svelte';
  import Button from '../lib/ui/Button.svelte';
  import Card from '../lib/ui/Card.svelte';
  import DataTable, { type Column } from '../lib/ui/DataTable.svelte';
  import EmptyState from '../lib/ui/EmptyState.svelte';
  import Field from '../lib/ui/Field.svelte';
  import Icon from '../lib/ui/Icon.svelte';
  import Input from '../lib/ui/Input.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import Select from '../lib/ui/Select.svelte';
  import { btn, cx, menuItem, menuSeparator, popover, tabsList, tabsTrigger, textarea } from '../lib/ui/styles';

  let tab = $state('servers');
  const list = new Paged<McpServer>((q, l, c) => listPage(mcpServers.path, q, l, c));
  const grants = new Paged<McpGrant>((q, l, c) => listPage(mcpGrants.path, q, l, c));
  const status = new Loader(() => mcpStatus());
  const stateOf = (n: string) => status.data?.items.find((s) => s.name === n);
  const TONE: Record<S['McpConnectionState'], 'success' | 'danger' | 'neutral' | 'info'> = { up: 'success', down: 'danger', idle: 'info', disabled: 'neutral' };
  const LABEL: Record<S['McpConnectionState'], string> = { up: 'verbunden', down: 'gestört', idle: 'unbenutzt', disabled: 'deaktiviert' };
  const TRANSPORTS: { value: S['McpTransport']; label: string }[] = [
    { value: 'streamable-http', label: 'Streamable HTTP' },
    { value: 'sse', label: 'SSE (älter)' },
    { value: 'stdio', label: 'stdio (lokales Programm)' },
  ];
  const canServers = () => canWrite({ kind: 'catalog' });

  function reload() {
    void list.reload();
    void status.reload();
    void grants.reload();
  }

  // Verweise (nur Namen – Werte sind nie lesbar).
  let refs = $state<{ secrets: string[]; credentials: string[]; keys: string[] }>({ secrets: [], credentials: [], keys: [] });
  let refsLoaded = false;
  async function loadRefs() {
    if (refsLoaded) return;
    refsLoaded = true;
    const names = async (p: Promise<{ metadata: { name: string } }[]>) => (await p.catch(() => [])).map((x) => x.metadata.name);
    const [s, c, k] = await Promise.all([
      names(secrets.list()),
      names(credentials.list()),
      names(listPage<{ metadata: { name: string } }>('/admin/api/v1/api-keys', {}, 200).then((p) => p.items)),
    ]);
    refs = { secrets: s, credentials: c, keys: k };
  }

  /** „k: v“ bzw. „k=v“ je Zeile ↔ Objekt. */
  const toPairs = (o: Record<string, string> | undefined, sep: string) =>
    Object.entries(o ?? {})
      .map(([k, v]) => `${k}${sep}${v}`)
      .join('\n');
  function fromPairs(s: string, sep: string): Record<string, string> | null {
    const out: Record<string, string> = {};
    for (const l of s.split('\n').map((x) => x.trim()).filter(Boolean)) {
      const i = l.indexOf(sep);
      if (i < 1) return null;
      out[l.slice(0, i).trim()] = l.slice(i + 1).trim();
    }
    return out;
  }

  // ------------------------------------------------------------ Server-Formular
  const blankServer = () => ({
    name: '',
    transport: 'streamable-http' as S['McpTransport'],
    url: '',
    enabled: true,
    auth: '' as '' | 'secret' | 'credential',
    secret: '',
    credential: '',
    headers: '',
    credentialHeader: '',
    credentialField: '',
    timeoutMs: '',
    args: '',
    env: '',
    credentialEnv: '',
  });
  let sf = $state(blankServer());
  let sOpen = $state(false);
  let sEdit = $state<string | null>(null);
  let sEtag = $state<string | null>(null);
  let sLoaded = $state<McpServer | null>(null);
  let sBusy = $state(false);
  let sLoading = $state(false);
  let sError = $state<string | null>(null);
  let sErr = $state<Record<string, string>>({});

  async function openServer(name: string | null) {
    sEdit = name;
    sEtag = null;
    sLoaded = null;
    sError = null;
    sErr = {};
    sf = blankServer();
    sOpen = true;
    void loadRefs();
    if (!name) return;
    sLoading = true;
    try {
      const r = await mcpServers.get(name);
      const s = r.data.spec;
      const st = (s.settings ?? {}) as Record<string, unknown>;
      sEtag = r.etag;
      sLoaded = r.data;
      sf = {
        name,
        transport: s.transport ?? 'streamable-http',
        url: s.url,
        enabled: s.enabled !== false,
        auth: s.secret ? 'secret' : s.credential ? 'credential' : '',
        secret: s.secret ?? '',
        credential: s.credential ?? '',
        headers: toPairs(st.headers as Record<string, string>, ': '),
        credentialHeader: String(st.credentialHeader ?? ''),
        credentialField: String(st.credentialField ?? ''),
        timeoutMs: st.timeoutMs == null ? '' : String(st.timeoutMs),
        args: ((st.args as string[]) ?? []).join('\n'),
        env: toPairs(st.env as Record<string, string>, '='),
        credentialEnv: String(st.credentialEnv ?? ''),
      };
    } catch (e) {
      handleError(e, { action: 'MCP-Server laden' });
      sOpen = false;
    } finally {
      sLoading = false;
    }
  }

  const SERVER_FIELDS: Record<string, string> = { 'metadata.name': 'name', url: 'url', transport: 'transport', secret: 'secret', credential: 'credential' };
  function serverField(field: string, message: string): boolean {
    const k = SERVER_FIELDS[field] ?? (field.startsWith('settings.headers') ? 'headers' : field.startsWith('settings.') ? field.slice(9) : field === 'settings' ? 'settings' : null);
    if (!k) return false;
    sErr = { ...sErr, [k in sf ? k : 'headers']: message };
    return true;
  }

  async function submitServer() {
    const e: Record<string, string> = {};
    if (!sEdit && !NAME_RE.test(sf.name)) e.name = 'Ungültiger Name.';
    if (!sEdit && sf.name.includes('__')) e.name = '„__“ ist der Trenner im Tool-Namensraum.';
    if (!sf.url.trim()) e.url = sf.transport === 'stdio' ? 'Absoluten Pfad des Programms angeben.' : 'URL angeben.';
    else if (sf.transport === 'stdio' && !sf.url.startsWith('/')) e.url = 'Absoluter Pfad (Argumente unten).';
    else if (sf.transport !== 'stdio' && !/^https?:\/\//.test(sf.url.trim())) e.url = 'http(s)-URL angeben.';
    if (sf.auth === 'secret' && !sf.secret) e.secret = 'Secret wählen.';
    if (sf.auth === 'credential' && !sf.credential) e.credential = 'Zugangsdaten wählen.';
    const timeout = sf.timeoutMs.trim() ? Number(sf.timeoutMs) : null;
    if (timeout !== null && (!Number.isInteger(timeout) || timeout < 1)) e.timeoutMs = 'Ganze Zahl ≥ 1.';
    const headers = fromPairs(sf.headers, ':');
    if (!headers) e.headers = 'Je Zeile „Header: Wert“.';
    const env = fromPairs(sf.env, '=');
    if (!env) e.env = 'Je Zeile „NAME=Wert“.';
    sErr = e;
    sError = null;
    if (Object.keys(e).length) return;
    const stdio = sf.transport === 'stdio';
    // Unbekannte Einstellungen bleiben erhalten; bekannte werden aus dem Formular gesetzt.
    const settings: Record<string, unknown> = { ...((sLoaded?.spec.settings ?? {}) as Record<string, unknown>) };
    const put = (k: string, v: unknown) => {
      if (v === undefined || v === null || v === '' || (Array.isArray(v) && !v.length) || (typeof v === 'object' && !Array.isArray(v) && !Object.keys(v as object).length)) delete settings[k];
      else settings[k] = v;
    };
    put('headers', stdio ? undefined : headers);
    put('credentialHeader', stdio ? undefined : sf.credentialHeader.trim());
    put('credentialField', sf.credentialField.trim());
    put('timeoutMs', timeout);
    put('args', stdio ? sf.args.split('\n').map((x) => x.trim()).filter(Boolean) : undefined);
    put('env', stdio ? env : undefined);
    put('credentialEnv', stdio ? sf.credentialEnv.trim() : undefined);
    const name = sEdit ?? sf.name;
    const spec: S['McpServerSpec'] = {
      url: sf.url.trim(),
      transport: sf.transport,
      enabled: sf.enabled,
      secret: sf.auth === 'secret' ? sf.secret : null,
      credential: sf.auth === 'credential' ? sf.credential : null,
      settings: settings as Record<string, never>,
    };
    sBusy = true;
    try {
      if (sEdit) await mcpServers.update(name, { metadata: { name, labels: sLoaded?.metadata.labels ?? {} }, spec }, sEtag);
      else await mcpServers.create({ metadata: { name }, spec });
      sOpen = false;
      toast('success', 'MCP-Server gespeichert', name);
      reload();
    } catch (err) {
      handleError(err, { action: 'MCP-Server speichern', form: (m) => (sError = m), reload: () => sEdit && openServer(sEdit), field: serverField });
    } finally {
      sBusy = false;
    }
  }

  // ------------------------------------------------------------ Freigabe-Formular
  const blankGrant = () => ({ name: '', server: '', level: 'team' as 'team' | 'api-key', target: '', tools: '' });
  let gf = $state(blankGrant());
  let gOpen = $state(false);
  let gEdit = $state<string | null>(null);
  let gEtag = $state<string | null>(null);
  let gLoaded = $state<McpGrant | null>(null);
  let gBusy = $state(false);
  let gLoading = $state(false);
  let gError = $state<string | null>(null);
  let gErr = $state<Record<string, string>>({});
  let serverNames = $state<string[]>([]);

  async function openGrant(name: string | null, server?: string) {
    gEdit = name;
    gEtag = null;
    gLoaded = null;
    gError = null;
    gErr = {};
    gf = { ...blankGrant(), server: server ?? '' };
    gOpen = true;
    void loadRefs();
    mcpServers
      .list()
      .then((l) => (serverNames = l.map((s) => s.metadata.name)))
      .catch(() => {});
    if (!name) return;
    gLoading = true;
    try {
      const r = await mcpGrants.get(name);
      const s = r.data.spec;
      gEtag = r.etag;
      gLoaded = r.data;
      gf = { name, server: s.server, level: s.subject.level === 'api-key' ? 'api-key' : 'team', target: s.subject.target ?? '', tools: (s.tools ?? []).join('\n') };
    } catch (e) {
      handleError(e, { action: 'Freigabe laden' });
      gOpen = false;
    } finally {
      gLoading = false;
    }
  }
  const GRANT_FIELDS: Record<string, string> = { 'metadata.name': 'name', server: 'server', 'subject.target': 'target', 'subject.level': 'target', tools: 'tools' };
  function grantField(field: string, message: string): boolean {
    const k = GRANT_FIELDS[field];
    if (!k) return false;
    gErr = { ...gErr, [k]: message };
    return true;
  }
  async function submitGrant() {
    const e: Record<string, string> = {};
    if (!gEdit && !NAME_RE.test(gf.name)) e.name = 'Ungültiger Name.';
    if (!gf.server) e.server = 'Server wählen.';
    if (!gf.target.trim()) e.target = gf.level === 'team' ? 'Team angeben.' : 'API-Key angeben.';
    const tools = splitList(gf.tools);
    if (tools.some((t) => t.slice(1).includes('!'))) e.tools = '„!“ nur am Anfang eines Musters.';
    gErr = e;
    gError = null;
    if (Object.keys(e).length) return;
    const name = gEdit ?? gf.name;
    const spec: S['McpGrantSpec'] = { server: gf.server, subject: { level: gf.level, target: gf.target.trim() }, tools };
    gBusy = true;
    try {
      if (gEdit) await mcpGrants.update(name, { metadata: { name, labels: gLoaded?.metadata.labels ?? {} }, spec }, gEtag);
      else await mcpGrants.create({ metadata: { name }, spec });
      gOpen = false;
      toast('success', 'Freigabe gespeichert', name);
      void grants.reload();
    } catch (err) {
      handleError(err, { action: 'Freigabe speichern', form: (m) => (gError = m), reload: () => gEdit && openGrant(gEdit), field: grantField });
    } finally {
      gBusy = false;
    }
  }

  // ------------------------------------------------------------ Löschen
  let del = $state<{ kind: 'server' | 'grant'; name: string } | null>(null);
  let delOpen = $state(false);
  async function runDelete(): Promise<string | null> {
    if (!del) return null;
    const res = del.kind === 'server' ? mcpServers : mcpGrants;
    try {
      const { etag } = await res.get(del.name);
      await res.remove(del.name, etag);
      toast('success', del.kind === 'server' ? 'MCP-Server gelöscht' : 'Freigabe gelöscht', del.name);
      reload();
      return null;
    } catch (e) {
      return formErrorOf(e, del.kind === 'server' ? 'MCP-Server löschen' : 'Freigabe löschen');
    }
  }

  const grantWritable = (g: McpGrant) =>
    g.spec.subject.level === 'team' ? canWrite({ kind: 'team', organization: orgOfTeam(g.spec.subject.target) ?? '', team: g.spec.subject.target ?? '' }) : canWriteSomething();

  const columns: Column<McpServer>[] = [
    { id: 'name', label: 'Server', cell: nameCell },
    { id: 'state', label: 'Zustand', cell: stateCell },
    { id: 'transport', label: 'Transport', cell: transportCell, from: 'md' },
    { id: 'auth', label: 'Anmeldung', cell: authCell, from: 'lg' },
    { id: 'actions', label: 'Aktionen', cell: serverActions, srOnlyLabel: true, align: 'right', class: 'w-10' },
  ];
  const grantColumns: Column<McpGrant>[] = [
    { id: 'name', label: 'Freigabe', cell: grantNameCell },
    { id: 'subject', label: 'Für', cell: subjectCell },
    { id: 'tools', label: 'Tools', cell: toolsCell, from: 'md' },
    { id: 'actions', label: 'Aktionen', cell: grantActions, srOnlyLabel: true, align: 'right', class: 'w-10' },
  ];
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
{#snippet menu(label: string, onedit: () => void, ondelete: () => void, extra: { text: string; run: () => void } | null = null)}
  <DropdownMenu.Root>
    <DropdownMenu.Trigger class={btn('ghost', 'icon-sm')} aria-label="Aktionen für {label}"><Icon name="more" /></DropdownMenu.Trigger>
    <DropdownMenu.Portal>
      <DropdownMenu.Content class={cx(popover, 'w-48')} align="end" sideOffset={4}>
        <DropdownMenu.Item class={menuItem} onSelect={onedit}><Icon name="pencil" size={14} /> Bearbeiten</DropdownMenu.Item>
        {#if extra}<DropdownMenu.Item class={menuItem} onSelect={extra.run}><Icon name="key" size={14} /> {extra.text}</DropdownMenu.Item>{/if}
        <DropdownMenu.Separator class={menuSeparator} />
        <DropdownMenu.Item class={cx(menuItem, 'text-danger-fg [&_svg]:text-danger-fg')} onSelect={ondelete}><Icon name="trash" size={14} /> Löschen …</DropdownMenu.Item>
      </DropdownMenu.Content>
    </DropdownMenu.Portal>
  </DropdownMenu.Root>
{/snippet}
{#snippet serverActions(m: McpServer)}
  {#if canServers()}
    {@render menu(
      m.metadata.name,
      () => openServer(m.metadata.name),
      () => {
        del = { kind: 'server', name: m.metadata.name };
        delOpen = true;
      },
      { text: 'Freigabe anlegen', run: () => openGrant(null, m.metadata.name) },
    )}
  {/if}
{/snippet}
{#snippet grantNameCell(g: McpGrant)}
  <span class="block font-mono text-sm font-medium">{g.metadata.name}</span>
  <span class="block font-mono text-xs text-fg-muted">Server {g.spec.server}</span>
{/snippet}
{#snippet subjectCell(g: McpGrant)}<span class="text-sm">{g.spec.subject.level === 'team' ? 'Team' : 'Key'} · <span class="font-mono">{g.spec.subject.target}</span></span>{/snippet}
{#snippet toolsCell(g: McpGrant)}
  <span class="flex flex-wrap gap-1">
    {#each g.spec.tools ?? [] as t (t)}<Badge tone={t.startsWith('!') ? 'danger' : 'outline'} mono>{t}</Badge>{:else}<Badge tone="info">alle Tools</Badge>{/each}
  </span>
{/snippet}
{#snippet grantActions(g: McpGrant)}
  {#if grantWritable(g)}
    {@render menu(
      g.metadata.name,
      () => openGrant(g.metadata.name),
      () => {
        del = { kind: 'grant', name: g.metadata.name };
        delOpen = true;
      },
    )}
  {/if}
{/snippet}

<PageHeader title="MCP-Server" description="Angebundene MCP-Server mit Verbindungszustand und Freigaben. Clients erreichen die freigegebenen Tools über /mcp des Gateways.">
  {#snippet actions()}
    <Button icon="refresh" onclick={reload} loading={list.loading || status.loading}>Aktualisieren</Button>
    {#if tab === 'servers' && canServers()}<Button variant="primary" icon="plus" onclick={() => openServer(null)} data-testid="create-mcp-server">Server registrieren</Button>{/if}
    {#if tab === 'grants' && canWriteSomething()}<Button variant="primary" icon="plus" onclick={() => openGrant(null)} data-testid="create-mcp-grant">Freigabe anlegen</Button>{/if}
  {/snippet}
</PageHeader>

{#if status.data && !status.data.gatewayEnabled}
  <p class="mb-4 rounded-md border border-info-fg/30 bg-info-bg px-3 py-2 text-sm text-info-fg" role="status">Der MCP-Endpunkt des Gateways ist abgeschaltet – Server werden nicht verbunden.</p>
{/if}
<Tabs.Root bind:value={tab}>
  <Tabs.List class={cx(tabsList, 'mb-4')}>
    <Tabs.Trigger value="servers" class={tabsTrigger} data-testid="tab-servers">Server</Tabs.Trigger>
    <Tabs.Trigger value="grants" class={tabsTrigger} data-testid="tab-grants">Freigaben</Tabs.Trigger>
  </Tabs.List>
  <Tabs.Content value="servers">
    <Card flush>
      <AsyncState loader={list}>
        {#snippet children(rows)}
          <DataTable {rows} {columns} server={list} key={(m) => m.metadata.name} caption="MCP-Server" search={() => ''} searchPlaceholder="Name …" testid="mcp-table">
            {#snippet empty()}<EmptyState icon="network" title="Noch keine MCP-Server" text="„Server registrieren“ bindet einen Upstream-MCP-Server an (oder per GitOps)." />{/snippet}
          </DataTable>
        {/snippet}
      </AsyncState>
    </Card>
  </Tabs.Content>
  <Tabs.Content value="grants">
    <Card flush>
      <AsyncState loader={grants}>
        {#snippet children(rows)}
          <DataTable {rows} columns={grantColumns} server={grants} key={(g) => g.metadata.name} caption="MCP-Freigaben" search={() => ''} searchPlaceholder="Name …" testid="mcp-grants-table">
            {#snippet empty()}<EmptyState icon="key" title="Noch keine Freigaben" text="Ohne Freigabe sieht kein Team und kein Key Tools eines MCP-Servers." />{/snippet}
          </DataTable>
        {/snippet}
      </AsyncState>
    </Card>
  </Tabs.Content>
</Tabs.Root>

<FormDialog bind:open={sOpen} title={sEdit ? `MCP-Server „${sEdit}“ bearbeiten` : 'MCP-Server registrieren'} size="lg" busy={sBusy} loading={sLoading} error={sError} submitLabel={sEdit ? 'Speichern' : 'Registrieren'} onsubmit={submitServer} testid="mcp-server-form">
  {#if !sEdit}
    <Field label="Name" required hint="{NAME_HINT} Ohne „__“ (Tool-Präfix name__tool)." error={sErr.name}>
      {#snippet children({ id, describedby, invalid })}<Input {id} bind:value={sf.name} aria-describedby={describedby} aria-invalid={invalid || undefined} autocomplete="off" placeholder="github" />{/snippet}
    </Field>
  {/if}
  <Field label="Transport" error={sErr.transport}>
    {#snippet children({ id })}<Select {id} bind:value={sf.transport} items={TRANSPORTS} />{/snippet}
  </Field>
  <Field label={sf.transport === 'stdio' ? 'Programm (absoluter Pfad)' : 'URL'} required error={sErr.url} hint={sf.transport === 'stdio' ? 'Argumente unten' : 'SSRF-Regeln wie Deployments (https, keine privaten Ziele)'} class="sm:col-span-2">
    {#snippet children({ id, describedby, invalid })}<Input {id} bind:value={sf.url} aria-describedby={describedby} aria-invalid={invalid || undefined} autocomplete="off" placeholder={sf.transport === 'stdio' ? '/usr/local/bin/mcp-fs' : 'https://mcp.example.org/mcp'} />{/snippet}
  </Field>
  <Field label="Aktiv">
    {#snippet children({ id })}<Select {id} value={sf.enabled ? 'true' : 'false'} onchange={(v) => (sf.enabled = v === 'true')} items={[{ value: 'true', label: 'an' }, { value: 'false', label: 'aus' }]} />{/snippet}
  </Field>
  <Field label="Anmeldung" hint="nur als Verweis – Werte bleiben verschlüsselt (F149)">
    {#snippet children({ id })}<Select {id} bind:value={sf.auth} items={[{ value: '', label: 'ohne' }, { value: 'secret', label: 'Secret (empfohlen)' }, { value: 'credential', label: 'Anbieter-Zugangsdaten' }]} />{/snippet}
  </Field>
  {#if sf.auth === 'secret'}
    <Field label="Secret" required error={sErr.secret} hint={refs.secrets.length ? undefined : 'Noch keine Secrets – unter „Secrets“ anlegen.'}>
      {#snippet children({ id, describedby, invalid })}<Select {id} bind:value={sf.secret} items={refs.secrets.map((s) => ({ value: s, label: s }))} {describedby} {invalid} />{/snippet}
    </Field>
  {:else if sf.auth === 'credential'}
    <Field label="Zugangsdaten" required error={sErr.credential}>
      {#snippet children({ id, describedby, invalid })}<Select {id} bind:value={sf.credential} items={refs.credentials.map((s) => ({ value: s, label: s }))} {describedby} {invalid} />{/snippet}
    </Field>
  {/if}
  {#if sf.auth}
    <Field label="Feld mit dem Token" optional hint="Standard apiKey" error={sErr.credentialField}>
      {#snippet children({ id, describedby })}<Input {id} bind:value={sf.credentialField} aria-describedby={describedby} placeholder="apiKey" autocomplete="off" />{/snippet}
    </Field>
    {#if sf.transport !== 'stdio'}
      <Field label="Header für das Token" optional hint="Standard authorization (Bearer …)" error={sErr.credentialHeader}>
        {#snippet children({ id, describedby })}<Input {id} bind:value={sf.credentialHeader} aria-describedby={describedby} placeholder="authorization" autocomplete="off" />{/snippet}
      </Field>
    {:else}
      <Field label="Umgebungsvariable für das Token" optional error={sErr.credentialEnv}>
        {#snippet children({ id, describedby })}<Input {id} bind:value={sf.credentialEnv} aria-describedby={describedby} placeholder="GITHUB_TOKEN" autocomplete="off" />{/snippet}
      </Field>
    {/if}
  {/if}
  <Field label="Zeitlimit je Anfrage (ms)" optional error={sErr.timeoutMs} hint="Standard mcp.call_timeout_secs">
    {#snippet children({ id, describedby, invalid })}<Input {id} bind:value={sf.timeoutMs} inputmode="numeric" aria-describedby={describedby} aria-invalid={invalid || undefined} placeholder="Standard" />{/snippet}
  </Field>
  {#if sf.transport === 'stdio'}
    <Field label="Argumente" optional hint="eins je Zeile" error={sErr.args} class="sm:col-span-2">
      {#snippet children({ id, describedby })}<textarea {id} class={[textarea, 'min-h-16 font-mono']} bind:value={sf.args} aria-describedby={describedby} spellcheck="false"></textarea>{/snippet}
    </Field>
    <Field label="Umgebung" optional hint="NAME=Wert je Zeile – nicht geheim; Geheimes über die Anmeldung" error={sErr.env} class="sm:col-span-2">
      {#snippet children({ id, describedby, invalid })}<textarea {id} class={[textarea, 'min-h-16 font-mono']} bind:value={sf.env} aria-describedby={describedby} aria-invalid={invalid || undefined} spellcheck="false"></textarea>{/snippet}
    </Field>
  {:else}
    <Field label="Weitere Header" optional hint="„Header: Wert“ je Zeile – nicht geheim; Token über die Anmeldung" error={sErr.headers} class="sm:col-span-2">
      {#snippet children({ id, describedby, invalid })}<textarea {id} class={[textarea, 'min-h-16 font-mono']} bind:value={sf.headers} aria-describedby={describedby} aria-invalid={invalid || undefined} spellcheck="false" data-testid="mcp-headers"></textarea>{/snippet}
    </Field>
  {/if}
</FormDialog>

<FormDialog bind:open={gOpen} title={gEdit ? `Freigabe „${gEdit}“ bearbeiten` : 'MCP-Freigabe anlegen'} busy={gBusy} loading={gLoading} error={gError} submitLabel={gEdit ? 'Speichern' : 'Anlegen'} onsubmit={submitGrant} testid="mcp-grant-form">
  {#if !gEdit}
    <Field label="Name" required hint={NAME_HINT} error={gErr.name} class="sm:col-span-2">
      {#snippet children({ id, describedby, invalid })}<Input {id} bind:value={gf.name} aria-describedby={describedby} aria-invalid={invalid || undefined} autocomplete="off" placeholder="research-github" />{/snippet}
    </Field>
  {/if}
  <Field label="Server" required error={gErr.server}>
    {#snippet children({ id, describedby, invalid })}<Select {id} bind:value={gf.server} items={serverNames.map((s) => ({ value: s, label: s }))} {describedby} {invalid} />{/snippet}
  </Field>
  <Field label="Für">
    {#snippet children({ id })}<Select {id} bind:value={gf.level} items={[{ value: 'team', label: 'Team' }, { value: 'api-key', label: 'API-Key (innerhalb der Team-Freigabe)' }]} />{/snippet}
  </Field>
  <Field label={gf.level === 'team' ? 'Team' : 'API-Key'} required error={gErr.target} class="sm:col-span-2">
    {#snippet children({ id, describedby, invalid })}<Input {id} bind:value={gf.target} list="grant-targets" aria-describedby={describedby} aria-invalid={invalid || undefined} autocomplete="off" />{/snippet}
  </Field>
  <datalist id="grant-targets">{#each gf.level === 'team' ? catalog.teams.map((t) => t.metadata.name) : refs.keys as t (t)}<option value={t}></option>{/each}</datalist>
  <Field label="Tools" optional hint="Glob-Muster je Zeile (* und ?), „!muster“ schließt aus; leer = alle Tools" error={gErr.tools} class="sm:col-span-2">
    {#snippet children({ id, describedby, invalid })}<textarea {id} class={[textarea, 'min-h-20 font-mono']} bind:value={gf.tools} aria-describedby={describedby} aria-invalid={invalid || undefined} spellcheck="false" placeholder={'search_*\n!delete_*'}></textarea>{/snippet}
  </Field>
</FormDialog>

<ConfirmDialog bind:open={delOpen} title={del?.kind === 'server' ? 'MCP-Server löschen?' : 'Freigabe löschen?'} confirmLabel="Löschen" onconfirm={runDelete}>
  <p>
    „<span class="font-mono text-fg">{del?.name}</span>“ wird gelöscht{del?.kind === 'server' ? '; Clients sehen seine Tools nach dem Hot Reload nicht mehr.' : '; die Tools sind für diesen Empfänger nicht mehr sichtbar.'}
  </p>
</ConfirmDialog>
