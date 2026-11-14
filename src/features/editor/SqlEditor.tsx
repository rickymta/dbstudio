// CodeMirror 6 editor of an editor tab. Loaded lazily (EditorTab) to keep the initial bundle
// small. The document is NOT controlled: the text of the tab lives in `editorStore` and flows
// in once, as `initialText`; later changes from outside (format, open file) go through the
// imperative handle.

import {
  autocompletion,
  closeBrackets,
  closeBracketsKeymap,
  completionKeymap,
  snippet,
} from '@codemirror/autocomplete';
import { defaultKeymap, history, historyKeymap, indentWithTab } from '@codemirror/commands';
import { bracketMatching, indentOnInput } from '@codemirror/language';
import { setDiagnostics, lintKeymap, type Diagnostic } from '@codemirror/lint';
import { highlightSelectionMatches, searchKeymap } from '@codemirror/search';
import { Compartment, EditorState, type Extension } from '@codemirror/state';
import {
  EditorView,
  drawSelection,
  highlightActiveLine,
  highlightActiveLineGutter,
  keymap,
  lineNumbers,
} from '@codemirror/view';
import { forwardRef, useEffect, useImperativeHandle, useRef } from 'react';
import type { ScriptDialect } from '@/lib/types';
import type { EditorMode } from '@/stores/tabs';
import { editorHighlight, editorTheme } from './cmTheme';
import { createCompletionSource, type CompletionTarget } from './completionSource';
import { languageExtension } from './dialects';
import type { ErrorMark } from './errorMarks';
import { keysOf, type ShortcutId } from './shortcuts';

export interface SqlEditorHandle {
  focus: () => void;
  /** Report pending text changes now (they are otherwise debounced). */
  flush: () => void;
  /** Replace the whole document (keeps the cursor near its old place). */
  replaceDoc: (text: string) => void;
  /** Replace the selection, or the whole document when nothing is selected. */
  replaceSelectionOrDoc: (map: (text: string) => string) => void;
  /** Insert at the cursor (replacing the selection); `snippet` expands `${1:name}` placeholders. */
  insertText: (text: string, snippet: boolean) => void;
  /** Move the cursor and scroll to it. */
  reveal: (from: number, to?: number) => void;
}

export interface SqlEditorProps {
  initialText: string;
  initialScroll?: { top: number; left: number };
  onScroll?: (position: { top: number; left: number }) => void;
  initialSelection: { from: number; to: number };
  mode: EditorMode;
  dialect: ScriptDialect;
  /** Drives autocomplete; read on every request. */
  target: CompletionTarget;
  /** Error marks (run error and lexer problems). */
  marks: ErrorMark[];
  ariaLabel: string;
  /** Extensions of other features (the debugger's gutter and inline values); keep the array stable. */
  extraExtensions?: Extension[];
  /** One handler for every editor shortcut (see `shortcuts.ts`). */
  onShortcut: (id: ShortcutId) => void;
  onChange: (text: string) => void;
  onSelection: (selection: { from: number; to: number }) => void;
}

/** Typing reports the text after this pause: `toString` of a large document is not free. */
const CHANGE_DEBOUNCE_MS = 150;

const EDITOR_SHORTCUTS: ShortcutId[] = [
  'run',
  'runAll',
  'explain',
  'cancel',
  'format',
  'save',
  'open',
  'history',
  'focusResults',
];

export const SqlEditor = forwardRef<SqlEditorHandle, SqlEditorProps>(
  function SqlEditor(props, ref) {
    const host = useRef<HTMLDivElement>(null);
    const viewRef = useRef<EditorView | null>(null);
    const language = useRef(new Compartment());
    const extra = useRef(new Compartment());
    const changeTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
    const flushChange = useRef<() => void>(() => {});
    const latest = useRef(props);
    useEffect(() => {
      latest.current = props;
    });

    // Create once per tab mount.
    useEffect(() => {
      if (!host.current) return;
      const { initialText, initialSelection } = latest.current;
      const clamp = (n: number) => Math.min(Math.max(n, 0), initialText.length);
      const source = createCompletionSource(() => latest.current.target);
      const view = new EditorView({
        parent: host.current,
        state: EditorState.create({
          doc: initialText,
          selection: { anchor: clamp(initialSelection.from), head: clamp(initialSelection.to) },
          extensions: [
            // Editor shortcuts first so they win over the default keymap (Mod-Enter inserts a line).
            keymap.of(
              EDITOR_SHORTCUTS.map((id) => ({
                key: keysOf(id),
                preventDefault: true,
                run: () => {
                  latest.current.onShortcut(id);
                  return true;
                },
              })),
            ),
            lineNumbers(),
            highlightActiveLineGutter(),
            highlightActiveLine(),
            drawSelection(),
            history(),
            indentOnInput(),
            bracketMatching(),
            closeBrackets(),
            highlightSelectionMatches(),
            autocompletion({ override: [source], activateOnTyping: true }),
            keymap.of([
              ...closeBracketsKeymap,
              ...defaultKeymap,
              ...searchKeymap,
              ...historyKeymap,
              ...completionKeymap,
              ...lintKeymap,
              indentWithTab,
            ]),
            language.current.of(languageExtension(latest.current.mode, latest.current.dialect)),
            extra.current.of(latest.current.extraExtensions ?? []),
            editorHighlight,
            editorTheme,
            EditorView.contentAttributes.of({ 'aria-label': latest.current.ariaLabel }),
            EditorView.updateListener.of((u) => {
              if (u.docChanged) {
                if (changeTimer.current) clearTimeout(changeTimer.current);
                changeTimer.current = setTimeout(() => flushChange.current(), CHANGE_DEBOUNCE_MS);
              }
              if (u.docChanged || u.selectionSet) {
                const r = u.state.selection.main;
                latest.current.onSelection({ from: r.from, to: r.to });
              }
            }),
          ],
        }),
      });
      viewRef.current = view;
      const restoreFrame = requestAnimationFrame(() => {
        view.scrollDOM.scrollTop = latest.current.initialScroll?.top ?? 0;
        view.scrollDOM.scrollLeft = latest.current.initialScroll?.left ?? 0;
      });
      const saveScroll = () =>
        latest.current.onScroll?.({
          top: view.scrollDOM.scrollTop,
          left: view.scrollDOM.scrollLeft,
        });
      view.scrollDOM.addEventListener('scroll', saveScroll);
      flushChange.current = () => {
        if (changeTimer.current === null) return;
        clearTimeout(changeTimer.current);
        changeTimer.current = null;
        latest.current.onChange(view.state.doc.toString());
      };
      return () => {
        cancelAnimationFrame(restoreFrame);
        saveScroll();
        view.scrollDOM.removeEventListener('scroll', saveScroll);
        flushChange.current(); // a tab switch right after typing must not lose text
        view.destroy();
        viewRef.current = null;
      };
    }, []);

    // Highlighting follows the dialect.
    useEffect(() => {
      viewRef.current?.dispatch({
        effects: language.current.reconfigure(languageExtension(props.mode, props.dialect)),
      });
    }, [props.mode, props.dialect]);

    // Extensions of other features (debugger).
    useEffect(() => {
      viewRef.current?.dispatch({
        effects: extra.current.reconfigure(props.extraExtensions ?? []),
      });
    }, [props.extraExtensions]);

    // Error marks become lint diagnostics (underline, gutter marker, tooltip).
    useEffect(() => {
      const view = viewRef.current;
      if (!view) return;
      const len = view.state.doc.length;
      const diagnostics: Diagnostic[] = props.marks
        .filter((m) => m.from <= len)
        .map((m) => ({
          from: m.from,
          to: Math.min(Math.max(m.to, m.from), len),
          severity: 'error',
          message: m.message,
        }));
      view.dispatch(setDiagnostics(view.state, diagnostics));
    }, [props.marks]);

    useImperativeHandle(
      ref,
      () => ({
        focus: () => viewRef.current?.focus(),
        flush: () => flushChange.current(),
        replaceDoc: (text) => {
          const view = viewRef.current;
          if (!view) return;
          const at = Math.min(view.state.selection.main.head, text.length);
          view.dispatch({
            changes: { from: 0, to: view.state.doc.length, insert: text },
            selection: { anchor: at },
          });
        },
        replaceSelectionOrDoc: (map) => {
          const view = viewRef.current;
          if (!view) return;
          const { from, to } = view.state.selection.main;
          const [a, b] = from === to ? [0, view.state.doc.length] : [from, to];
          const next = map(view.state.sliceDoc(a, b));
          view.dispatch({ changes: { from: a, to: b, insert: next } });
        },
        insertText: (text, asSnippet) => {
          const view = viewRef.current;
          if (!view) return;
          const { from, to } = view.state.selection.main;
          if (asSnippet) snippet(text)(view, null, from, to);
          else
            view.dispatch({
              changes: { from, to, insert: text },
              selection: { anchor: from + text.length },
            });
          view.focus();
        },
        reveal: (from, to = from) => {
          const view = viewRef.current;
          if (!view) return;
          const len = view.state.doc.length;
          view.dispatch({
            selection: { anchor: Math.min(from, len), head: Math.min(to, len) },
            effects: EditorView.scrollIntoView(Math.min(from, len), { y: 'center' }),
          });
          view.focus();
        },
      }),
      [],
    );

    return <div ref={host} className="h-full min-h-0 overflow-hidden" data-testid="sql-editor" />;
  },
);

export default SqlEditor;
