<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // Rollenzuweisungen bearbeiten (Nutzer, Admin-API-Keys): Rolle + optional Organisation/Team (Admin-API §5.4).
  import type { Role, RoleBinding } from '../api/resources';
  import { ROLE_LABEL } from '../session.svelte';
  import Button from '../ui/Button.svelte';
  import Icon from '../ui/Icon.svelte';
  import Select from '../ui/Select.svelte';
  import { btn, input } from '../ui/styles';

  let { value = $bindable([]), organizations = [], teams = [] }: { value?: RoleBinding[]; organizations?: string[]; teams?: string[] } = $props();

  const ROLES: { value: Role; hint: string }[] = [
    { value: 'admin', hint: 'alles im Geltungsbereich' },
    { value: 'org-admin', hint: 'Teams und Keys einer Organisation' },
    { value: 'team-admin', hint: 'Keys eines Teams' },
    { value: 'viewer', hint: 'nur lesen' },
    { value: 'auditor', hint: 'nur lesen (Audit)' },
  ];
  const uid = $props.id();

  function add() {
    value = [...value, { role: 'viewer' }];
  }
  function remove(i: number) {
    value = value.filter((_, j) => j !== i);
  }
  function set(i: number, patch: Partial<RoleBinding>) {
    value = value.map((b, j) => {
      if (j !== i) return b;
      const n = { ...b, ...patch };
      if (!n.organization) delete n.organization;
      if (!n.team) delete n.team;
      return n;
    });
  }
</script>

<div class="flex flex-col gap-2" data-testid="role-editor">
  {#each value as b, i (i)}
    <div class="grid grid-cols-[1fr_auto] gap-2 rounded-lg border border-line bg-surface-2 p-2 sm:grid-cols-[10rem_1fr_1fr_auto]">
      <Select
        label="Rolle {i + 1}"
        value={b.role}
        items={ROLES.map((r) => ({ value: r.value, label: ROLE_LABEL[r.value], hint: r.hint }))}
        onchange={(v) => set(i, { role: v as Role })}
      />
      <button type="button" class={btn('ghost', 'icon', 'sm:order-last')} aria-label="Rolle {i + 1} entfernen" onclick={() => remove(i)}><Icon name="x" /></button>
      <input
        class={[input, 'col-span-2 sm:col-span-1']}
        aria-label="Organisation (Rolle {i + 1})"
        placeholder={b.role === 'org-admin' ? 'Organisation (Pflicht)' : 'Organisation (optional)'}
        list="{uid}-orgs"
        value={b.organization ?? ''}
        oninput={(e) => set(i, { organization: e.currentTarget.value.trim() || null })}
      />
      <input
        class={[input, 'col-span-2 sm:col-span-1']}
        aria-label="Team (Rolle {i + 1})"
        placeholder={b.role === 'team-admin' ? 'Team (Pflicht)' : 'Team (optional)'}
        list="{uid}-teams"
        value={b.team ?? ''}
        oninput={(e) => set(i, { team: e.currentTarget.value.trim() || null })}
      />
    </div>
  {:else}
    <p class="text-sm text-fg-muted">Keine Rolle zugewiesen.</p>
  {/each}
  <div><Button size="sm" icon="plus" onclick={add}>Rolle hinzufügen</Button></div>
  <datalist id="{uid}-orgs">{#each organizations as o (o)}<option value={o}></option>{/each}</datalist>
  <datalist id="{uid}-teams">{#each teams as t (t)}<option value={t}></option>{/each}</datalist>
</div>
