// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2
//
// Kleiner Markdown-Parser für Modellantworten im Playground (AIGW-54). Ergebnis ist ein Baum aus Blöcken und
// Inline-Teilen, den Markdown.svelte als normale Svelte-Elemente rendert – kein {@html}, kein innerHTML: Text bleibt
// Text (kein XSS), CSP-tauglich. Unterstützt: Überschriften, Absätze, Code-Blöcke (```), Listen (-, *, 1.),
// Zitate, Trennlinien, Tabellen (GFM, einfach); inline Code, **fett**, *kursiv*, ~~durch~~, Links/Autolinks
// (nur http/https/mailto, sonst bleibt der Text stehen).

export type Inline =
  | { t: 'text'; v: string }
  | { t: 'code'; v: string }
  | { t: 'strong' | 'em' | 'del'; c: Inline[] }
  | { t: 'link'; href: string; c: Inline[] }
  | { t: 'br' };

export type Block =
  | { t: 'h'; level: 1 | 2 | 3 | 4 | 5 | 6; c: Inline[] }
  | { t: 'p'; c: Inline[] }
  | { t: 'code'; lang: string; v: string }
  | { t: 'ul' | 'ol'; start: number; items: Block[][] }
  | { t: 'quote'; c: Block[] }
  | { t: 'hr' }
  | { t: 'table'; head: Inline[][]; rows: Inline[][][] };

const SAFE_URL = /^(https?:\/\/|mailto:)/i;

/** Nur unbedenkliche Ziele (http/https/mailto); sonst `null`. */
export function safeHref(url: string): string | null {
  const u = url.trim();
  if (!SAFE_URL.test(u)) return null;
  try {
    return new URL(u).href;
  } catch {
    return null;
  }
}

// ------------------------------------------------------------------ Inline

export function inline(src: string): Inline[] {
  const out: Inline[] = [];
  let buf = '';
  const flush = () => {
    if (buf) out.push({ t: 'text', v: buf });
    buf = '';
  };
  let i = 0;
  while (i < src.length) {
    const ch = src[i];
    const rest = src.slice(i);
    // Escape
    if (ch === '\\' && i + 1 < src.length && /[\\`*_~[\]()#>!|-]/.test(src[i + 1])) {
      buf += src[i + 1];
      i += 2;
      continue;
    }
    // harter Umbruch: zwei Leerzeichen + \n bzw. \n innerhalb eines Absatzes
    if (ch === '\n') {
      flush();
      out.push({ t: 'br' });
      i++;
      continue;
    }
    if (ch === '`') {
      const m = /^(`+)([\s\S]*?[^`])\1(?!`)/.exec(rest);
      if (m) {
        flush();
        out.push({ t: 'code', v: m[2].replace(/^ (.*) $/, '$1') });
        i += m[0].length;
        continue;
      }
    }
    const strong = /^(\*\*|__)(?=\S)([\s\S]*?\S)\1/.exec(rest);
    if (strong) {
      flush();
      out.push({ t: 'strong', c: inline(strong[2]) });
      i += strong[0].length;
      continue;
    }
    const del = /^~~(?=\S)([\s\S]*?\S)~~/.exec(rest);
    if (del) {
      flush();
      out.push({ t: 'del', c: inline(del[1]) });
      i += del[0].length;
      continue;
    }
    const em = /^(\*|_)(?=\S)([\s\S]*?\S)\1(?!\1)/.exec(rest);
    if (em && !(ch === '_' && /\w/.test(src[i - 1] ?? ''))) {
      flush();
      out.push({ t: 'em', c: inline(em[2]) });
      i += em[0].length;
      continue;
    }
    if (ch === '[') {
      const m = /^\[([^\]]*)\]\(\s*<?([^)\s>]+)>?(?:\s+"[^"]*")?\s*\)/.exec(rest);
      if (m) {
        const href = safeHref(m[2]);
        flush();
        if (href) out.push({ t: 'link', href, c: inline(m[1]) });
        else out.push(...inline(m[1]));
        i += m[0].length;
        continue;
      }
    }
    const auto = /^<?(https?:\/\/[^\s<>"]+[^\s<>".,;:!?)\]])>?/.exec(rest);
    if (auto && (i === 0 || /[\s(]/.test(src[i - 1]))) {
      const href = safeHref(auto[1]);
      if (href) {
        flush();
        out.push({ t: 'link', href, c: [{ t: 'text', v: auto[1] }] });
        i += auto[0].length;
        continue;
      }
    }
    buf += ch;
    i++;
  }
  flush();
  return out;
}

// ------------------------------------------------------------------ Blöcke

const FENCE = /^ {0,3}(`{3,}|~{3,})\s*([\w+#.-]*)/;
const HEADING = /^ {0,3}(#{1,6})\s+(.*?)\s*#*\s*$/;
const HR = /^ {0,3}([-*_])(\s*\1){2,}\s*$/;
const LIST = /^( {0,3})([-*+]|\d{1,9}[.)])\s+(.*)$/;
const QUOTE = /^ {0,3}>\s?(.*)$/;
const TABLE_SEP = /^\s*\|?\s*:?-{1,}:?\s*(\|\s*:?-{1,}:?\s*)*\|?\s*$/;

function cells(line: string): string[] {
  let l = line.trim();
  if (l.startsWith('|')) l = l.slice(1);
  if (l.endsWith('|') && !l.endsWith('\\|')) l = l.slice(0, -1);
  return l.split(/(?<!\\)\|/).map((c) => c.trim());
}

/** Parst Markdown in Blöcke (Höchsttiefe für Verschachtelung, damit böswillige Eingaben nicht ausufern). */
export function parse(src: string, depth = 0): Block[] {
  const lines = src.replace(/\r\n?/g, '\n').split('\n');
  const out: Block[] = [];
  let para: string[] = [];
  const endPara = () => {
    if (para.length) out.push({ t: 'p', c: inline(para.join('\n').trim()) });
    para = [];
  };
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    if (!line.trim()) {
      endPara();
      i++;
      continue;
    }
    const fence = FENCE.exec(line);
    if (fence) {
      endPara();
      const marker = fence[1];
      const body: string[] = [];
      i++;
      while (i < lines.length && !lines[i].trimStart().startsWith(marker)) body.push(lines[i++]);
      i++; // schließender Zaun (oder Ende – ein offener Block beim Streamen)
      out.push({ t: 'code', lang: fence[2] ?? '', v: body.join('\n') });
      continue;
    }
    const h = HEADING.exec(line);
    if (h) {
      endPara();
      out.push({ t: 'h', level: h[1].length as 1, c: inline(h[2]) });
      i++;
      continue;
    }
    if (HR.test(line) && !para.length) {
      out.push({ t: 'hr' });
      i++;
      continue;
    }
    if (line.includes('|') && i + 1 < lines.length && TABLE_SEP.test(lines[i + 1]) && lines[i + 1].includes('-')) {
      endPara();
      const head = cells(line).map(inline);
      i += 2;
      const rows: Inline[][][] = [];
      while (i < lines.length && lines[i].includes('|') && lines[i].trim()) rows.push(cells(lines[i++]).map(inline));
      out.push({ t: 'table', head, rows });
      continue;
    }
    const q = QUOTE.exec(line);
    if (q && depth < 8) {
      endPara();
      const body: string[] = [];
      while (i < lines.length && lines[i].trim()) {
        const m = QUOTE.exec(lines[i]);
        body.push(m ? m[1] : lines[i]);
        i++;
      }
      out.push({ t: 'quote', c: parse(body.join('\n'), depth + 1) });
      continue;
    }
    const li = LIST.exec(line);
    if (li && depth < 8) {
      endPara();
      const ordered = /\d/.test(li[2]);
      const start = ordered ? parseInt(li[2], 10) : 1;
      const items: Block[][] = [];
      while (i < lines.length) {
        const m = LIST.exec(lines[i]);
        if (!m || /\d/.test(m[2]) !== ordered || m[1].length > li[1].length + 1) break;
        const indent = m[1].length + m[2].length + 1;
        const body = [m[3]];
        i++;
        while (i < lines.length) {
          const l = lines[i];
          if (!l.trim()) {
            // Leerzeile: Liste geht weiter, wenn eingerückter Inhalt folgt.
            if (i + 1 < lines.length && /^\s{2,}\S/.test(lines[i + 1])) {
              body.push('');
              i++;
              continue;
            }
            break;
          }
          const sub = LIST.exec(l);
          if (sub && sub[1].length <= li[1].length) break;
          if (!/^\s/.test(l) && !sub) {
            // Fortsetzungszeile ohne Einrückung (lazy continuation)
            if (FENCE.test(l) || HEADING.test(l) || QUOTE.test(l)) break;
            body.push(l);
            i++;
            continue;
          }
          body.push(l.slice(Math.min(indent, l.length - l.trimStart().length)));
          i++;
        }
        items.push(parse(body.join('\n'), depth + 1));
      }
      out.push({ t: ordered ? 'ol' : 'ul', start, items });
      continue;
    }
    para.push(line);
    i++;
  }
  endPara();
  return out;
}
