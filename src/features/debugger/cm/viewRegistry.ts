// Editor views the debugger decorates, by scope key (`script:<tab>` / `routine:<conn>:<sig>`).
// Commands read the cursor line and reveal lines through here; nothing else touches views.

import { EditorView } from '@codemirror/view';

const views = new Map<string, Set<EditorView>>();

export function registerView(scopeKey: string, view: EditorView): () => void {
  let set = views.get(scopeKey);
  if (!set) views.set(scopeKey, (set = new Set()));
  set.add(view);
  return () => {
    set.delete(view);
    if (set.size === 0) views.delete(scopeKey);
  };
}

export function viewOf(scopeKey: string): EditorView | undefined {
  const set = views.get(scopeKey);
  return set ? [...set][set.size - 1] : undefined;
}

/** 1-based document line of the main cursor, or `null` without a view. */
export function cursorLine(scopeKey: string): number | null {
  const view = viewOf(scopeKey);
  if (!view) return null;
  return view.state.doc.lineAt(view.state.selection.main.head).number;
}

/** Scroll `line` (1-based document line) to the middle of the view. */
export function revealDocLine(scopeKey: string, line: number): void {
  const view = viewOf(scopeKey);
  if (!view) return;
  const n = Math.min(Math.max(line, 1), view.state.doc.lines);
  const pos = view.state.doc.line(n).from;
  view.dispatch({ effects: EditorView.scrollIntoView(pos, { y: 'center' }) });
}

/** Current document text of the view, when mounted. */
export function docText(scopeKey: string): string | null {
  return viewOf(scopeKey)?.state.doc.toString() ?? null;
}

/** Insert text at the cursor of the view (used for the "create extension" statement). */
export function insertAtCursor(scopeKey: string, text: string): boolean {
  const view = viewOf(scopeKey);
  if (!view) return false;
  const { from, to } = view.state.selection.main;
  view.dispatch({ changes: { from, to, insert: text }, selection: { anchor: from + text.length } });
  return true;
}
