import { describe, expect, it, vi } from 'vitest';
import { blockBrowserKeys, isBrowserKey } from './browserKeys';

const key = (k: string, init: KeyboardEventInit = {}) =>
  new KeyboardEvent('keydown', { key: k, cancelable: true, bubbles: true, ...init });

describe('isBrowserKey', () => {
  it('covers F1 to F12 with any modifier', () => {
    for (let n = 1; n <= 12; n++) expect(isBrowserKey(key(`F${n}`)), `F${n}`).toBe(true);
    expect(isBrowserKey(key('F5', { ctrlKey: true }))).toBe(true);
    expect(isBrowserKey(key('F5', { shiftKey: true }))).toBe(true);
    expect(isBrowserKey(key('F13'))).toBe(false);
    expect(isBrowserKey(key('F'))).toBe(false);
  });

  it('leaves Alt+F4 to the operating system', () => {
    expect(isBrowserKey(key('F4', { altKey: true }))).toBe(false);
    expect(isBrowserKey(key('F4'))).toBe(true);
  });

  it('covers find and find next, which open the find bar of the webview', () => {
    expect(isBrowserKey(key('f', { ctrlKey: true }))).toBe(true);
    expect(isBrowserKey(key('F', { metaKey: true }))).toBe(true);
    expect(isBrowserKey(key('g', { ctrlKey: true }))).toBe(true);
    expect(isBrowserKey(key('G', { ctrlKey: true, shiftKey: true }))).toBe(true);
    expect(isBrowserKey(key('f'))).toBe(false);
    expect(isBrowserKey(key('f', { ctrlKey: true, altKey: true }))).toBe(false);
  });

  it('covers the reload shortcuts and nothing else on R', () => {
    expect(isBrowserKey(key('r', { ctrlKey: true }))).toBe(true);
    expect(isBrowserKey(key('R', { ctrlKey: true, shiftKey: true }))).toBe(true);
    expect(isBrowserKey(key('r', { metaKey: true }))).toBe(true);
    expect(isBrowserKey(key('r'))).toBe(false);
    expect(isBrowserKey(key('r', { ctrlKey: true, altKey: true }))).toBe(false);
    expect(isBrowserKey(key('Enter', { ctrlKey: true }))).toBe(false);
  });
});

describe('blockBrowserKeys', () => {
  it('cancels the default action but lets the application handle the key first', () => {
    const seen = vi.fn((e: KeyboardEvent) => expect(e.defaultPrevented).toBe(false));
    document.body.addEventListener('keydown', seen);
    const undo = blockBrowserKeys();

    const f5 = key('F5');
    document.body.dispatchEvent(f5);
    expect(seen).toHaveBeenCalledTimes(1);
    expect(f5.defaultPrevented).toBe(true);

    const letter = key('a');
    document.body.dispatchEvent(letter);
    expect(letter.defaultPrevented).toBe(false);

    undo();
    const again = key('F5');
    document.body.dispatchEvent(again);
    expect(again.defaultPrevented).toBe(false);
    document.body.removeEventListener('keydown', seen);
  });
});
