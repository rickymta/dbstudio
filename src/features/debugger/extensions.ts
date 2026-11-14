import type { Extension } from '@codemirror/state';
import { ensureScriptScope } from './debugActions';
import { scriptScope } from './debugStore';
import { onEditorTabRemoved } from '@/features/editor/editorStore';
import { createSourceExtension } from './cm/extension';

const cache = new Map<string, Extension[]>();

/**
 * CodeMirror extensions of the debugger for an editor tab (breakpoint gutter, execution-line
 * highlight, inline values, hover, key handler). One stable array per tab.
 */
export function debuggerExtensions(tabStateId: string): Extension[] {
  let extensions = cache.get(tabStateId);
  if (!extensions) {
    extensions = createSourceExtension({
      scopeKey: scriptScope(tabStateId),
      tabIdOf: () => tabStateId,
      ensure: () => ensureScriptScope(tabStateId),
    });
    cache.set(tabStateId, extensions);
  }
  return extensions;
}

/** Test helper. */
export function resetDebuggerExtensions(): void {
  cache.clear();
}

onEditorTabRemoved((tabStateId) => cache.delete(tabStateId));
