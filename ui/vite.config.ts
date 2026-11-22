// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2
import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';
import tailwindcss from '@tailwindcss/vite';

// Statische SPA unter /admin/, ins Binary eingebettet (crates/aigw-admin).
// Strikte CSP: keine Inline-Skripte, keine data:-Assets (assetsInlineLimit 0),
// kein Modulepreload-Polyfill (wäre ein Inline-Skript).
// Entwicklung: `pnpm dev` leitet /admin/api an ein laufendes aigw weiter (AIGW_DEV_API,
// Standard http://127.0.0.1:4001).
export default defineConfig({
  base: '/admin/',
  plugins: [tailwindcss(), svelte()],
  build: {
    target: 'es2022',
    assetsInlineLimit: 0,
    modulePreload: { polyfill: false },
    sourcemap: false,
    reportCompressedSize: false,
  },
  server: {
    proxy: {
      '/admin/api': { target: process.env.AIGW_DEV_API ?? 'http://127.0.0.1:4001' },
    },
  },
});
