// Read-only view of the raw plan text on CodeMirror 6, with the selected node's lines marked.
// Loaded lazily by PlanTab to keep the initial bundle small.

import { json } from '@codemirror/lang-json';
import { xml } from '@codemirror/lang-xml';
import { HighlightStyle, syntaxHighlighting } from '@codemirror/language';
import { EditorState, StateEffect, StateField, type Extension } from '@codemirror/state';
import { Decoration, EditorView, lineNumbers } from '@codemirror/view';
import { tags as t } from '@lezer/highlight';
import { useEffect, useRef } from 'react';
import type { RawPlan } from '@/lib/types';
import { FORMAT_LANGUAGE } from './planModel';

const setRange = StateEffect.define<[number, number] | null>();

const lineMark = Decoration.line({ class: 'cm-plan-selected' });

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
  '.cm-gutters': {
    backgroundColor: 'var(--surface-muted)',
    color: 'var(--muted-foreground)',
    border: 'none',
    borderRight: '1px solid var(--border)',
  },
  '.cm-activeLine': { backgroundColor: 'transparent' },
  '.cm-plan-selected': { backgroundColor: 'var(--warning-soft)' },
});

const highlight = HighlightStyle.define([
  { tag: [t.propertyName, t.tagName], color: 'var(--accent)' },
  { tag: [t.attributeName], color: 'var(--warning)' },
  { tag: [t.attributeValue], color: 'var(--success)' },
  { tag: [t.string, t.special(t.string)], color: 'var(--success)' },
  { tag: [t.number, t.bool, t.null], color: 'var(--warning)' },
  { tag: [t.punctuation, t.operator], color: 'var(--muted-foreground)' },
]);

/** Line decorations for the inclusive 0-based line range given by `setRange`. */
const markers = StateField.define({
  create: () => Decoration.none,
  update(deco, tr) {
    for (const e of tr.effects) {
      if (!e.is(setRange)) continue;
      const range = e.value;
      if (!range) return Decoration.none;
      const doc = tr.state.doc;
      const from = Math.min(range[0] + 1, doc.lines);
      const to = Math.min(range[1] + 1, doc.lines);
      const marks = [];
      for (let n = from; n <= to; n++) marks.push(lineMark.range(doc.line(n).from));
      return Decoration.set(marks);
    }
    return deco.map(tr.changes);
  },
  provide: (f) => EditorView.decorations.from(f),
});

interface PlanRawProps {
  raw: RawPlan;
  /** Inclusive 0-based line range of the selected node. */
  range: [number, number] | null;
}

export default function PlanRaw({ raw, range }: PlanRawProps) {
  const host = useRef<HTMLDivElement>(null);
  const view = useRef<EditorView | null>(null);
  const language = FORMAT_LANGUAGE[raw.format];

  useEffect(() => {
    if (!host.current) return;
    const extensions: Extension[] = [
      lineNumbers(),
      EditorState.readOnly.of(true),
      EditorView.editable.of(false),
      syntaxHighlighting(highlight),
      theme,
      markers,
    ];
    if (language === 'json') extensions.push(json());
    if (language === 'xml') extensions.push(xml());
    const v = new EditorView({
      parent: host.current,
      state: EditorState.create({ doc: raw.text, extensions }),
    });
    view.current = v;
    return () => {
      v.destroy();
      view.current = null;
    };
  }, [raw.text, language]);

  // Mark and reveal the selected node.
  useEffect(() => {
    const v = view.current;
    if (!v) return;
    const effects: StateEffect<unknown>[] = [setRange.of(range)];
    if (range && range[0] < v.state.doc.lines) {
      effects.push(EditorView.scrollIntoView(v.state.doc.line(range[0] + 1).from, { y: 'center' }));
    }
    v.dispatch({ effects });
  }, [range, raw.text, language]);

  return (
    <div
      ref={host}
      className="h-full min-h-0 overflow-hidden"
      data-testid="plan-raw"
      data-selected-lines={range ? `${range[0]}-${range[1]}` : ''}
    />
  );
}
