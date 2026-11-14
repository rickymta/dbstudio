// Read-only SQL viewer on CodeMirror 6. Loaded lazily (see DdlViewer) to keep the initial
// bundle small; the query editor of a later phase will reuse the same packages.

import { MySQL, PostgreSQL, StandardSQL, sql, type SQLDialect } from '@codemirror/lang-sql';
import { HighlightStyle, syntaxHighlighting } from '@codemirror/language';
import { EditorState } from '@codemirror/state';
import { EditorView, lineNumbers } from '@codemirror/view';
import { tags as t } from '@lezer/highlight';
import { useEffect, useRef } from 'react';
import type { DbKind } from '@/lib/types';

function sqlDialectFor(kind: DbKind | undefined): SQLDialect {
  switch (kind) {
    case 'postgres':
      return PostgreSQL;
    case 'mysql':
      return MySQL;
    default:
      return StandardSQL;
  }
}

// Colors come from the design tokens, so light/dark follow the app theme automatically.
const sqlTheme = EditorView.theme({
  '&': {
    height: '100%',
    color: 'var(--foreground)',
    backgroundColor: 'var(--surface)',
    fontSize: 'var(--editor-font-size, 14px)',
  },
  '&.cm-focused': { outline: 'none' },
  '.cm-scroller': {
    fontFamily: 'var(--data-font-family)',
    fontVariantLigatures: 'var(--code-ligatures)',
    lineHeight: '1.55',
  },
  '.cm-content': { caretColor: 'var(--foreground)', padding: '10px 0' },
  '.cm-gutters': {
    backgroundColor: 'var(--surface-muted)',
    color: 'var(--muted-foreground)',
    border: 'none',
    borderRight: '1px solid var(--border)',
  },
  '.cm-activeLine': { backgroundColor: 'transparent' },
  '.cm-selectionBackground, &.cm-focused .cm-selectionBackground, ::selection': {
    backgroundColor: 'var(--selection) !important',
  },
});

const sqlHighlight = HighlightStyle.define([
  {
    tag: [t.keyword, t.operatorKeyword, t.modifier],
    color: 'var(--syntax-keyword)',
    fontWeight: '600',
  },
  { tag: [t.string, t.special(t.string)], color: 'var(--syntax-string)' },
  { tag: [t.number, t.bool, t.null], color: 'var(--syntax-number)' },
  { tag: [t.lineComment, t.blockComment], color: 'var(--muted-foreground)', fontStyle: 'italic' },
  { tag: [t.typeName, t.standard(t.name)], color: 'var(--syntax-type)' },
  { tag: [t.punctuation, t.operator], color: 'var(--muted-foreground)' },
]);

interface CodeViewProps {
  value: string;
  kind?: DbKind;
  label?: string;
}

export default function CodeView({ value, kind, label }: CodeViewProps) {
  const host = useRef<HTMLDivElement>(null);
  const view = useRef<EditorView | null>(null);

  // Create once per dialect; the document is synced below.
  useEffect(() => {
    if (!host.current) return;
    const v = new EditorView({
      parent: host.current,
      state: EditorState.create({
        doc: value,
        extensions: [
          lineNumbers(),
          EditorState.readOnly.of(true),
          EditorView.editable.of(false),
          sql({ dialect: sqlDialectFor(kind) }),
          syntaxHighlighting(sqlHighlight),
          sqlTheme,
          EditorView.contentAttributes.of({
            'aria-label': label ?? 'SQL',
            'aria-readonly': 'true',
          }),
        ],
      }),
    });
    view.current = v;
    return () => {
      v.destroy();
      view.current = null;
    };
    // `value` is applied by the effect below; recreate only when the dialect changes.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [kind, label]);

  useEffect(() => {
    const v = view.current;
    if (v && v.state.doc.toString() !== value) {
      v.dispatch({ changes: { from: 0, to: v.state.doc.length, insert: value } });
    }
  }, [value, kind]);

  return <div ref={host} className="h-full min-h-0 overflow-hidden" data-testid="code-view" />;
}
