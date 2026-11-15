// Pure helpers of the cell editor: which editor a column gets, how typed text becomes an
// `EditValue`, and how a pending edit is shown. NULL and the empty string are different values:
// NULL is `{ kind: 'value', v: { t: 'null' } }`, the empty string is `{ kind: 'text', v: '' }`.

import type { ColumnMeta, EditValue, Value, ValueKind } from '@/lib/types';

export type EditorKind = 'bool' | 'number' | 'json' | 'temporal' | 'text' | 'binary';

export const NULL_EDIT: EditValue = { kind: 'value', v: { t: 'null' } };

/** A LOB the server cut short: writing it back (edit, duplicate) would save the cut text. */
export function isTruncatedValue(v: Value): boolean {
  return v.t === 'ext' && (v.v.type === 'truncatedText' || v.v.type === 'truncatedBytes');
}

export function isNullEdit(e: EditValue): boolean {
  return e.kind === 'value' && e.v.t === 'null';
}

export function editorKind(kind: ValueKind): EditorKind {
  switch (kind) {
    case 'bool':
      return 'bool';
    case 'int':
    case 'uInt':
    case 'float':
    case 'decimal':
      return 'number';
    case 'json':
    case 'array':
    case 'document':
      return 'json';
    case 'date':
    case 'time':
    case 'dateTime':
    case 'timestamp':
      return 'temporal';
    case 'bytes':
      return 'binary';
    default:
      return 'text';
  }
}

export type ParseError = 'invalidNumber' | 'invalidJson' | 'invalidBool';

export type ParseResult = { ok: true; edit: EditValue } | { ok: false; error: ParseError };

const INT_RE = /^[+-]?\d+$/;
const UINT_RE = /^\+?\d+$/;
const DECIMAL_RE = /^[+-]?(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?$/;
const SPECIAL_FLOAT_RE = /^[+-]?(nan|inf|infinity)$/i;

/**
 * Turn what the user typed into an `EditValue`. Numbers stay text (never converted to a JS
 * number, so 64-bit integers and decimals keep every digit); the database coerces the literal.
 * `asNull` wins over the text.
 */
export function parseInput(kind: ValueKind, text: string, asNull: boolean): ParseResult {
  if (asNull) return { ok: true, edit: NULL_EDIT };
  switch (kind) {
    case 'bool': {
      const v = text.trim().toLowerCase();
      if (v === 'true' || v === 't' || v === '1')
        return { ok: true, edit: { kind: 'value', v: { t: 'bool', v: true } } };
      if (v === 'false' || v === 'f' || v === '0')
        return { ok: true, edit: { kind: 'value', v: { t: 'bool', v: false } } };
      return { ok: false, error: 'invalidBool' };
    }
    case 'int': {
      const v = text.trim();
      return INT_RE.test(v)
        ? { ok: true, edit: { kind: 'text', v } }
        : { ok: false, error: 'invalidNumber' };
    }
    case 'uInt': {
      const v = text.trim();
      return UINT_RE.test(v)
        ? { ok: true, edit: { kind: 'text', v } }
        : { ok: false, error: 'invalidNumber' };
    }
    case 'float':
    case 'decimal': {
      const v = text.trim();
      return DECIMAL_RE.test(v) || (kind === 'float' && SPECIAL_FLOAT_RE.test(v))
        ? { ok: true, edit: { kind: 'text', v } }
        : { ok: false, error: 'invalidNumber' };
    }
    case 'array': {
      // A typed array: the server renders it in the database's own array syntax.
      let parsed: unknown;
      try {
        parsed = JSON.parse(text);
      } catch {
        return { ok: false, error: 'invalidJson' };
      }
      const v = Array.isArray(parsed) ? jsonToArray(parsed) : null;
      return v ? { ok: true, edit: { kind: 'value', v } } : { ok: false, error: 'invalidJson' };
    }
    case 'json':
    case 'document':
      try {
        JSON.parse(text);
      } catch {
        return { ok: false, error: 'invalidJson' };
      }
      return { ok: true, edit: { kind: 'text', v: text } };
    default:
      return { ok: true, edit: { kind: 'text', v: text } };
  }
}

/** JSON array of scalars (nested arrays allowed) as an array `Value`; `null` for objects. */
function jsonToArray(items: unknown[]): Value | null {
  const out: Value[] = [];
  for (const x of items) {
    if (x === null) out.push({ t: 'null' });
    else if (typeof x === 'boolean') out.push({ t: 'bool', v: x });
    else if (typeof x === 'number') out.push({ t: 'decimal', v: String(x) });
    else if (typeof x === 'string') out.push({ t: 'text', v: x });
    else if (Array.isArray(x)) {
      const inner = jsonToArray(x);
      if (!inner) return null;
      out.push(inner);
    } else return null;
  }
  return { t: 'array', v: out };
}

/** Raw text of a value for the editor and for comparisons; NULL has no text. */
export function rawText(v: Value): string | null {
  switch (v.t) {
    case 'null':
      return null;
    case 'bool':
      return v.v ? 'true' : 'false';
    case 'int':
    case 'uInt':
    case 'float':
      return String(v.v);
    case 'ext':
      return v.v.text;
    case 'array':
    case 'document':
      return JSON.stringify(plainOf(v));
    default:
      return String(v.v);
  }
}

function plainOf(v: Value): unknown {
  switch (v.t) {
    case 'array':
      return v.v.map(plainOf);
    case 'document':
      return Object.fromEntries(v.v.map(([k, x]) => [k, plainOf(x)]));
    case 'null':
      return null;
    case 'bool':
    case 'text':
    case 'decimal':
      return v.v;
    case 'int':
    case 'uInt':
    case 'float':
      return typeof v.v === 'number' ? v.v : String(v.v);
    case 'ext':
      return v.v.text;
    default:
      return v.v;
  }
}

/** Does `edit` write the value the cell already has? Then it is not a change. */
export function editEqualsOriginal(original: Value, edit: EditValue): boolean {
  if (edit.kind === 'default') return false;
  if (edit.kind === 'value') {
    if (edit.v.t === 'null' || original.t === 'null') return edit.v.t === original.t;
    return JSON.stringify(edit.v) === JSON.stringify(original);
  }
  const text = rawText(original);
  return text !== null && text === edit.v;
}

/** How a pending edit is rendered in the grid. `null` for DEFAULT (rendered by the grid). */
export function editDisplayValue(edit: EditValue, kind: ValueKind): Value | null {
  if (edit.kind === 'default') return null;
  if (edit.kind === 'value') return edit.v;
  const v = edit.v;
  switch (kind) {
    case 'int':
    case 'uInt':
    case 'float':
      return { t: kind, v };
    case 'decimal':
    case 'date':
    case 'time':
    case 'dateTime':
    case 'timestamp':
    case 'uuid':
    case 'json':
      return { t: kind, v };
    default:
      return { t: 'text', v };
  }
}

/** Initial text of the editor for a cell. */
export function editorInitialText(current: Value | EditValue | null): string {
  if (!current) return '';
  if ('kind' in current) {
    if (current.kind === 'default') return '';
    if (current.kind === 'text') return current.v;
    return rawText(current.v) ?? '';
  }
  return rawText(current) ?? '';
}

/** True when a cell needs the dialog editor (JSON, binary, long or multi-line text). */
export function needsDialogEditor(column: ColumnMeta, current: Value | EditValue | null): boolean {
  const kind = editorKind(column.valueKind);
  if (kind === 'json' || kind === 'binary') return true;
  if (kind !== 'text') return false;
  const text = editorInitialText(current);
  return text.includes('\n') || text.length > 120;
}
