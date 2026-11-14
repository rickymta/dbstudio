// Debugger shortcuts (design 5.2): JetBrains defaults plus a VS Code preset. One table drives
// the CodeMirror keymap of the editors, the key handler of the tool window and the tooltips.
// Combos are written `Ctrl+Alt+F9`; `Cmd` is the macOS command key.

import type { DebugKeymap } from '@/lib/types';

export type DebugCommand =
  | 'debug'
  | 'resume'
  | 'pause'
  | 'stop'
  | 'rerun'
  | 'stepOver'
  | 'stepInto'
  | 'smartStepInto'
  | 'forceStepInto'
  | 'stepOut'
  | 'runToCursor'
  | 'forceRunToCursor'
  | 'evaluate'
  | 'showExecutionPoint'
  | 'toggleBreakpoint'
  | 'viewBreakpoints'
  | 'muteBreakpoints';

/** When a command may fire: always, only with a live session, or only without one. */
type When = 'always' | 'session' | 'idle';

interface Binding {
  when: When;
  /** JetBrains on Windows/Linux, JetBrains on macOS, VS Code (all platforms). */
  jb: string[];
  jbMac: string[];
  vs: string[];
}

const T: Record<DebugCommand, Binding> = {
  debug: { when: 'idle', jb: ['Shift+F9'], jbMac: ['Ctrl+D'], vs: ['F5'] },
  resume: { when: 'session', jb: ['F9'], jbMac: ['Alt+Cmd+R'], vs: ['F5'] },
  pause: { when: 'session', jb: [], jbMac: [], vs: ['F6'] },
  stop: { when: 'session', jb: ['Ctrl+F2'], jbMac: ['Cmd+F2'], vs: ['Shift+F5'] },
  rerun: {
    when: 'session',
    jb: ['Ctrl+F5'],
    jbMac: ['Cmd+R'],
    vs: ['Ctrl+Shift+F5', 'Cmd+Shift+F5'],
  },
  stepOver: { when: 'session', jb: ['F8'], jbMac: ['F8'], vs: ['F10'] },
  stepInto: { when: 'session', jb: ['F7'], jbMac: ['F7'], vs: ['F11'] },
  smartStepInto: { when: 'session', jb: ['Shift+F7'], jbMac: ['Shift+F7'], vs: [] },
  forceStepInto: { when: 'session', jb: ['Alt+Shift+F7'], jbMac: ['Alt+Shift+F7'], vs: [] },
  stepOut: { when: 'session', jb: ['Shift+F8'], jbMac: ['Shift+F8'], vs: ['Shift+F11'] },
  runToCursor: { when: 'session', jb: ['Alt+F9'], jbMac: ['Alt+F9'], vs: [] },
  forceRunToCursor: { when: 'session', jb: ['Ctrl+Alt+F9'], jbMac: ['Alt+Cmd+F9'], vs: [] },
  evaluate: { when: 'session', jb: ['Alt+F8'], jbMac: ['Alt+F8'], vs: [] },
  showExecutionPoint: { when: 'session', jb: ['Alt+F10'], jbMac: ['Alt+F10'], vs: [] },
  toggleBreakpoint: { when: 'always', jb: ['Ctrl+F8'], jbMac: ['Cmd+F8'], vs: ['F9'] },
  viewBreakpoints: {
    when: 'always',
    jb: ['Ctrl+Shift+F8'],
    jbMac: ['Cmd+Shift+F8'],
    vs: [],
  },
  muteBreakpoints: { when: 'always', jb: [], jbMac: [], vs: [] },
};

export const DEBUG_COMMANDS = Object.keys(T) as DebugCommand[];

export const isMacPlatform = (): boolean =>
  typeof navigator !== 'undefined' &&
  /mac|iphone|ipad/i.test(navigator.platform || navigator.userAgent);

/** Combos of a command for the keymap on this platform. */
export function bindingsOf(
  command: DebugCommand,
  keymap: DebugKeymap,
  mac = isMacPlatform(),
): string[] {
  const b = T[command];
  if (keymap === 'vsCode') return b.vs.filter((c) => mac || !c.startsWith('Cmd+'));
  return mac ? b.jbMac : b.jb;
}

interface ParsedCombo {
  key: string;
  ctrl: boolean;
  alt: boolean;
  shift: boolean;
  meta: boolean;
}

function parse(combo: string): ParsedCombo {
  const parts = combo.split('+');
  const key = parts.pop()!;
  const has = (m: string) => parts.includes(m);
  return {
    key,
    ctrl: has('Ctrl'),
    alt: has('Alt'),
    shift: has('Shift'),
    meta: has('Cmd'),
  };
}

type KeyLike = Pick<KeyboardEvent, 'key' | 'code' | 'ctrlKey' | 'altKey' | 'shiftKey' | 'metaKey'>;

/** Whether the keyboard event is exactly `combo` (modifiers must match). */
export function matchesCombo(e: KeyLike, combo: string): boolean {
  const p = parse(combo);
  if (e.ctrlKey !== p.ctrl || e.altKey !== p.alt || e.shiftKey !== p.shift || e.metaKey !== p.meta)
    return false;
  if (p.key.length === 1) {
    // Letters: `code` survives Alt/Option remapping on macOS.
    return e.code === `Key${p.key.toUpperCase()}` || e.key.toLowerCase() === p.key.toLowerCase();
  }
  return e.key === p.key;
}

/** The command a key event triggers in the given state, or `null`. */
export function commandOfEvent(
  e: KeyLike,
  keymap: DebugKeymap,
  hasSession: boolean,
  mac = isMacPlatform(),
): DebugCommand | null {
  for (const command of DEBUG_COMMANDS) {
    const when = T[command].when;
    if (when === 'session' && !hasSession) continue;
    if (when === 'idle' && hasSession) continue;
    if (bindingsOf(command, keymap, mac).some((combo) => matchesCombo(e, combo))) return command;
  }
  return null;
}

const MAC_GLYPH: Record<string, string> = { Ctrl: '⌃', Alt: '⌥', Shift: '⇧', Cmd: '⌘' };

/** `Alt+Shift+F7` -> `⌥⇧F7` on macOS, unchanged elsewhere. */
export function formatCombo(combo: string, mac = isMacPlatform()): string {
  if (!mac) return combo;
  const parts = combo.split('+');
  const key = parts.pop()!;
  const order = ['Ctrl', 'Alt', 'Shift', 'Cmd'];
  const mods = order.filter((m) => parts.includes(m)).map((m) => MAC_GLYPH[m]);
  return [...mods, key.length === 1 ? key.toUpperCase() : key].join('');
}

/** Shortcut text for tooltips ("" when the command has no key in this keymap). */
export function shortcutLabel(
  command: DebugCommand,
  keymap: DebugKeymap,
  mac = isMacPlatform(),
): string {
  const first = bindingsOf(command, keymap, mac)[0];
  return first ? formatCombo(first, mac) : '';
}

/** Value for `aria-keyshortcuts` (ARIA syntax: `Control+F2`, `Meta+F2`). */
export function ariaShortcut(
  command: DebugCommand,
  keymap: DebugKeymap,
  mac = isMacPlatform(),
): string | undefined {
  const first = bindingsOf(command, keymap, mac)[0];
  return first?.replace('Ctrl', 'Control').replace('Cmd', 'Meta');
}
