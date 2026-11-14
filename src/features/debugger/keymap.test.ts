import { describe, expect, it } from 'vitest';
import { ariaShortcut, bindingsOf, commandOfEvent, formatCombo, shortcutLabel } from './keymap';

const ev = (
  key: string,
  mods: Partial<Record<'ctrlKey' | 'altKey' | 'shiftKey' | 'metaKey', boolean>> = {},
) => ({
  key,
  code: key.length === 1 ? `Key${key.toUpperCase()}` : key,
  ctrlKey: false,
  altKey: false,
  shiftKey: false,
  metaKey: false,
  ...mods,
});

describe('debugger keymap', () => {
  it('maps the JetBrains defaults (Windows/Linux)', () => {
    const jb = (e: ReturnType<typeof ev>) => commandOfEvent(e, 'jetBrains', true, false);
    expect(jb(ev('F9'))).toBe('resume');
    expect(jb(ev('F8'))).toBe('stepOver');
    expect(jb(ev('F7'))).toBe('stepInto');
    expect(jb(ev('F7', { altKey: true, shiftKey: true }))).toBe('forceStepInto');
    expect(jb(ev('F8', { shiftKey: true }))).toBe('stepOut');
    expect(jb(ev('F9', { altKey: true }))).toBe('runToCursor');
    expect(jb(ev('F8', { altKey: true }))).toBe('evaluate');
    expect(jb(ev('F2', { ctrlKey: true }))).toBe('stop');
    expect(jb(ev('F8', { ctrlKey: true }))).toBe('toggleBreakpoint');
    expect(jb(ev('F8', { ctrlKey: true, shiftKey: true }))).toBe('viewBreakpoints');
  });

  it('uses the command key on macOS', () => {
    const mac = (e: ReturnType<typeof ev>) => commandOfEvent(e, 'jetBrains', true, true);
    expect(mac(ev('F2', { metaKey: true }))).toBe('stop');
    expect(mac(ev('F8', { metaKey: true, shiftKey: true }))).toBe('viewBreakpoints');
    expect(mac(ev('r', { altKey: true, metaKey: true }))).toBe('resume');
    expect(mac(ev('F2', { ctrlKey: true }))).toBeNull();
  });

  it('maps the VS Code preset', () => {
    const vs = (e: ReturnType<typeof ev>, session = true) =>
      commandOfEvent(e, 'vsCode', session, false);
    expect(vs(ev('F5'))).toBe('resume');
    expect(vs(ev('F5'), false)).toBe('debug');
    expect(vs(ev('F10'))).toBe('stepOver');
    expect(vs(ev('F11'))).toBe('stepInto');
    expect(vs(ev('F11', { shiftKey: true }))).toBe('stepOut');
    expect(vs(ev('F5', { shiftKey: true }))).toBe('stop');
    expect(vs(ev('F9'))).toBe('toggleBreakpoint');
    // JetBrains keys are not active in this preset
    expect(vs(ev('F8'))).toBeNull();
  });

  it('only fires session commands while a session exists', () => {
    expect(commandOfEvent(ev('F8'), 'jetBrains', false, false)).toBeNull();
    expect(commandOfEvent(ev('F8', { ctrlKey: true }), 'jetBrains', false, false)).toBe(
      'toggleBreakpoint',
    );
    expect(commandOfEvent(ev('F9', { shiftKey: true }), 'jetBrains', false, false)).toBe('debug');
    expect(commandOfEvent(ev('F9', { shiftKey: true }), 'jetBrains', true, false)).toBeNull();
    expect(commandOfEvent(ev('F9'), 'jetBrains', true, false)).toBe('resume');
  });

  it('formats labels for tooltips', () => {
    expect(formatCombo('Alt+Shift+F7', true)).toBe('⌥⇧F7');
    expect(formatCombo('Alt+Shift+F7', false)).toBe('Alt+Shift+F7');
    expect(shortcutLabel('stepOver', 'vsCode', false)).toBe('F10');
    expect(shortcutLabel('pause', 'jetBrains', false)).toBe('');
    expect(ariaShortcut('stop', 'jetBrains', false)).toBe('Control+F2');
    expect(bindingsOf('rerun', 'vsCode', false)).toEqual(['Ctrl+Shift+F5']);
  });
});
