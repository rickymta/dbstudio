// Pure value formatting for the data grid and the cell viewer. Nothing here touches React.
// Large integers arrive as decimal strings (beyond 2^53) and must never go through `Number`.

import { parse as parseLossless } from 'lossless-json';
import type { ColumnMeta, Value, ValueKind } from '@/lib/types';
import { toLocalTimestamp, type TimestampDisplay } from './timestamp';

export type CellKind =
  'null' | 'bool' | 'number' | 'temporal' | 'text' | 'uuid' | 'binary' | 'structured';

export interface CellDisplay {
  text: string;
  kind: CellKind;
  align: 'left' | 'right';
  /** Native type label of an `ext` value (`objectId`, `int64`...). */
  badge?: string;
}

/** Longest text kept for in-cell display; the cell is clipped with CSS ellipsis anyway. */
export const MAX_CELL_CHARS = 200;

const NEWLINE = '\u21B5'; // ↵
const TAB = '\u21E5'; // ⇥
const BIDI = '\u21C4'; // ⇄
const REPLACEMENT = '\uFFFD';

function clip(text: string, max: number): string {
  return text.length > max ? `${text.slice(0, max)}\u2026` : text;
}

// C0 controls (except those handled first), DEL, C1 controls, and bidi override/isolate marks.
// eslint-disable-next-line no-control-regex
const CONTROL_RE = /[\u0000-\u0008\u000B\u000C\u000E-\u001F\u007F\u0080-\u009F]/g;
const BIDI_RE = /[\u061C\u200E\u200F\u202A-\u202E\u2066-\u2069]/g;

/**
 * Make control and bidi characters visible so a value renders on one line and cannot reorder
 * the text around it. Newlines/tabs get arrows, other C0 controls their "control picture"
 * (U+2400 block), C1 controls a replacement mark, bidi marks an arrows symbol.
 */
export function visibleWhitespace(text: string): string {
  return text
    .replace(/\r\n|\r|\n/g, NEWLINE)
    .replace(/\t/g, TAB)
    .replace(CONTROL_RE, (c) => {
      const code = c.charCodeAt(0);
      if (code < 0x20) return String.fromCharCode(0x2400 + code);
      if (code === 0x7f) return '\u2421';
      return REPLACEMENT;
    })
    .replace(BIDI_RE, BIDI);
}

// ---------------------------------------------------------------------------------------------
// Bytes
// ---------------------------------------------------------------------------------------------

/** Decoded byte length from the base64 length alone (no whitespace is ever inserted). */
export function base64ByteLength(b64: string): number {
  const n = b64.length;
  if (n === 0) return 0;
  const padding = b64.charCodeAt(n - 1) === 61 ? (b64.charCodeAt(n - 2) === 61 ? 2 : 1) : 0;
  return Math.floor((n * 3) / 4) - padding;
}

export function decodeBase64(b64: string): Uint8Array {
  let binary: string;
  try {
    binary = atob(b64); // atob ignores ASCII whitespace itself
  } catch {
    return new Uint8Array(0);
  }
  const out = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) out[i] = binary.charCodeAt(i);
  return out;
}

export function toHex(bytes: Uint8Array, limit = bytes.length): string {
  let out = '';
  const n = Math.min(limit, bytes.length);
  for (let i = 0; i < n; i++) out += bytes[i]!.toString(16).padStart(2, '0');
  return out;
}

/** `1.2 KB`-style size; 1024-based. */
export function formatBytes(n: number): string {
  if (!Number.isFinite(n) || n < 0) return '';
  if (n < 1024) return `${n} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let value = n / 1024;
  let i = 0;
  while (value >= 1024 && i < units.length - 1) {
    value /= 1024;
    i++;
  }
  return `${value >= 100 ? Math.round(value) : value.toFixed(1)} ${units[i]}`;
}

/** Hex preview of the first bytes plus the total size, e.g. `0x89504e47 (1.2 KB)`. */
export function bytesPreview(b64: string): string {
  const size = base64ByteLength(b64);
  const head = decodeBase64(b64.slice(0, 16)); // only the first 12 bytes are decoded
  const shown = Math.min(head.length, 8);
  const hex = toHex(head, shown);
  return `0x${hex}${size > shown ? '…' : ''} (${formatBytes(size)})`;
}

/** Classic hex dump: offset, 16 hex bytes, ASCII gutter. */
export function hexDump(bytes: Uint8Array, limit = 64 * 1024): string {
  const n = Math.min(bytes.length, limit);
  const lines: string[] = [];
  for (let off = 0; off < n; off += 16) {
    const chunk = bytes.subarray(off, Math.min(off + 16, n));
    const hex: string[] = [];
    let ascii = '';
    for (let i = 0; i < 16; i++) {
      if (i < chunk.length) {
        const b = chunk[i]!;
        hex.push(b.toString(16).padStart(2, '0'));
        ascii += b >= 0x20 && b < 0x7f ? String.fromCharCode(b) : '.';
      } else {
        hex.push('  ');
      }
    }
    const left = hex.slice(0, 8).join(' ');
    const right = hex.slice(8).join(' ');
    lines.push(`${off.toString(16).padStart(8, '0')}  ${left}  ${right}  |${ascii}|`);
  }
  return lines.join('\n');
}

export type ImageMime = 'image/png' | 'image/jpeg' | 'image/gif' | 'image/webp';

/** Recognize PNG/JPEG/GIF/WebP from magic bytes. */
export function detectImageMime(bytes: Uint8Array): ImageMime | null {
  const at = (i: number) => bytes[i] ?? -1;
  if (at(0) === 0x89 && at(1) === 0x50 && at(2) === 0x4e && at(3) === 0x47) return 'image/png';
  if (at(0) === 0xff && at(1) === 0xd8 && at(2) === 0xff) return 'image/jpeg';
  if (at(0) === 0x47 && at(1) === 0x49 && at(2) === 0x46 && at(3) === 0x38) return 'image/gif';
  if (
    at(0) === 0x52 &&
    at(1) === 0x49 &&
    at(2) === 0x46 &&
    at(3) === 0x46 &&
    at(8) === 0x57 &&
    at(9) === 0x45 &&
    at(10) === 0x42 &&
    at(11) === 0x50
  ) {
    return 'image/webp';
  }
  return null;
}

// ---------------------------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------------------------

/** A JSON number kept as its original text, so no digit is ever lost. */
export class RawNumber {
  constructor(readonly raw: string) {}
  toString(): string {
    return this.raw;
  }
}

/** Parse JSON text without losing number precision; `undefined` when the text is not valid JSON. */
export function parseJsonLossless(text: string): unknown {
  try {
    return parseLossless(text, null, (n: string) => new RawNumber(n));
  } catch {
    return undefined;
  }
}

/**
 * Plain tree of a value for the viewer. Numbers (including ints/decimals nested in arrays and
 * documents) become `RawNumber`; `json` values are parsed losslessly (invalid JSON stays a
 * string); bytes stay base64.
 */
export function valueToPlain(v: Value): unknown {
  switch (v.t) {
    case 'null':
      return null;
    case 'int':
    case 'uInt':
    case 'decimal':
      return new RawNumber(String(v.v));
    case 'float':
      return new RawNumber(floatText(v.v));
    case 'json': {
      const parsed = parseJsonLossless(v.v);
      return parsed === undefined ? v.v : parsed;
    }
    case 'array':
      return v.v.map(valueToPlain);
    case 'document':
      // Documents are ordered pairs on the wire; the plain tree is an object (duplicate keys
      // collapse, integer-like keys sort first - both are limits of JS objects).
      return Object.fromEntries(v.v.map(([k, x]) => [k, valueToPlain(x)]));
    case 'ext':
      return v.v.text || v.v.type;
    default:
      return v.v;
  }
}

/** JSON text of a plain tree (with `RawNumber`s); `indent` > 0 pretty-prints. */
export function stringifyPlain(plain: unknown, indent = 0): string {
  const out: string[] = [];
  const nl = (depth: number) => (indent > 0 ? `\n${' '.repeat(indent * depth)}` : '');
  const walk = (x: unknown, depth: number) => {
    if (x === null || x === undefined) out.push('null');
    else if (x instanceof RawNumber) out.push(x.raw);
    else if (Array.isArray(x)) {
      if (x.length === 0) return void out.push('[]');
      out.push('[');
      x.forEach((item, i) => {
        if (i > 0) out.push(',');
        out.push(nl(depth + 1));
        walk(item, depth + 1);
      });
      out.push(`${nl(depth)}]`);
    } else if (typeof x === 'object') {
      const entries = Object.entries(x as Record<string, unknown>);
      if (entries.length === 0) return void out.push('{}');
      out.push('{');
      entries.forEach(([k, item], i) => {
        if (i > 0) out.push(',');
        out.push(nl(depth + 1), JSON.stringify(k), indent > 0 ? ': ' : ':');
        walk(item, depth + 1);
      });
      out.push(`${nl(depth)}}`);
    } else out.push(JSON.stringify(x) ?? 'null');
  };
  walk(plain, 0);
  return out.join('');
}

function floatText(v: number | string): string {
  if (typeof v === 'number') return Object.is(v, -0) ? '-0' : String(v);
  return v;
}

class Overflow extends Error {}

/**
 * Compact JSON text of an array/document value, cut after about `max` characters (the rest is
 * never serialized, so cost does not grow with the value's size).
 */
function boundedValueJson(v: Value, max: number): string {
  let length = 0;
  const parts: string[] = [];
  const push = (s: string) => {
    parts.push(s);
    length += s.length;
    if (length > max) throw new Overflow();
  };
  const walk = (x: Value) => {
    switch (x.t) {
      case 'null':
        return push('null');
      case 'bool':
        return push(x.v ? 'true' : 'false');
      case 'int':
      case 'uInt':
      case 'decimal':
        return push(String(x.v));
      case 'float':
        return push(floatText(x.v));
      case 'json':
        return push(x.v.length > max ? x.v.slice(0, max) : x.v);
      case 'array':
        push('[');
        x.v.forEach((item, i) => {
          if (i > 0) push(',');
          walk(item);
        });
        return push(']');
      case 'document': {
        push('{');
        let first = true;
        for (const [k, item] of x.v) {
          if (!first) push(',');
          first = false;
          push(`${JSON.stringify(k)}:`);
          walk(item);
        }
        return push('}');
      }
      case 'ext':
        return push(JSON.stringify(x.v.text || x.v.type));
      default:
        return push(JSON.stringify(String((x as { v: unknown }).v)));
    }
  };
  try {
    walk(v);
  } catch (e) {
    if (!(e instanceof Overflow)) throw e;
    return `${parts.join('').slice(0, max)}…`;
  }
  return parts.join('');
}

function unknownKindText(v: Value): string {
  try {
    return JSON.stringify((v as { v?: unknown }).v ?? v) ?? '';
  } catch {
    return String(v);
  }
}

/** Full text of a value, as copied to the clipboard. NULL is the empty string. */
export function valueToText(v: Value, timestamp: TimestampDisplay = 'utc'): string {
  switch (v.t) {
    case 'null':
      return '';
    case 'bool':
      return v.v ? 'true' : 'false';
    case 'float':
      return floatText(v.v);
    case 'int':
    case 'uInt':
      return String(v.v);
    case 'bytes':
      return `0x${toHex(decodeBase64(v.v))}`;
    case 'json':
      return v.v; // the server's original text
    case 'array':
    case 'document':
      return stringifyPlain(valueToPlain(v));
    case 'timestamp':
      return timestamp === 'local' ? toLocalTimestamp(v.v) : v.v;
    case 'ext':
      return v.v.text;
    case 'text':
    case 'decimal':
    case 'date':
    case 'time':
    case 'dateTime':
    case 'uuid':
      return String(v.v);
    default:
      return unknownKindText(v);
  }
}

/** Approximate size in bytes of the text `valueToText` would produce (cheap, no serialization). */
export function estimateValueSize(v: Value): number {
  switch (v.t) {
    case 'text':
    case 'json':
      return v.v.length;
    case 'bytes':
      return base64ByteLength(v.v) * 2 + 2;
    case 'array':
      return v.v.reduce((n, x) => n + estimateValueSize(x) + 1, 2);
    case 'document':
      return v.v.reduce((n, [k, x]) => n + k.length + estimateValueSize(x) + 4, 2);
    case 'ext':
      return v.v.text.length;
    default:
      return 32;
  }
}

/** Copying more than this asks for confirmation first. */
export const LARGE_COPY_BYTES = 5 * 1024 * 1024;

const displayCache = new WeakMap<object, CellDisplay>();
const localDisplayCache = new WeakMap<object, CellDisplay>();

function computeCell(v: Value, maxChars: number, timestamp: TimestampDisplay): CellDisplay {
  switch (v.t) {
    case 'null':
      return { text: 'NULL', kind: 'null', align: 'left' };
    case 'bool':
      return { text: v.v ? 'true' : 'false', kind: 'bool', align: 'left' };
    case 'int':
    case 'uInt':
      return { text: String(v.v), kind: 'number', align: 'right' };
    case 'decimal':
      return { text: v.v, kind: 'number', align: 'right' };
    case 'float':
      return { text: floatText(v.v), kind: 'number', align: 'right' };
    case 'timestamp':
      return {
        text: visibleWhitespace(timestamp === 'local' ? toLocalTimestamp(v.v) : v.v),
        kind: 'temporal',
        align: 'left',
      };
    case 'date':
    case 'time':
    case 'dateTime':
      return { text: visibleWhitespace(v.v), kind: 'temporal', align: 'left' };
    case 'uuid':
      return { text: visibleWhitespace(v.v), kind: 'uuid', align: 'left' };
    case 'text':
      return {
        text: visibleWhitespace(clip(v.v, maxChars)),
        kind: 'text',
        align: 'left',
      };
    case 'bytes':
      return { text: bytesPreview(v.v), kind: 'binary', align: 'left' };
    case 'json':
      // Original text, never parsed per cell.
      return {
        text: visibleWhitespace(clip(v.v, maxChars)),
        kind: 'structured',
        align: 'left',
      };
    case 'array':
    case 'document':
      return {
        text: visibleWhitespace(boundedValueJson(v, maxChars)),
        kind: 'structured',
        align: 'left',
      };
    case 'ext':
      return {
        text: visibleWhitespace(clip(v.v.text || v.v.type, maxChars)),
        kind: 'text',
        align: 'left',
        badge: v.v.type,
      };
    default:
      // A value kind this build does not know: show it as JSON rather than failing.
      return {
        text: visibleWhitespace(clip(unknownKindText(v), maxChars)),
        kind: 'structured',
        align: 'left',
      };
  }
}

/** Short, single-line rendering of a value for a grid cell (cached per value object). */
export function formatCell(
  v: Value,
  maxChars = MAX_CELL_CHARS,
  timestamp: TimestampDisplay = 'utc',
): CellDisplay {
  if (maxChars !== MAX_CELL_CHARS || typeof v !== 'object' || v === null) {
    return computeCell(v, maxChars, timestamp);
  }
  const cache = timestamp === 'local' ? localDisplayCache : displayCache;
  let d = cache.get(v);
  if (!d) {
    d = computeCell(v, maxChars, timestamp);
    cache.set(v, d);
  }
  return d;
}

/** Is the column's values right-aligned (numeric)? Used for header alignment. */
export function isNumericKind(kind: ValueKind): boolean {
  return kind === 'int' || kind === 'uInt' || kind === 'float' || kind === 'decimal';
}

// ---------------------------------------------------------------------------------------------
// Column widths
// ---------------------------------------------------------------------------------------------

export const MIN_COLUMN_WIDTH = 56;
export const MAX_COLUMN_WIDTH = 600;
const CHAR_WIDTH = 7.4; // px per mono character at 12px, plus header padding handled below

const KIND_WIDTH: Record<ValueKind, number> = {
  null: 80,
  bool: 72,
  int: 96,
  uInt: 96,
  float: 110,
  decimal: 110,
  text: 200,
  bytes: 170,
  date: 110,
  time: 100,
  dateTime: 170,
  timestamp: 200,
  uuid: 290,
  json: 220,
  array: 220,
  document: 240,
  ext: 200,
};

/** Default width of a column from its kind and name (header text must stay readable). */
export function defaultColumnWidth(column: Pick<ColumnMeta, 'name' | 'valueKind'>): number {
  const byKind = KIND_WIDTH[column.valueKind] ?? 160;
  const byName = Math.ceil(column.name.length * CHAR_WIDTH) + 44; // sort arrow + resize grip
  const nameBoost = column.name === 'message' || column.name.endsWith('_name') ? 40 : 0;
  return Math.min(MAX_COLUMN_WIDTH, Math.max(MIN_COLUMN_WIDTH, byKind + nameBoost, byName));
}

export function clampColumnWidth(width: number): number {
  return Math.min(MAX_COLUMN_WIDTH, Math.max(MIN_COLUMN_WIDTH, Math.round(width)));
}
