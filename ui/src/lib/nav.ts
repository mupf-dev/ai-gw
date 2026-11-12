// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Navigationseinträge der Sidebar. Weitere Seiten (Keys, Teams, Modelle, Routen, Logs …)
// kommen mit den jeweiligen Arbeitspaketen hinzu.
export type NavItem = { id: string; label: string };

export const nav: NavItem[] = [{ id: 'overview', label: 'Übersicht' }];
