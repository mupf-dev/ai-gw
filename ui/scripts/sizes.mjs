// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2
//
// Bundle-Größen der gebauten UI (gzip -9 und Brotli q11, im Speicher berechnet): initial geladene Dateien
// (laut index.html) vs. per dynamic import nachgeladene Seiten. Ausgabe als JSON; `pnpm sizes`.
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join, relative } from 'node:path';
import { brotliCompressSync, constants, gzipSync } from 'node:zlib';

const root = process.argv[2] ?? 'dist';
const walk = (d) => readdirSync(d).flatMap((n) => (statSync(join(d, n)).isDirectory() ? walk(join(d, n)) : [join(d, n)]));
const html = readFileSync(join(root, 'index.html'), 'utf8');
const initial = new Set([...html.matchAll(/(?:src|href)="\/admin\/([^"]+\.(?:js|css))"/g)].map((m) => m[1]));
const files = walk(root)
  .filter((f) => /\.(js|css|html|svg)$/.test(f))
  .map((f) => {
    const data = readFileSync(f);
    const rel = relative(root, f);
    return {
      file: rel,
      raw: data.length,
      gz: gzipSync(data, { level: 9 }).length,
      br: brotliCompressSync(data, { params: { [constants.BROTLI_PARAM_QUALITY]: 11 } }).length,
      initial: initial.has(rel) || rel === 'index.html',
    };
  })
  .sort((a, b) => b.gz - a.gz);
const sum = (pred, k) => files.filter(pred).reduce((s, f) => s + f[k], 0);
const js = (f) => f.file.endsWith('.js');
const css = (f) => f.file.endsWith('.css');
const kib = (b) => Math.round((b / 1024) * 10) / 10;
const out = {
  initial: { jsGzKiB: kib(sum((f) => f.initial && js(f), 'gz')), cssGzKiB: kib(sum((f) => f.initial && css(f), 'gz')), jsBrKiB: kib(sum((f) => f.initial && js(f), 'br')) },
  all: { jsGzKiB: kib(sum(js, 'gz')), cssGzKiB: kib(sum(css, 'gz')), totalGzKiB: kib(sum(() => true, 'gz')), files: files.length },
  files: files.map((f) => ({ ...f, gzKiB: kib(f.gz) })),
};
console.log(JSON.stringify(out, null, 2));
