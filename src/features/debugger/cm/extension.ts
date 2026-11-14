// CodeMirror extension of the debugger for one source (editor script or routine): breakpoint
// gutter, execution line, inline values, hover, read-only while debugging and the key handler.
// State is derived from `debugStore`; the extension only draws and reports clicks.

import { EditorState, Prec, type Extension } from '@codemirror/state';
import { EditorView, ViewPlugin, hoverTooltip, type Tooltip } from '@codemirror/view';
import i18n from '@/lib/i18n';
import type { SourceRef } from '@/lib/types';
import {
  removeBreakpoint,
  runCommand,
  syncBreakpointLines,
  toggleBreakpoint,
} from '../debugActions';
import {
  isLive,
  tabOfRoutine,
  useDebugStore,
  type DebugSession,
  type ScopeInfo,
} from '../debugStore';
import { useDebugUi } from '../debugUiStore';
import { commandOfEvent } from '../keymap';
import { evalTypeName, evalValueText } from '../valueText';
import {
  BpMarker,
  debugGutters,
  readOnlyCompartment,
  setBreakpointMarkers,
  setExecLines,
  setInlineValues,
  breakpointField,
  type BpKind,
  type ExecKind,
} from './gutter';
import { registerView } from './viewRegistry';
import { debuggerTheme } from './theme';
import { valueToText } from '@/features/data/format';

export interface SourceOptions {
  scopeKey: string;
  /** File line of document line 1 (routine sources may start later). Default 1. */
  lineBase?: number;
  /** The editor tab owning the session this source is debugged in, if any. */
  tabIdOf: () => string | null;
  /** Registers the scope in the store (called when a view mounts, never during render). */
  ensure?: () => void;
}

const t = (key: string, options?: Record<string, unknown>): string =>
  i18n.t(key, { ns: 'debugger', ...options });

export const sameSource = (a: SourceRef, b: SourceRef): boolean =>
  a.kind === b.kind &&
  (a.kind === 'script' || (b.kind === 'routine' && a.routine.signature === b.routine.signature));

/** The live debug session this scope is shown in. */
export function sessionOfScope(scope: ScopeInfo | undefined): [string, DebugSession] | null {
  if (!scope) return null;
  const store = useDebugStore.getState();
  const tabId =
    scope.tabId ?? (scope.routine ? tabOfRoutine(scope.routine.signature) : null) ?? null;
  const session = tabId ? store.sessions[tabId] : undefined;
  return tabId && session ? [tabId, session] : null;
}

export interface Derived {
  breakpoints: Array<{ id: string; line: number; kind: BpKind }>;
  exec: Array<{ line: number; kind: ExecKind }>;
  inline: Array<{ line: number; text: string; kind: string }>;
  readOnly: boolean;
}

/** What the editor of a scope has to show for the current store (file lines, 1-based). */
export function derive(scopeKey: string): Derived {
  const store = useDebugStore.getState();
  const scope = store.scopes[scopeKey];
  const out: Derived = { breakpoints: [], exec: [], inline: [], readOnly: false };
  if (!scope) return out;
  const found = sessionOfScope(scope);
  const live = found && isLive(found[1]) ? found[1] : null;
  const statuses = live?.state.breakpoints ?? [];
  for (const b of scope.breakpoints) {
    const status = statuses.find((s) => s.id === b.id);
    let kind: BpKind = 'on';
    if (!b.enabled) kind = 'off';
    else if (status && !status.verified) kind = 'unverified';
    else if (!b.suspend) kind = 'log';
    else if (b.condition?.trim()) kind = 'cond';
    out.breakpoints.push({ id: b.id, line: status?.verified ? status.line : b.line, kind });
  }
  if (!live) return out;
  const { state } = live;
  out.readOnly = !!scope.tabId;
  const status = state.status;
  const stoppedOnError =
    status.kind === 'paused' &&
    (status.reason.kind === 'error' || status.reason.kind === 'blocked');
  if (status.kind === 'paused') {
    state.frames.forEach((frame, index) => {
      if (!sameSource(frame.source, scope.source)) return;
      const selected = index === state.selectedFrame;
      out.exec.push({
        line: frame.line,
        kind: selected ? (stoppedOnError && index === 0 ? 'error' : 'current') : 'call',
      });
    });
  }
  if (store.inlineValues) {
    for (const v of state.inlineValues) {
      if (sameSource(v.source, scope.source)) {
        out.inline.push({ line: v.line, text: v.text, kind: v.kind });
      }
    }
  }
  return out;
}

const keyOf = (d: Derived): string => JSON.stringify(d);

function bpLabel(line: number, kind: BpKind): string {
  const base = t('debugger:gutter.breakpoint', { line });
  return kind === 'on' ? base : `${base} (${t(`debugger:gutter.kind.${kind}`)})`;
}

/** Offsets of document lines 1.. clamped; `line` is a file line. */
function docLineStart(state: EditorState, line: number, base: number): number | null {
  const n = line - base + 1;
  if (n < 1 || n > state.doc.lines) return null;
  return state.doc.line(n).from;
}

function hoverLookup(scopeKey: string, word: string): { text: string; type?: string } | null {
  const store = useDebugStore.getState();
  const scope = store.scopes[scopeKey];
  const found = sessionOfScope(scope);
  if (!found || !isLive(found[1])) return null;
  const [tabId, session] = found;
  const norm = (n: string) => n.replace(/^[@:$]/, '').toLowerCase();
  const target = word.toLowerCase();
  for (const children of Object.values(store.children[tabId] ?? {})) {
    const v = children.find((c) => norm(c.name) === target);
    if (v)
      return { text: v.value ? valueToText(v.value) : v.display, type: v.typeName ?? undefined };
  }
  for (const group of session.state.variables) {
    if (norm(group.name) === target)
      return {
        text: group.value ? valueToText(group.value) : group.display,
        type: group.typeName ?? undefined,
      };
  }
  const watch = session.state.watches.find((w) => norm(w.expr) === target);
  if (watch)
    return {
      text: evalValueText(watch.result.value),
      type: evalTypeName(watch.result) ?? undefined,
    };
  return null;
}

/**
 * Everything the debugger adds to an editor of one source. Keep the returned array stable
 * between renders (the editor reconfigures when the array identity changes).
 */
export function createSourceExtension(options: SourceOptions): Extension[] {
  const { scopeKey, tabIdOf } = options;
  const base = options.lineBase ?? 1;

  const plugin = ViewPlugin.define((view) => {
    options.ensure?.();
    const unregister = registerView(scopeKey, view);
    let lastKey = '';
    let scheduled = false;
    let syncTimer: ReturnType<typeof setTimeout> | null = null;

    const apply = () => {
      scheduled = false;
      const d = derive(scopeKey);
      const key = keyOf(d);
      if (key === lastKey) return;
      lastKey = key;
      const state = view.state;
      const effects = [
        setBreakpointMarkers.of(
          d.breakpoints.flatMap((b) => {
            const pos = docLineStart(state, b.line, base);
            return pos === null
              ? []
              : [{ pos, marker: new BpMarker(b.kind, b.id, bpLabel(b.line, b.kind)) }];
          }),
        ),
        setExecLines.of(
          d.exec.flatMap((x) => {
            const pos = docLineStart(state, x.line, base);
            return pos === null ? [] : [{ pos, kind: x.kind }];
          }),
        ),
        setInlineValues.of(
          d.inline.flatMap((x) => {
            const n = x.line - base + 1;
            if (n < 1 || n > state.doc.lines) return [];
            return [{ pos: state.doc.line(n).to, text: x.text, kind: x.kind }];
          }),
        ),
        readOnlyCompartment.reconfigure(EditorState.readOnly.of(d.readOnly)),
      ];
      view.dispatch({ effects });
    };
    const schedule = () => {
      if (scheduled) return;
      scheduled = true;
      queueMicrotask(apply);
    };
    const unsubscribe = useDebugStore.subscribe(schedule);
    schedule();

    return {
      update(update) {
        if (!update.docChanged) return;
        // Breakpoints moved with the text: report the new lines (debounced).
        if (syncTimer) clearTimeout(syncTimer);
        syncTimer = setTimeout(() => {
          syncTimer = null;
          const lines: Array<{ id: string; line: number }> = [];
          const cursor = view.state.field(breakpointField).iter();
          while (cursor.value) {
            lines.push({
              id: cursor.value.id,
              line: view.state.doc.lineAt(cursor.from).number + base - 1,
            });
            cursor.next();
          }
          // The doc may be shorter than a stale marker list; only report what is mounted.
          syncBreakpointLines(scopeKey, lines);
          lastKey = '';
          schedule();
        }, 120);
      },
      destroy() {
        if (syncTimer) clearTimeout(syncTimer);
        unsubscribe();
        unregister();
      },
    };
  });

  const fileLine = (docLine: number) => docLine + base - 1;

  const onToggle = (line: number, altKey: boolean) => {
    toggleBreakpoint(scopeKey, fileLine(line), altKey);
  };
  const onContext = (line: number, event: MouseEvent) => {
    const scope = useDebugStore.getState().scopes[scopeKey];
    let bp = scope?.breakpoints.find((b) => b.line === fileLine(line));
    if (!bp) {
      toggleBreakpoint(scopeKey, fileLine(line));
      bp = useDebugStore
        .getState()
        .scopes[scopeKey]?.breakpoints.find((b) => b.line === fileLine(line));
    }
    if (bp) {
      useDebugUi.getState().openPopover({
        scopeKey,
        breakpointId: bp.id,
        x: event.clientX,
        y: event.clientY,
      });
    }
  };

  const keys = EditorView.domEventHandlers({
    keydown(event, view) {
      const tabId = tabIdOf();
      const store = useDebugStore.getState();
      const hasSession = !!tabId && isLive(store.sessions[tabId]);
      const command = commandOfEvent(event, store.keymap, hasSession);
      if (!command) return false;
      event.preventDefault();
      if (tabId) runCommand(tabId, command, { scopeKey });
      else if (command === 'toggleBreakpoint') {
        toggleBreakpoint(
          scopeKey,
          fileLine(view.state.doc.lineAt(view.state.selection.main.head).number),
        );
      }
      return true;
    },
  });

  const hover = hoverTooltip((view, pos): Tooltip | null => {
    const word = view.state.wordAt(pos);
    if (!word) return null;
    const text = view.state.sliceDoc(word.from, word.to);
    const found = hoverLookup(scopeKey, text);
    if (!found) return null;
    return {
      pos: word.from,
      end: word.to,
      above: true,
      create() {
        const dom = document.createElement('div');
        dom.className = 'cm-debug-hover';
        dom.textContent = found.text;
        if (found.type) {
          const type = document.createElement('span');
          type.className = 'cm-debug-hover-type';
          type.textContent = `{${found.type}}`;
          dom.append(type);
        }
        return { dom };
      },
    };
  });

  return [
    debuggerTheme,
    debugGutters({ onToggle, onContext }),
    readOnlyCompartment.of(EditorState.readOnly.of(false)),
    plugin,
    hover,
    Prec.highest(keys),
  ];
}

export { removeBreakpoint };
