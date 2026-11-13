// Pure value formatting for the data grid and the cell viewer. Nothing here touches React.
// Large integers arrive as decimal strings (beyond 2^53) and must never go through `Number`.

import type { ColumnMeta, Value, ValueKind } from '@/lib/types';

export type CellKind =
  'null' | 'bool' | 'number' | 'temporal' | 'text' | 'uuid' | 'binary' | 'structured';

export interface CellDisplay {
  text: string;
  kind: CellKind;
  align: 'left' | 'right';
}

/** Longest text kept for in-cell display; the cell is clipped with CSS ellipsis anyway. */
export const MAX_CELL_CHARS = 200;

const NEWLINE = '↵'; // ↵
const TAB = '⇥'; // ⇥

function clip(text: string, max: number): string {
  return text.length > max ? `${text.slice(0, max)}…` : text;
}

/** Make control characters visible so a value always renders on one line. */
export function visibleWhitespace(text: string): string {
  return text.replace(/\r\n|\r|\n/g, NEWLINE).replace(/\t/g, TAB);
}

// ---------------------------------------------------------------------------------------------
// Bytes
// ---------------------------------------------------------------------------------------------

/** Decoded byte length of a base64 string without decoding it. */
export function base64ByteLength(b64: string): number {
  const clean = b64.replace(/\s+/g, '');
  if (clean.length === 0) return 0;
  const padding = clean.endsWith('==') ? 2 : clean.endsWith('=') ? 1 : 0;
  return Math.floor((clean.length * 3) / 4) - padding;
}

export function decodeBase64(b64: string): Uint8Array {
  let binary: string;
  try {
    binary = atob(b64.replace(/\s+/g, ''));
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
  const head = decodeBase64(b64.replace(/\s+/g, '').slice(0, 16));
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

/** Plain JS form of a value tree (for JSON rendering); bytes stay base64, big ints stay strings. */
export function valueToPlain(v: Value): unknown {
  switch (v.t) {
    case 'null':
      return null;
    case 'array':
      return v.v.map(valueToPlain);
    case 'document': {
      const out: Record<string, unknown> = {};
      for (const [k, x] of Object.entries(v.v)) out[k] = valueToPlain(x);
      return out;
    }
    default:
      return v.v;
  }
}

function floatText(v: number | string): string {
  if (typeof v === 'number') return Object.is(v, -0) ? '-0' : String(v);
  return v;
}

function stringifyJson(value: unknown): string {
  try {
    return JSON.stringify(value) ?? 'null';
  } catch {
    return String(value);
  }
}

/** Full text of a value, as copied to the clipboard. NULL is the empty string. */
export function valueToText(v: Value): string {
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
      return stringifyJson(v.v);
    case 'array':
    case 'document':
      return stringifyJson(valueToPlain(v));
    default:
      return String(v.v);
  }
}

/** Short, single-line rendering of a value for a grid cell. */
export function formatCell(v: Value, maxChars = MAX_CELL_CHARS): CellDisplay {
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
    case 'date':
    case 'time':
    case 'dateTime':
    case 'timestamp':
      return { text: v.v, kind: 'temporal', align: 'left' };
    case 'uuid':
      return { text: v.v, kind: 'uuid', align: 'left' };
    case 'text':
      return {
        text: visibleWhitespace(clip(v.v, maxChars)),
        kind: 'text',
        align: 'left',
      };
    case 'bytes':
      return { text: bytesPreview(v.v), kind: 'binary', align: 'left' };
    case 'json':
      return {
        text: visibleWhitespace(clip(stringifyJson(v.v), maxChars)),
        kind: 'structured',
        align: 'left',
      };
    case 'array':
    case 'document':
      return {
        text: visibleWhitespace(clip(stringifyJson(valueToPlain(v)), maxChars)),
        kind: 'structured',
        align: 'left',
      };
  }
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
