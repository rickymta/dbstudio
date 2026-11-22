// Keyboard shortcuts (APP-04). One table drives the CodeMirror keymap, the global handler and
// the cheat-sheet dialog. Keys use CodeMirror's syntax: `Mod` is Cmd on macOS, Ctrl elsewhere.

export type ShortcutId =
  | 'run'
  | 'runAll'
  | 'explain'
  | 'cancel'
  | 'format'
  | 'save'
  | 'open'
  | 'history'
  | 'newTab'
  | 'closeTab'
  | 'findObject'
  | 'launchpad'
  | 'toggleSidebar'
  | 'focusResults'
  | 'palette'
  | 'settings'
  | 'activity'
  | 'zoomIn'
  | 'zoomOut'
  | 'zoomReset'
  | 'help';

/** Sections of the keyboard shortcuts window, in display order. */
export const SHORTCUT_GROUPS = ['editor', 'app', 'window'] as const;
export type ShortcutGroup = (typeof SHORTCUT_GROUPS)[number];

export interface Shortcut {
  id: ShortcutId;
  keys: string;
  /** Other combos that do the same (shown in the cheat sheet, matched by `matchesShortcut`). */
  alt?: string[];
  /** Where it works: inside the editor only, or anywhere in the app. */
  scope: 'editor' | 'global';
  group: ShortcutGroup;
}

export const SHORTCUTS: readonly Shortcut[] = [
  { id: 'run', keys: 'Mod-Enter', scope: 'editor', group: 'editor' },
  { id: 'runAll', keys: 'Mod-Shift-Enter', scope: 'editor', group: 'editor' },
  { id: 'explain', keys: 'Mod-e', scope: 'editor', group: 'editor' },
  { id: 'cancel', keys: 'Mod-.', scope: 'editor', group: 'editor' },
  { id: 'format', keys: 'Mod-Shift-f', scope: 'editor', group: 'editor' },
  { id: 'save', keys: 'Mod-s', scope: 'editor', group: 'editor' },
  { id: 'open', keys: 'Mod-o', scope: 'editor', group: 'editor' },
  { id: 'history', keys: 'Mod-Shift-h', scope: 'editor', group: 'editor' },
  { id: 'focusResults', keys: 'F6', scope: 'editor', group: 'editor' },
  { id: 'newTab', keys: 'Mod-t', scope: 'global', group: 'app' },
  { id: 'closeTab', keys: 'Mod-w', scope: 'global', group: 'app' },
  { id: 'findObject', keys: 'Mod-Shift-o', scope: 'global', group: 'app' },
  { id: 'launchpad', keys: 'Mod-Shift-k', scope: 'global', group: 'app' },
  { id: 'toggleSidebar', keys: 'Mod-b', scope: 'global', group: 'app' },
  { id: 'palette', keys: 'Mod-Shift-p', scope: 'global', group: 'app' },
  // Handled by the status bar (settings) and by `AppWindows` (the rest), which listen before
  // the editor so the combos also work while typing a statement.
  { id: 'settings', keys: 'Mod-,', scope: 'global', group: 'app' },
  { id: 'activity', keys: 'Mod-Shift-l', scope: 'global', group: 'window' },
  { id: 'zoomIn', keys: 'Mod-=', alt: ['Mod-+'], scope: 'global', group: 'window' },
  { id: 'zoomOut', keys: 'Mod--', scope: 'global', group: 'window' },
  { id: 'zoomReset', keys: 'Mod-0', scope: 'global', group: 'window' },
  { id: 'help', keys: 'Mod-/', scope: 'global', group: 'window' },
];

export const keysOf = (id: ShortcutId): string => SHORTCUTS.find((s) => s.id === id)!.keys;

export const isMac = (): boolean =>
  typeof navigator !== 'undefined' &&
  /mac|iphone|ipad/i.test(navigator.platform || navigator.userAgent);

/** `Mod-Shift-f` -> `⌘⇧F` on macOS, `Ctrl+Shift+F` elsewhere. */
export function formatKeys(keys: string, mac = isMac()): string {
  const parts = keys.split(/-(?=.)/);
  const key = parts.pop()!;
  const names = parts.map((p) => {
    if (p === 'Mod') return mac ? '⌘' : 'Ctrl';
    if (p === 'Shift') return mac ? '⇧' : 'Shift';
    if (p === 'Alt') return mac ? '⌥' : 'Alt';
    return p;
  });
  const shown = key.length === 1 ? key.toUpperCase() : key;
  return mac ? [...names, shown].join('') : [...names, shown].join('+');
}

/** Whether a keyboard event is the shortcut `keys` (letters compare case-insensitively). */
export function matchesKeys(e: KeyboardEvent, keys: string, mac = isMac()): boolean {
  const parts = keys.split(/-(?=.)/);
  const key = parts.pop()!;
  const need = new Set(parts);
  const mod = mac ? e.metaKey : e.ctrlKey;
  if (need.has('Mod') !== mod) return false;
  if (need.has('Shift') !== e.shiftKey) return false;
  if (need.has('Alt') !== e.altKey) return false;
  // The other platform's modifier must not be pressed.
  if (mac ? e.ctrlKey : e.metaKey) return false;
  return e.key.toLowerCase() === key.toLowerCase();
}

/**
 * Whether the event is shortcut `id` or one of its alternatives. Symbols typed with Shift
 * (`+`) match with or without it, since layouts differ.
 */
export function matchesShortcut(e: KeyboardEvent, id: ShortcutId, mac = isMac()): boolean {
  const s = SHORTCUTS.find((x) => x.id === id)!;
  return [s.keys, ...(s.alt ?? [])].some((keys) => {
    if (matchesKeys(e, keys, mac)) return true;
    const key = keys.slice(-1);
    if (!/[^\w]/.test(key) || keys.includes('Shift')) return false;
    return matchesKeys(e, keys.replace(/-(?=.$)/, '-Shift-'), mac);
  });
}
