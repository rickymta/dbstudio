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
  | 'toggleSidebar'
  | 'focusResults'
  | 'palette'
  | 'help';

export interface Shortcut {
  id: ShortcutId;
  keys: string;
  /** Where it works: inside the editor only, or anywhere in the app. */
  scope: 'editor' | 'global';
}

export const SHORTCUTS: readonly Shortcut[] = [
  { id: 'run', keys: 'Mod-Enter', scope: 'editor' },
  { id: 'runAll', keys: 'Mod-Shift-Enter', scope: 'editor' },
  { id: 'explain', keys: 'Mod-e', scope: 'editor' },
  { id: 'cancel', keys: 'Mod-.', scope: 'editor' },
  { id: 'format', keys: 'Mod-Shift-f', scope: 'editor' },
  { id: 'save', keys: 'Mod-s', scope: 'editor' },
  { id: 'open', keys: 'Mod-o', scope: 'editor' },
  { id: 'history', keys: 'Mod-Shift-h', scope: 'editor' },
  { id: 'newTab', keys: 'Mod-t', scope: 'global' },
  { id: 'closeTab', keys: 'Mod-w', scope: 'global' },
  { id: 'findObject', keys: 'Mod-Shift-o', scope: 'global' },
  { id: 'toggleSidebar', keys: 'Mod-b', scope: 'global' },
  { id: 'focusResults', keys: 'F6', scope: 'editor' },
  { id: 'palette', keys: 'Mod-Shift-p', scope: 'global' },
  { id: 'help', keys: 'Mod-/', scope: 'global' },
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
