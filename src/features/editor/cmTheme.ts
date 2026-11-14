// CodeMirror theme of the editor: colors come from the design tokens, so light and dark follow
// the app theme without extra work.

import { HighlightStyle, syntaxHighlighting } from '@codemirror/language';
import { EditorView } from '@codemirror/view';
import { tags as t } from '@lezer/highlight';

export const editorTheme = EditorView.theme({
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
  '.cm-activeLine': {
    backgroundColor: 'color-mix(in srgb, var(--surface-muted) 60%, transparent)',
  },
  '.cm-activeLineGutter': { backgroundColor: 'var(--surface-muted)' },
  '.cm-selectionBackground, &.cm-focused .cm-selectionBackground, ::selection': {
    backgroundColor: 'var(--selection) !important',
  },
  '.cm-tooltip': {
    backgroundColor: 'var(--surface)',
    color: 'var(--foreground)',
    border: '1px solid var(--border)',
    borderRadius: '6px',
  },
  '.cm-tooltip-autocomplete ul li[aria-selected]': {
    backgroundColor: 'var(--accent)',
    color: 'var(--accent-foreground)',
  },
  '.cm-completionDetail': { color: 'var(--muted-foreground)', marginLeft: '0.8em' },
  '.cm-diagnostic-error': { borderLeft: '3px solid var(--danger)' },
  '.cm-lintRange-error': {
    backgroundImage: 'none',
    textDecoration: 'underline wavy var(--danger)',
  },
});

export const editorHighlight = syntaxHighlighting(
  HighlightStyle.define([
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
  ]),
);
