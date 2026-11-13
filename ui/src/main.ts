// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
import './app.css';
import { mount } from 'svelte';
import App from './App.svelte';
import { applyPrefs } from './lib/prefs.svelte';
import { loadSession } from './lib/session.svelte';

// Schema/Akzent vor dem ersten Rendern setzen (kein Inline-Skript im HTML wegen CSP).
applyPrefs();

const target = document.getElementById('app');
if (target) mount(App, { target });
// Anmeldung prüfen (ggf. Weiterleitung zum OIDC-Login).
loadSession();
