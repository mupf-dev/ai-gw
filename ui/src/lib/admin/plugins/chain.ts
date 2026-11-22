// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2
//
// Vorschau der wirksamen Plugin-Kette für einen Kontext (Organisation, Team, Key, Modellgruppe) – dieselben
// Regeln wie aigw-plugins/src/index.rs (docs/konzept/12-plugins.md §4):
// 1. Ebenen global → Organisation → Team → API-Key → Modellgruppe; `enabled: false` fällt ganz heraus.
// 2. `append` hängt an; `override` entfernt geerbte Bindungen desselben Plugins und hängt sich
//    an; `disable` entfernt sie nur. Ist eine geerbte Bindung `locked`, ist override/disable wirkungslos.
// 3. Ausführung: `order` aufsteigend über alle Ebenen, bei Gleichstand äußere Ebene zuerst, dann Name.
// Antwort- und Stream-Haken laufen in umgekehrter Reihenfolge (Zwiebelprinzip, F167b).
import type { PluginBinding, S } from '../../api/resources';

export interface ChainContext {
  organization?: string;
  team?: string;
  apiKey?: string;
  modelGroup?: string;
}

export interface ChainStep {
  binding: PluginBinding;
  /** Rang der Ebene (0 = global … 4 = Modellgruppe). */
  rank: number;
}

export interface RemovedStep {
  binding: PluginBinding;
  /** Entfernt durch diese Bindung (override/disable) bzw. – bei wirkungslosen – gesperrt durch diese. */
  by: string;
}

export const LEVEL_RANK: Record<S['ScopeLevel'], number> = { global: 0, organization: 1, team: 2, 'api-key': 3, 'model-group': 4 };

function matches(b: PluginBinding, c: ChainContext): boolean {
  const t = b.spec.scope.target ?? '';
  switch (b.spec.scope.level) {
    case 'global':
      return true;
    case 'organization':
      return !!c.organization && t === c.organization;
    case 'team':
      return !!c.team && t === c.team;
    case 'api-key':
      return !!c.apiKey && t === c.apiKey;
    case 'model-group':
      return !!c.modelGroup && t === c.modelGroup;
  }
}

const byName = (a: PluginBinding, b: PluginBinding) => (a.metadata.name < b.metadata.name ? -1 : a.metadata.name > b.metadata.name ? 1 : 0);

/** Wirksame Kette (Anfrage-Reihenfolge) und durch `override`/`disable` entfernte Bindungen. */
export function compose(all: PluginBinding[], c: ChainContext): { steps: ChainStep[]; removed: RemovedStep[]; ineffective: RemovedStep[] } {
  const relevant = all.filter((b) => b.spec.enabled !== false && matches(b, c));
  let acc: ChainStep[] = [];
  const removed: RemovedStep[] = [];
  const ineffective: RemovedStep[] = [];
  for (let rank = 0; rank <= 4; rank++) {
    const level = relevant
      .filter((b) => LEVEL_RANK[b.spec.scope.level] === rank)
      .sort((a, b) => (a.spec.order ?? 0) - (b.spec.order ?? 0) || byName(a, b));
    for (const b of level) {
      const inherit = b.spec.inherit ?? 'append';
      if (inherit === 'append') {
        acc.push({ binding: b, rank });
        continue;
      }
      const outer = (s: ChainStep) => s.rank < rank && s.binding.spec.plugin === b.spec.plugin;
      const lock = acc.find((s) => outer(s) && s.binding.spec.locked);
      if (lock) {
        // Gesperrte geerbte Bindung: override/disable ist wirkungslos (auch die Bindung selbst greift nicht).
        ineffective.push({ binding: b, by: lock.binding.metadata.name });
        continue;
      }
      for (const s of acc.filter(outer)) removed.push({ binding: s.binding, by: b.metadata.name });
      acc = acc.filter((s) => !outer(s));
      if (inherit === 'override') acc.push({ binding: b, rank });
    }
  }
  acc.sort((a, b) => (a.binding.spec.order ?? 0) - (b.binding.spec.order ?? 0) || a.rank - b.rank || byName(a.binding, b.binding));
  return { steps: acc, removed, ineffective };
}
