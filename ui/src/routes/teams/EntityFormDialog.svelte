<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts" module>
  export type Kind = 'org' | 'team' | 'user';
</script>

<script lang="ts">
  // Organisation, Team oder Nutzer anlegen/bearbeiten (ETag/If-Match). Organisation/Team: Anzeigename, Modelle
  // (Team), Limits (Fenster gleitend/Kalender, F25), Payload-Modus und Reasoning mit geerbtem Wert (F30/F185).
  // Nutzer: E-Mail, OIDC-Issuer und -Kennung vorab (F90), Rollen.
  import { untrack } from 'svelte';
  import { listPage } from '../../lib/api/client';
  import { modelGroups, organizations, teams, users, NAME_HINT, NAME_RE, type ModelGroup, type Organization, type RoleBinding, type S, type Team, type User } from '../../lib/api/resources';
  import { handleError, splitList } from '../../lib/admin/errors.svelte';
  import FormDialog from '../../lib/admin/FormDialog.svelte';
  import InheritFields from '../../lib/admin/InheritFields.svelte';
  import { fromRows, toRows, type LimitRow } from '../../lib/admin/limits';
  import LimitsEditor from '../../lib/admin/LimitsEditor.svelte';
  import RoleEditor from '../../lib/admin/RoleEditor.svelte';
  import { catalog, loadCatalog } from '../../lib/context.svelte';
  import { canWrite } from '../../lib/rbac.svelte';
  import { toast } from '../../lib/toast.svelte';
  import Field from '../../lib/ui/Field.svelte';
  import Icon from '../../lib/ui/Icon.svelte';
  import Input from '../../lib/ui/Input.svelte';
  import Select from '../../lib/ui/Select.svelte';

  let {
    open = $bindable(false),
    kind,
    editName = null,
    onsaved,
  }: { open?: boolean; kind: Kind; editName?: string | null; onsaved: (name: string) => void } = $props();

  const KIND_LABEL: Record<Kind, string> = { org: 'Organisation', team: 'Team', user: 'Nutzer' };
  const resOf = (k: Kind) => (k === 'org' ? organizations : k === 'team' ? teams : users);
  const blank = () => ({
    name: '',
    displayName: '',
    organization: '',
    allowedModels: '',
    email: '',
    oidcIssuer: '',
    oidcSubject: '',
    roles: [] as RoleBinding[],
    payloadMode: '',
    exposeReasoning: '',
    limits: [] as LimitRow[],
  });
  let f = $state(blank());
  let etag = $state<string | null>(null);
  let loadedSpec = $state<Record<string, unknown>>({});
  let labels = $state<Record<string, string>>({});
  let busy = $state(false);
  let loading = $state(false);
  let formError = $state<string | null>(null);
  let errors = $state<Record<string, string>>({});
  let groups = $state<string[]>([]);

  const writableOrgs = () => catalog.orgs.map((o) => o.metadata.name).filter((o) => canWrite({ kind: 'team', organization: o, team: '\u0000neu' }));
  const orgSpec = (name: string) => (catalog.orgs.find((o) => o.metadata.name === name)?.spec ?? null) as S['OrganizationSpec'] | null;

  $effect(() => {
    if (!open) return;
    const n = editName;
    untrack(() => {
      formError = null;
      errors = {};
      etag = null;
      loadedSpec = {};
      labels = {};
      f = { ...blank(), organization: kind === 'team' ? (writableOrgs()[0] ?? '') : '' };
      if (kind !== 'user') {
        listPage<ModelGroup>(modelGroups.path, {}, 500)
          .then((p) => (groups = p.items.map((g) => g.metadata.name)))
          .catch(() => (groups = []));
      }
      if (n) void load(n);
    });
  });

  async function load(name: string) {
    loading = true;
    try {
      const r = await resOf(kind).get(name);
      const o = r.data as Organization | Team | User;
      etag = r.etag;
      labels = o.metadata.labels ?? {};
      const s = o.spec as Record<string, unknown>;
      loadedSpec = s;
      f = {
        name,
        displayName: (s.displayName as string | null) ?? '',
        organization: (s.organization as string) ?? '',
        allowedModels: ((s.allowedModels as string[] | undefined) ?? []).join(', '),
        email: (s.email as string | null) ?? '',
        oidcIssuer: (s.oidcIssuer as string | null) ?? '',
        oidcSubject: (s.oidcSubject as string | null) ?? '',
        roles: [...((s.roles as RoleBinding[] | undefined) ?? [])],
        payloadMode: (s.payloadMode as string | null) ?? '',
        exposeReasoning: s.exposeReasoning == null ? '' : String(s.exposeReasoning),
        limits: toRows(s.limits as S['LimitSpec'][] | undefined),
      };
    } catch (e) {
      handleError(e, { action: `${KIND_LABEL[kind]} laden` });
      open = false;
    } finally {
      loading = false;
    }
  }

  async function submit() {
    const e: Record<string, string> = {};
    if (!editName && !NAME_RE.test(f.name)) e.name = 'Ungültiger Name: a-z, 0-9, „-“, „.“, „_“, beginnt alphanumerisch.';
    if (kind === 'team' && !f.organization) e.organization = 'Bitte eine Organisation wählen.';
    if (kind === 'user' && f.email && !/^[^@\s]+@[^@\s]+$/.test(f.email)) e.email = 'Keine gültige E-Mail-Adresse.';
    if (kind === 'user' && !!f.oidcSubject.trim() !== !!f.oidcIssuer.trim()) e.oidcSubject = 'OIDC-Issuer und -Kennung nur gemeinsam angeben.';
    if (kind === 'user' && f.roles.some((r) => (r.role === 'team-admin' && !r.team) || (r.role === 'org-admin' && !r.organization && !r.team))) {
      e.roles = 'Org-Admin braucht eine Organisation, Team-Admin ein Team.';
    }
    const limits = kind === 'user' ? [] : fromRows(f.limits);
    if (limits === null) e.limits = 'Grenzen müssen ganze Zahlen ≥ 1 sein.';
    errors = e;
    formError = null;
    if (Object.keys(e).length) return;
    const name = editName ?? f.name;
    const opt = (s: string) => s.trim() || null;
    const inherit = { payloadMode: (f.payloadMode || null) as S['PayloadMode'] | null, exposeReasoning: f.exposeReasoning === '' ? null : f.exposeReasoning === 'true' };
    const metadata = { name, labels };
    busy = true;
    try {
      if (kind === 'org') {
        const spec = { ...loadedSpec, displayName: opt(f.displayName), limits: limits ?? [], ...inherit };
        if (editName) await organizations.update(name, { metadata, spec }, etag);
        else await organizations.create({ metadata, spec });
      } else if (kind === 'team') {
        const spec = { ...loadedSpec, organization: f.organization, displayName: opt(f.displayName), allowedModels: splitList(f.allowedModels), limits: limits ?? [], ...inherit };
        if (editName) await teams.update(name, { metadata, spec }, etag);
        else await teams.create({ metadata, spec });
      } else {
        const spec = { ...loadedSpec, displayName: opt(f.displayName), email: opt(f.email), oidcIssuer: opt(f.oidcIssuer), oidcSubject: opt(f.oidcSubject), roles: f.roles };
        if (editName) await users.update(name, { metadata, spec }, etag);
        else await users.create({ metadata, spec });
      }
      open = false;
      toast('success', `${KIND_LABEL[kind]} gespeichert`, name);
      if (kind !== 'user') void loadCatalog();
      onsaved(name);
    } catch (err) {
      handleError(err, { action: `${KIND_LABEL[kind]} speichern`, form: (m) => (formError = m), reload: () => editName && load(editName) });
    } finally {
      busy = false;
    }
  }
</script>

<FormDialog
  bind:open
  title={editName ? `${KIND_LABEL[kind]} „${editName}“ bearbeiten` : `${KIND_LABEL[kind]} anlegen`}
  size={kind === 'org' ? 'md' : 'lg'}
  {busy}
  {loading}
  error={formError}
  submitLabel={editName ? 'Speichern' : 'Anlegen'}
  onsubmit={submit}
  testid="entity-form"
>
  {#if !editName}
    <Field label="Name" required hint={NAME_HINT} error={errors.name} class={kind === 'org' ? 'sm:col-span-2' : ''}>
      {#snippet children({ id, describedby, invalid })}
        <Input {id} bind:value={f.name} aria-describedby={describedby} aria-invalid={invalid || undefined} autocomplete="off" placeholder={kind === 'org' ? 'acme' : kind === 'team' ? 'platform' : 'anna'} />
      {/snippet}
    </Field>
  {/if}
  {#if kind === 'team'}
    <Field label="Organisation" required error={errors.organization}>
      {#snippet children({ id, describedby, invalid })}
        <Select {id} {describedby} {invalid} bind:value={f.organization} items={(editName ? catalog.orgs.map((o) => o.metadata.name) : writableOrgs()).map((o) => ({ value: o, label: o }))} />
      {/snippet}
    </Field>
  {/if}
  <Field label="Anzeigename" optional class="sm:col-span-2">
    {#snippet children({ id, describedby })}
      <Input {id} bind:value={f.displayName} aria-describedby={describedby} autocomplete="off" />
    {/snippet}
  </Field>
  {#if kind === 'team'}
    <Field label="Erlaubte Modellgruppen" optional hint="Namen oder Aliasse, mit Komma getrennt; leer = alle." class="sm:col-span-2">
      {#snippet children({ id, describedby })}
        <Input {id} bind:value={f.allowedModels} aria-describedby={describedby} autocomplete="off" placeholder="gpt-4o, claude-sonnet" />
      {/snippet}
    </Field>
  {/if}
  {#if kind !== 'user'}
    <InheritFields
      bind:payloadMode={f.payloadMode}
      bind:exposeReasoning={f.exposeReasoning}
      parents={kind === 'team' && f.organization ? [{ label: `Organisation ${f.organization}`, spec: orgSpec(f.organization) }] : []}
    />
    <fieldset class="flex flex-col gap-1.5 sm:col-span-2">
      <legend class="mb-1 text-sm font-medium text-fg">Limits <span class="text-xs font-normal text-fg-subtle">(gelten zusätzlich zu Limits darüber und darunter)</span></legend>
      <LimitsEditor bind:value={f.limits} {groups} />
      {#if errors.limits}<p class="flex items-center gap-1 text-xs text-danger-fg"><Icon name="alert" size={12} />{errors.limits}</p>{/if}
    </fieldset>
  {/if}
  {#if kind === 'user'}
    <Field label="E-Mail" optional hint="Anzeige; Verknüpfung per E-Mail nur mit link_by_email." error={errors.email} class="sm:col-span-2">
      {#snippet children({ id, describedby, invalid })}
        <Input {id} type="email" bind:value={f.email} aria-describedby={describedby} aria-invalid={invalid || undefined} autocomplete="off" />
      {/snippet}
    </Field>
    <Field label="OIDC-Issuer" optional hint="Vorab eintragen, damit der erste Login den Nutzer eindeutig zuordnet (F90).">
      {#snippet children({ id, describedby })}<Input {id} bind:value={f.oidcIssuer} aria-describedby={describedby} autocomplete="off" placeholder="https://idp.example.org/realms/acme" />{/snippet}
    </Field>
    <Field label="OIDC-Kennung" optional hint="Wert des identity_claim (Standard sub, Entra: oid)." error={errors.oidcSubject}>
      {#snippet children({ id, describedby, invalid })}<Input {id} bind:value={f.oidcSubject} aria-describedby={describedby} aria-invalid={invalid || undefined} autocomplete="off" />{/snippet}
    </Field>
    <fieldset class="flex flex-col gap-1.5 sm:col-span-2">
      <legend class="mb-1 text-sm font-medium text-fg">Rollen <span class="text-xs font-normal text-fg-subtle">(zusätzlich zu Rollen aus IdP-Gruppen)</span></legend>
      <RoleEditor bind:value={f.roles} organizations={catalog.orgs.map((o) => o.metadata.name)} teams={catalog.teams.map((t) => t.metadata.name)} />
      {#if errors.roles}<p class="flex items-center gap-1 text-xs text-danger-fg"><Icon name="alert" size={12} />{errors.roles}</p>{/if}
    </fieldset>
  {/if}
</FormDialog>
