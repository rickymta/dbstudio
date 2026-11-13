import type { Value } from '@/lib/types';

const encoder = new TextEncoder();

export function bytesToHex(bytes: Uint8Array): string {
  return Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
}

/** Parse hex (whitespace and `0x` allowed); `null` when it is not valid hex. */
export function hexToBytes(hex: string): Uint8Array | null {
  const clean = hex.replace(/\s+/g, '').replace(/^0x/i, '');
  if (clean.length % 2 !== 0 || !/^[0-9a-fA-F]*$/.test(clean)) return null;
  const out = new Uint8Array(clean.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(clean.slice(i * 2, i * 2 + 2), 16);
  return out;
}

export function base64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

export function bytesToBase64(bytes: Uint8Array): string {
  let bin = '';
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin);
}

/** Strict UTF-8 decode; `null` when the bytes are not valid UTF-8. */
export function decodeUtf8(bytes: Uint8Array): string | null {
  try {
    return new TextDecoder('utf-8', { fatal: true }).decode(bytes);
  } catch {
    return null;
  }
}

export const textValue = (v: string): Value => ({ t: 'text', v });
export const bytesValue = (bytes: Uint8Array): Value => ({ t: 'bytes', v: bytesToBase64(bytes) });

/** Raw bytes of a text/bytes value (other kinds use their text form). */
export function valueBytes(value: Value): Uint8Array {
  if (value.t === 'bytes') return base64ToBytes(value.v);
  return encoder.encode(valueText(value));
}

/** Plain text form of a value; bytes show as hex. */
export function valueText(value: Value): string {
  switch (value.t) {
    case 'null':
      return '';
    case 'bytes':
      return bytesToHex(base64ToBytes(value.v));
    case 'bool':
    case 'int':
    case 'uInt':
    case 'float':
      return String(value.v);
    default:
      return typeof value.v === 'string' ? value.v : JSON.stringify(value.v);
  }
}

/** Shortened display of a cell: bytes as `0x...`, long text cut. */
export function valueCell(value: Value, max = 200): string {
  const text = value.t === 'bytes' ? `0x${valueText(value)}` : valueText(value);
  return text.length > max ? `${text.slice(0, max)}...` : text;
}

/** Text shown in an editor for `value`. */
export function editText(value: Value): string {
  return valueText(value);
}

/** Value to send back after editing `original` to `draft`; `null` when a bytes draft is not hex. */
export function editedValue(original: Value, draft: string): Value | null {
  if (original.t !== 'bytes') return textValue(draft);
  const bytes = hexToBytes(draft);
  return bytes ? bytesValue(bytes) : null;
}

export function valuesEqual(a: Value, b: Value): boolean {
  return a.t === b.t && valueText(a) === valueText(b);
}

export type StringMode = 'text' | 'json' | 'hex';

/** Pretty-printed JSON, or `null` when `text` is not valid JSON. */
export function prettyJson(text: string): string | null {
  try {
    return JSON.stringify(JSON.parse(text), null, 2);
  } catch {
    return null;
  }
}

/** Mode a string value opens in: invalid UTF-8 bytes can only be shown as hex. */
export function initialStringMode(value: Value): StringMode {
  return value.t === 'bytes' && decodeUtf8(base64ToBytes(value.v)) === null ? 'hex' : 'text';
}

/** Draft text of `value` shown in `mode`. */
export function toDraft(value: Value, mode: StringMode): string {
  if (mode === 'hex') return bytesToHex(valueBytes(value));
  const text = value.t === 'bytes' ? decodeUtf8(base64ToBytes(value.v)) : valueText(value);
  if (text === null) return bytesToHex(valueBytes(value));
  return mode === 'json' ? (prettyJson(text) ?? text) : text;
}

/** Convert a draft between modes; `null` when the draft cannot be shown in the target mode. */
export function convertDraft(draft: string, from: StringMode, to: StringMode): string | null {
  if (from === to) return draft;
  if (from === 'hex') {
    const bytes = hexToBytes(draft);
    if (!bytes) return null;
    const text = decodeUtf8(bytes);
    if (text === null) return null;
    return to === 'json' ? (prettyJson(text) ?? text) : text;
  }
  if (to === 'hex') return bytesToHex(encoder.encode(draft));
  return to === 'json' ? (prettyJson(draft) ?? draft) : draft;
}

/** Value that saving `draft` (shown in `mode`) writes; `null` when a hex draft is invalid. */
export function draftToValue(draft: string, mode: StringMode): Value | null {
  if (mode !== 'hex') return textValue(draft);
  const bytes = hexToBytes(draft);
  return bytes ? bytesValue(bytes) : null;
}
