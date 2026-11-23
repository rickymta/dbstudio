// The desktop app runs in a webview, which still has a browser's own keys: F5 / Ctrl+R reload
// the page (losing the open tabs' state), F3 / Ctrl+F / Ctrl+G open "find in page", F7 caret browsing, F12 dev tools...
// None of them belongs in the application, so their default action is cancelled. The
// application's own bindings on the same keys (F6, the debugger's F7-F11) keep working: they are
// handled before this listener, which only prevents what the webview would do afterwards.

import { ipcTransport } from '@/lib/ipc';

const FUNCTION_KEY = /^F([1-9]|1[0-2])$/;

/** Whether the webview's default action for this key press must be cancelled. */
export function isBrowserKey(e: KeyboardEvent): boolean {
  if (FUNCTION_KEY.test(e.key)) {
    // Alt+F4 closes the window: that one belongs to the operating system.
    return !(e.altKey && e.key === 'F4');
  }
  // Ctrl/Cmd+R and Ctrl/Cmd+Shift+R reload like F5; Ctrl/Cmd+F and Ctrl/Cmd+G (find, find
  // next) open the webview's "find in page" bar. Ctrl+F is the application's Spotlight and
  // the editor's own search, both handled before this listener.
  return (e.ctrlKey || e.metaKey) && !e.altKey && ['r', 'f', 'g'].includes(e.key.toLowerCase());
}

/**
 * Only the packaged desktop app blocks them: in a plain browser the keys are the browser's, and
 * during development F5 / F12 are how the page is reloaded and inspected.
 */
export function shouldBlockBrowserKeys(): boolean {
  return ipcTransport() === 'tauri' && !import.meta.env.DEV;
}

/** Cancel the webview's default action for browser keys; returns the function that undoes it. */
export function blockBrowserKeys(target: Window = window): () => void {
  const onKey = (e: KeyboardEvent) => {
    if (isBrowserKey(e)) e.preventDefault();
  };
  // Bubble phase: the editor and the debugger see the key first.
  target.addEventListener('keydown', onKey);
  return () => target.removeEventListener('keydown', onKey);
}
