// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Klassen-Rezepte für Bits-UI-Bausteine und eigene Komponenten. Bits UI ist headless; alle Teile werden
// über diese Rezepte gestylt, damit Trigger (Bits) und eigene Buttons gleich aussehen.

export type ButtonVariant = 'primary' | 'secondary' | 'ghost' | 'danger' | 'link';
export type ButtonSize = 'sm' | 'md' | 'icon' | 'icon-sm';

const BTN_BASE =
  'inline-flex items-center justify-center gap-1.5 whitespace-nowrap rounded-md font-medium select-none transition-colors duration-150 disabled:pointer-events-none disabled:opacity-50 aria-disabled:pointer-events-none aria-disabled:opacity-50';
const BTN_VARIANT: Record<ButtonVariant, string> = {
  primary: 'bg-accent text-on-accent hover:bg-accent-hover shadow-card',
  secondary: 'border border-line bg-surface text-fg hover:bg-surface-hover shadow-card',
  ghost: 'text-fg-muted hover:bg-surface-hover hover:text-fg',
  danger: 'bg-danger text-on-danger hover:bg-danger-hover',
  link: 'text-accent-text underline-offset-2 hover:underline px-0',
};
const BTN_SIZE: Record<ButtonSize, string> = {
  sm: 'h-7 px-2.5 text-sm',
  md: 'h-9 px-3.5 text-base',
  icon: 'size-9',
  'icon-sm': 'size-7',
};

export function btn(variant: ButtonVariant = 'secondary', size: ButtonSize = 'md', extra = ''): string {
  return `${BTN_BASE} ${BTN_VARIANT[variant]} ${variant === 'link' ? '' : BTN_SIZE[size]} ${extra}`;
}

export const input =
  'h-9 w-full min-w-0 rounded-md border border-line bg-surface px-3 text-base text-fg placeholder:text-fg-subtle transition-colors hover:border-line-strong focus-visible:border-accent focus-visible:outline-2 focus-visible:outline-offset-0 focus-visible:outline-focus aria-invalid:border-danger disabled:opacity-60';

export const popover =
  'anim-pop z-50 min-w-[12rem] rounded-lg border border-line bg-surface p-1 text-fg shadow-pop outline-none';
export const menuItem =
  'flex h-8 cursor-default select-none items-center gap-2 rounded-md px-2 text-base text-fg outline-none data-highlighted:bg-surface-hover data-disabled:opacity-50 [&_svg]:text-fg-muted';
export const menuLabel = 'px-2 pt-2 pb-1 label-caps text-fg-subtle';
export const menuSeparator = 'my-1 h-px bg-line';

export const tooltip =
  'anim-fade z-50 max-w-xs rounded-md bg-fg px-2 py-1 text-xs text-surface shadow-pop';

export const tabsList = 'flex items-center gap-1 border-b border-line';
export const tabsTrigger =
  'relative -mb-px inline-flex h-9 items-center gap-1.5 border-b-2 border-transparent px-3 text-base font-medium text-fg-muted transition-colors hover:text-fg data-[state=active]:border-accent data-[state=active]:text-fg';

/** Segmentierte Umschaltung (z. B. Zeitraum), als Bits-ToggleGroup oder Tabs-Variante. */
export const segment = 'inline-flex items-center rounded-md border border-line bg-surface-2 p-0.5';
export const segmentItem =
  'inline-flex h-7 items-center rounded-sm px-2.5 text-sm font-medium text-fg-muted transition-colors hover:text-fg data-[state=active]:bg-surface data-[state=active]:text-fg data-[state=active]:shadow-card data-[state=on]:bg-surface data-[state=on]:text-fg data-[state=on]:shadow-card';

/** Klassen zusammensetzen (falsy-Werte entfallen) – für Bits-UI-Props, die einen String erwarten. */
export function cx(...parts: Array<string | false | null | undefined>): string {
  return parts.filter(Boolean).join(' ');
}

export const card ='rounded-lg border border-line bg-surface shadow-card';

export const textarea =
  'min-h-20 w-full min-w-0 rounded-md border border-line bg-surface px-3 py-2 font-mono text-sm text-fg placeholder:text-fg-subtle transition-colors hover:border-line-strong focus-visible:border-accent focus-visible:outline-2 focus-visible:outline-offset-0 focus-visible:outline-focus aria-invalid:border-danger disabled:opacity-60';
export const checkbox =
  'grid size-4 shrink-0 place-items-center rounded-sm border border-line-strong bg-surface data-[state=checked]:border-accent data-[state=checked]:bg-accent data-[state=checked]:text-on-accent disabled:opacity-50';
export const switchRoot =
  'inline-flex h-5 w-9 shrink-0 items-center rounded-full bg-line-strong p-0.5 transition-colors data-[state=checked]:bg-accent disabled:opacity-50';
export const switchThumb = 'size-4 rounded-full bg-surface shadow-card transition-transform data-[state=checked]:translate-x-4';
export const kbd =
  'inline-flex h-5 min-w-5 items-center justify-center rounded-sm border border-line bg-surface-2 px-1 font-mono text-2xs text-fg-muted';
