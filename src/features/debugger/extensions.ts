import type { Extension } from '@codemirror/state';

const NONE: Extension[] = [];

/**
 * CodeMirror extensions of the debugger for an editor tab (breakpoint gutter, execution-line
 * highlight, inline values). Contract placeholder: package F returns a stable array per tab.
 */
export function debuggerExtensions(tabStateId: string): Extension[] {
  void tabStateId;
  return NONE;
}
