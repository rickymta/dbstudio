// CodeMirror state of the debugger: breakpoint markers (mapped through edits), the execution
// line (background + arrow) and the inline values at the end of lines.

import {
  Compartment,
  RangeSet,
  StateEffect,
  StateField,
  type Extension,
  type Range,
} from '@codemirror/state';
import { Decoration, EditorView, GutterMarker, WidgetType, gutter } from '@codemirror/view';

export type BpKind = 'on' | 'cond' | 'off' | 'log' | 'unverified';

export class BpMarker extends GutterMarker {
  constructor(
    readonly kind: BpKind,
    readonly id: string,
    readonly label: string,
  ) {
    super();
  }
  eq(other: GutterMarker): boolean {
    return (
      other instanceof BpMarker &&
      other.kind === this.kind &&
      other.id === this.id &&
      other.label === this.label
    );
  }
  toDOM(): Node {
    const el = document.createElement('span');
    // The spacer (empty id) only reserves the gutter width.
    el.className = this.id ? `cm-bp cm-bp-${this.kind}` : 'cm-bp-spacer';
    el.setAttribute('role', 'img');
    el.setAttribute('aria-label', this.label);
    el.dataset.bpId = this.id;
    el.dataset.bpKind = this.kind;
    return el;
  }
}

export interface MarkerAt {
  pos: number;
  marker: BpMarker;
}

export const setBreakpointMarkers = StateEffect.define<MarkerAt[]>();

export const breakpointField = StateField.define<RangeSet<BpMarker>>({
  create: () => RangeSet.empty,
  update(value, tr) {
    for (const e of tr.effects) {
      if (e.is(setBreakpointMarkers)) {
        return RangeSet.of(
          e.value.map((m) => m.marker.range(m.pos)),
          true,
        );
      }
    }
    return tr.docChanged ? value.map(tr.changes) : value;
  },
});

// -- execution line -------------------------------------------------------------------------

export type ExecKind = 'current' | 'call' | 'error';

class ArrowMarker extends GutterMarker {
  constructor(readonly kind: ExecKind) {
    super();
  }
  eq(other: GutterMarker): boolean {
    return other instanceof ArrowMarker && other.kind === this.kind;
  }
  toDOM(): Node {
    const el = document.createElement('span');
    el.className = `cm-exec-arrow cm-exec-arrow-${this.kind}`;
    el.setAttribute('aria-hidden', 'true');
    el.textContent = '➜';
    return el;
  }
}

export interface ExecAt {
  pos: number;
  kind: ExecKind;
}

export const setExecLines = StateEffect.define<ExecAt[]>();

export const execLineField = StateField.define<RangeSet<Decoration>>({
  create: () => Decoration.none,
  update(value, tr) {
    for (const e of tr.effects) {
      if (e.is(setExecLines)) {
        const deco: Range<Decoration>[] = e.value.map((x) =>
          Decoration.line({ class: `cm-exec-line cm-exec-line-${x.kind}` }).range(x.pos),
        );
        return Decoration.set(deco, true);
      }
    }
    return tr.docChanged ? value.map(tr.changes) : value;
  },
  provide: (f) => EditorView.decorations.from(f),
});

const arrowField = StateField.define<RangeSet<ArrowMarker>>({
  create: () => RangeSet.empty,
  update(value, tr) {
    for (const e of tr.effects) {
      if (e.is(setExecLines)) {
        return RangeSet.of(
          e.value.map((x) => new ArrowMarker(x.kind).range(x.pos)),
          true,
        );
      }
    }
    return tr.docChanged ? value.map(tr.changes) : value;
  },
});

// -- inline values --------------------------------------------------------------------------

class InlineWidget extends WidgetType {
  constructor(
    readonly text: string,
    readonly kind: string,
  ) {
    super();
  }
  eq(other: WidgetType): boolean {
    return other instanceof InlineWidget && other.text === this.text && other.kind === this.kind;
  }
  toDOM(): HTMLElement {
    const el = document.createElement('span');
    el.className = `cm-inline-value cm-inline-value-${this.kind}`;
    el.textContent = this.text;
    el.setAttribute('aria-hidden', 'true');
    return el;
  }
  ignoreEvent(): boolean {
    return true;
  }
}

export interface InlineAt {
  pos: number;
  text: string;
  kind: string;
}

export const setInlineValues = StateEffect.define<InlineAt[]>();

export const inlineField = StateField.define<RangeSet<Decoration>>({
  create: () => Decoration.none,
  update(value, tr) {
    for (const e of tr.effects) {
      if (e.is(setInlineValues)) {
        return Decoration.set(
          e.value.map((x) =>
            Decoration.widget({ widget: new InlineWidget(x.text, x.kind), side: 1 }).range(x.pos),
          ),
          true,
        );
      }
    }
    return tr.docChanged ? value.map(tr.changes) : value;
  },
  provide: (f) => EditorView.decorations.from(f),
});

// -- read only while debugging --------------------------------------------------------------

export const readOnlyCompartment = new Compartment();

// -- gutters --------------------------------------------------------------------------------

export interface GutterHandlers {
  /** `line` is a 1-based document line. */
  onToggle: (line: number, altKey: boolean) => void;
  onContext: (line: number, event: MouseEvent) => void;
}

/** Breakpoint gutter (left of the line numbers) and the narrow execution-arrow gutter. */
export function debugGutters(handlers: GutterHandlers): Extension[] {
  const lineOf = (view: EditorView, pos: number): number => view.state.doc.lineAt(pos).number;
  return [
    breakpointField,
    arrowField,
    execLineField,
    inlineField,
    gutter({
      class: 'cm-bp-gutter',
      markers: (view) => view.state.field(breakpointField),
      initialSpacer: () => new BpMarker('on', '', ''),
      domEventHandlers: {
        mousedown(view, block, event) {
          const e = event as MouseEvent;
          if (e.button !== 0) return false;
          e.preventDefault();
          handlers.onToggle(lineOf(view, block.from), e.altKey);
          return true;
        },
        contextmenu(view, block, event) {
          event.preventDefault();
          handlers.onContext(lineOf(view, block.from), event as MouseEvent);
          return true;
        },
      },
    }),
    gutter({
      class: 'cm-exec-gutter',
      markers: (view) => view.state.field(arrowField),
      initialSpacer: () => new ArrowMarker('current'),
    }),
  ];
}
