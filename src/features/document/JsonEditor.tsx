// CodeMirror 6 JSON editor/viewer. Loaded lazily by its users to keep the initial bundle small.

import { json } from '@codemirror/lang-json';
import { defaultKeymap, history, historyKeymap } from '@codemirror/commands';
import { HighlightStyle, syntaxHighlighting } from '@codemirror/language';
import { EditorState } from '@codemirror/state';
import { EditorView, keymap, lineNumbers } from '@codemirror/view';
import { tags as t } from '@lezer/highlight';
import { useEffect, useRef } from 'react';

// Colors come from the design tokens, so light/dark follow the app theme automatically.
const theme = EditorView.theme({
  '&': {
    height: '100%',
    color: 'var(--foreground)',
    backgroundColor: 'var(--surface)',
    fontSize: '12.5px',
  },
  '&.cm-focused': { outline: 'none' },
  '.cm-scroller': {
    fontFamily: 'ui-monospace, SFMono-Regular, Menlo, Consolas, monospace',
    lineHeight: '1.55',
  },
  '.cm-content': { caretColor: 'var(--foreground)', padding: '6px 0' },
  '.cm-gutters': {
    backgroundColor: 'var(--surface-muted)',
    color: 'var(--muted-foreground)',
    border: 'none',
    borderRight: '1px solid var(--border)',
  },
  '.cm-activeLine': { backgroundColor: 'transparent' },
  '.cm-selectionBackground, &.cm-focused .cm-selectionBackground, ::selection': {
    backgroundColor: 'var(--ring) !important',
  },
});

const highlight = HighlightStyle.define([
  { tag: [t.propertyName], color: 'var(--accent)' },
  { tag: [t.string, t.special(t.string)], color: 'var(--success)' },
  { tag: [t.number, t.bool, t.null], color: 'var(--warning)' },
  { tag: [t.punctuation, t.operator, t.brace, t.squareBracket], color: 'var(--muted-foreground)' },
]);

interface JsonEditorProps {
  value: string;
  label: string;
  /** Called with the new text on every edit (editable mode). */
  onChange?: (text: string) => void;
  /** Ctrl/Cmd+Enter. */
  onSubmit?: () => void;
  readOnly?: boolean;
}

export default function JsonEditor({
  value,
  label,
  onChange,
  onSubmit,
  readOnly,
}: JsonEditorProps) {
  const host = useRef<HTMLDivElement>(null);
  const view = useRef<EditorView | null>(null);
  const onChangeRef = useRef(onChange);
  const onSubmitRef = useRef(onSubmit);
  useEffect(() => {
    onChangeRef.current = onChange;
    onSubmitRef.current = onSubmit;
  });

  useEffect(() => {
    if (!host.current) return;
    const v = new EditorView({
      parent: host.current,
      state: EditorState.create({
        doc: value,
        extensions: [
          lineNumbers(),
          json(),
          syntaxHighlighting(highlight),
          theme,
          EditorState.readOnly.of(!!readOnly),
          EditorView.editable.of(!readOnly),
          EditorView.contentAttributes.of({
            'aria-label': label,
            ...(readOnly ? { 'aria-readonly': 'true' } : {}),
          }),
          ...(readOnly
            ? []
            : [
                history(),
                keymap.of([
                  {
                    key: 'Mod-Enter',
                    run: () => {
                      onSubmitRef.current?.();
                      return true;
                    },
                  },
                  ...defaultKeymap,
                  ...historyKeymap,
                ]),
                EditorView.updateListener.of((u) => {
                  if (u.docChanged) onChangeRef.current?.(u.state.doc.toString());
                }),
              ]),
        ],
      }),
    });
    view.current = v;
    return () => {
      v.destroy();
      view.current = null;
    };
    // `value` is applied by the effect below; recreate only when the mode or label changes.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [readOnly, label]);

  useEffect(() => {
    const v = view.current;
    if (v && v.state.doc.toString() !== value) {
      v.dispatch({ changes: { from: 0, to: v.state.doc.length, insert: value } });
    }
  }, [value]);

  return <div ref={host} className="h-full min-h-0 overflow-hidden" data-testid="json-editor" />;
}
