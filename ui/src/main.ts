// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2
import './app.css';
import { mount } from 'svelte';
import App from './App.svelte';
import './lib/mode.svelte';
import { applyPrefs, prefs } from './lib/prefs.svelte';
import { loadSession } from './lib/session.svelte';

// Schema/Akzent vor dem ersten Rendern setzen (kein Inline-Skript im HTML wegen CSP).
applyPrefs();

const target = document.getElementById('app');
if (target) mount(App, { target });
// Anmeldung prüfen (ggf. Weiterleitung zum OIDC-Login bzw. Token-Dialog, wenn echte Daten verlangt sind).
loadSession(() => prefs.mockMode !== 'on');
