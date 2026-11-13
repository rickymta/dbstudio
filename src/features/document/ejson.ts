// Extended JSON (EJSON) <-> `Value` for MongoDB documents.
//
// `toEjson` prints a document the way the editor shows it: extension types use their canonical
// wrappers (`{"$oid": ...}`, `{"$numberLong": ...}` ...) so the text parses back to the same BSON
// types on the server. `validateEjson` reports the first syntax error with a position, and
// `fromEjson` parses text back into a `Value` (used for round-trip checks and previews).
//
// The parser is hand-written (not JSON.parse) so that numbers keep their original text (no
// precision loss for 64-bit integers) and syntax errors carry a line/column on every engine.

import type { Value } from '@/lib/types';

// ---------------------------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------------------------

const INDENT = 2;

/** Pretty EJSON text of a value (documents print with two-space indentation). */
export function toEjson(v: Value, indent = INDENT): string {
  const out: string[] = [];
  write(v, 0, indent, out);
  return out.join('');
}

/** Single-line EJSON, used for compact previews. */
export function toEjsonCompact(v: Value): string {
  return toEjson(v, 0);
}

function nl(depth: number, indent: number): string {
  return indent > 0 ? `\n${' '.repeat(indent * depth)}` : '';
}

const str = (s: string) => JSON.stringify(s);

function wrapper(key: string, inner: string): string {
  return `{${str(key)}: ${inner}}`;
}

function floatText(v: number | string): string {
  if (typeof v === 'string') {
    if (v === 'NaN' || v === 'Infinity' || v === '-Infinity') {
      return wrapper('$numberDouble', str(v));
    }
    return /^-?\d+$/.test(v) ? `${v}.0` : v;
  }
  if (Number.isNaN(v)) return wrapper('$numberDouble', str('NaN'));
  if (!Number.isFinite(v)) return wrapper('$numberDouble', str(v > 0 ? 'Infinity' : '-Infinity'));
  if (Object.is(v, -0)) return '-0.0';
  const s = String(v);
  // Keep doubles distinguishable from integers: `1` would come back as an Int32.
  return /^-?\d+$/.test(s) ? `${s}.0` : s;
}

function extText(type: string, text: string): string {
  switch (type) {
    case 'objectId':
      return wrapper('$oid', str(text));
    case 'int64':
      return wrapper('$numberLong', str(text));
    case 'dateMillis':
      return wrapper('$date', wrapper('$numberLong', str(text)));
    case 'binary': {
      const i = text.indexOf(':');
      const subType = i >= 0 ? text.slice(0, i) : '00';
      const base64 = i >= 0 ? text.slice(i + 1) : text;
      return `{"$binary": {"base64": ${str(base64)}, "subType": ${str(subType)}}}`;
    }
    case 'bsonTimestamp': {
      const [t, i] = text.split(',');
      return `{"$timestamp": {"t": ${Number(t) || 0}, "i": ${Number(i) || 0}}}`;
    }
    case 'regex': {
      const last = text.lastIndexOf('/');
      const pattern = text.startsWith('/') && last > 0 ? text.slice(1, last) : text;
      const options = text.startsWith('/') && last > 0 ? text.slice(last + 1) : '';
      return `{"$regularExpression": {"pattern": ${str(pattern)}, "options": ${str(options)}}}`;
    }
    case 'javascript':
      return wrapper('$code', str(text));
    case 'symbol':
      return wrapper('$symbol', str(text));
    case 'minKey':
      return wrapper('$minKey', '1');
    case 'maxKey':
      return wrapper('$maxKey', '1');
    case 'undefined':
      return wrapper('$undefined', 'true');
    default:
      // Kinds without an EJSON form (dbPointer ...): show the server's text.
      return str(text);
  }
}

function write(v: Value, depth: number, indent: number, out: string[]): void {
  switch (v.t) {
    case 'null':
      out.push('null');
      return;
    case 'bool':
      out.push(v.v ? 'true' : 'false');
      return;
    case 'int':
      // Beyond 2^53 the wire carries a decimal string: it must stay a 64-bit integer.
      out.push(typeof v.v === 'string' ? wrapper('$numberLong', str(v.v)) : String(v.v));
      return;
    case 'uInt':
      out.push(typeof v.v === 'string' ? wrapper('$numberLong', str(v.v)) : String(v.v));
      return;
    case 'float':
      out.push(floatText(v.v));
      return;
    case 'decimal':
      out.push(wrapper('$numberDecimal', str(v.v)));
      return;
    case 'text':
    case 'date':
    case 'time':
    case 'dateTime':
      out.push(str(v.v));
      return;
    case 'bytes':
      out.push(`{"$binary": {"base64": ${str(v.v)}, "subType": "00"}}`);
      return;
    case 'timestamp':
      out.push(wrapper('$date', str(v.v)));
      return;
    case 'uuid':
      out.push(wrapper('$uuid', str(v.v)));
      return;
    case 'json':
      out.push(v.v.trim() === '' ? 'null' : v.v);
      return;
    case 'ext':
      out.push(extText(v.v.type, v.v.text));
      return;
    case 'array': {
      if (v.v.length === 0) return void out.push('[]');
      out.push('[');
      v.v.forEach((item, i) => {
        if (i > 0) out.push(',');
        out.push(nl(depth + 1, indent));
        write(item, depth + 1, indent, out);
      });
      out.push(`${nl(depth, indent)}]`);
      return;
    }
    case 'document': {
      if (v.v.length === 0) return void out.push('{}');
      out.push('{');
      v.v.forEach(([k, item], i) => {
        if (i > 0) out.push(',');
        out.push(nl(depth + 1, indent), str(k), indent > 0 ? ': ' : ':');
        write(item, depth + 1, indent, out);
      });
      out.push(`${nl(depth, indent)}}`);
      return;
    }
    default:
      out.push('null');
  }
}

// ---------------------------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------------------------

export type EjsonCheck =
  { ok: true } | { ok: false; error: string; line: number; col: number; offset: number };

/** Raw syntax tree with numbers kept as text. */
type Node =
  | { k: 'null' }
  | { k: 'bool'; v: boolean }
  | { k: 'num'; raw: string }
  | { k: 'str'; v: string }
  | { k: 'arr'; v: Node[] }
  | { k: 'obj'; v: [string, Node][] };

class SyntaxError2 extends Error {
  constructor(
    message: string,
    readonly offset: number,
  ) {
    super(message);
  }
}

const WS = new Set([' ', '\t', '\n', '\r']);

function parseText(text: string): Node {
  let i = 0;
  const fail = (msg: string, at = i): never => {
    throw new SyntaxError2(msg, at);
  };
  const skip = () => {
    while (i < text.length && WS.has(text[i]!)) i++;
  };
  const describe = () => (i >= text.length ? 'end of input' : `"${text[i]}"`);

  const parseString = (): string => {
    const start = i;
    i++; // opening quote
    let out = '';
    for (;;) {
      if (i >= text.length) return fail('Unterminated string', start);
      const c = text[i]!;
      if (c === '"') {
        i++;
        return out;
      }
      if (c === '\n') return fail('Unterminated string', start);
      if (c === '\\') {
        const e = text[i + 1];
        const simple: Record<string, string> = {
          '"': '"',
          '\\': '\\',
          '/': '/',
          b: '\b',
          f: '\f',
          n: '\n',
          r: '\r',
          t: '\t',
        };
        if (e !== undefined && e in simple) {
          out += simple[e];
          i += 2;
        } else if (e === 'u') {
          const hex = text.slice(i + 2, i + 6);
          if (!/^[0-9a-fA-F]{4}$/.test(hex)) return fail('Invalid unicode escape', i);
          out += String.fromCharCode(parseInt(hex, 16));
          i += 6;
        } else {
          return fail('Invalid escape sequence', i);
        }
      } else {
        out += c;
        i++;
      }
    }
  };

  const NUM = /-?(0|[1-9]\d*)(\.\d+)?([eE][+-]?\d+)?/y;
  const parseValue = (depth: number): Node => {
    skip();
    if (depth > 200) return fail('Document is nested too deeply');
    const c = text[i];
    if (c === '{') {
      i++;
      const entries: [string, Node][] = [];
      skip();
      if (text[i] === '}') {
        i++;
        return { k: 'obj', v: entries };
      }
      for (;;) {
        skip();
        if (text[i] !== '"') return fail(`Expected a quoted key but found ${describe()}`);
        const key = parseString();
        skip();
        if (text[i] !== ':') return fail(`Expected ":" but found ${describe()}`);
        i++;
        entries.push([key, parseValue(depth + 1)]);
        skip();
        if (text[i] === ',') {
          i++;
          continue;
        }
        if (text[i] === '}') {
          i++;
          return { k: 'obj', v: entries };
        }
        return fail(`Expected "," or "}" but found ${describe()}`);
      }
    }
    if (c === '[') {
      i++;
      const items: Node[] = [];
      skip();
      if (text[i] === ']') {
        i++;
        return { k: 'arr', v: items };
      }
      for (;;) {
        items.push(parseValue(depth + 1));
        skip();
        if (text[i] === ',') {
          i++;
          continue;
        }
        if (text[i] === ']') {
          i++;
          return { k: 'arr', v: items };
        }
        return fail(`Expected "," or "]" but found ${describe()}`);
      }
    }
    if (c === '"') return { k: 'str', v: parseString() };
    for (const [word, node] of [
      ['true', { k: 'bool', v: true }],
      ['false', { k: 'bool', v: false }],
      ['null', { k: 'null' }],
    ] as [string, Node][]) {
      if (text.startsWith(word, i)) {
        i += word.length;
        return node;
      }
    }
    NUM.lastIndex = i;
    const m = NUM.exec(text);
    if (m && m.index === i) {
      i += m[0].length;
      return { k: 'num', raw: m[0] };
    }
    return fail(i >= text.length ? 'Unexpected end of input' : `Unexpected ${describe()}`);
  };

  const root = parseValue(0);
  skip();
  if (i < text.length) fail(`Unexpected ${describe()} after the document`);
  return root;
}

function locate(text: string, offset: number): { line: number; col: number } {
  let line = 1;
  let col = 1;
  const end = Math.min(offset, text.length);
  for (let i = 0; i < end; i++) {
    if (text[i] === '\n') {
      line++;
      col = 1;
    } else col++;
  }
  return { line, col };
}

/** Syntax check of an EJSON document: the text must be one JSON object. */
export function validateEjson(text: string): EjsonCheck {
  if (text.trim() === '') {
    return { ok: false, error: 'Empty document', line: 1, col: 1, offset: 0 };
  }
  try {
    const root = parseText(text);
    if (root.k !== 'obj') {
      const offset = text.length - text.trimStart().length;
      return {
        ok: false,
        error: 'A document must be a JSON object',
        offset,
        ...locate(text, offset),
      };
    }
    return { ok: true };
  } catch (e) {
    if (e instanceof SyntaxError2) {
      return { ok: false, error: e.message, offset: e.offset, ...locate(text, e.offset) };
    }
    throw e;
  }
}

// -- tree -> Value ----------------------------------------------------------------------------

const INT32_MIN = -2147483648;
const INT32_MAX = 2147483647;

function numberValue(raw: string): Value {
  if (/[.eE]/.test(raw)) return { t: 'float', v: Number(raw) };
  const n = Number(raw);
  if (Number.isSafeInteger(n)) return { t: 'int', v: n };
  return { t: 'int', v: raw };
}

const asObj = (n: Node | undefined): Map<string, Node> | null =>
  n?.k === 'obj' ? new Map(n.v) : null;
const asStr = (n: Node | undefined): string | null => (n?.k === 'str' ? n.v : null);

function uuidFromBase64(b64: string): string | null {
  try {
    const bin = atob(b64);
    if (bin.length !== 16) return null;
    const h = Array.from(bin, (c) => c.charCodeAt(0).toString(16).padStart(2, '0')).join('');
    return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`;
  } catch {
    return null;
  }
}

/** The extension value an object like `{"$oid": ...}` stands for, or `null` for a plain document. */
function extended(entries: [string, Node][]): Value | null {
  if (entries.length !== 1) return null;
  const [key, node] = entries[0]!;
  if (!key.startsWith('$')) return null;
  const s = asStr(node);
  const inner = asObj(node);
  switch (key) {
    case '$oid':
      return s !== null ? { t: 'ext', v: { type: 'objectId', text: s } } : null;
    case '$numberInt':
      return s !== null && /^-?\d+$/.test(s) && Number(s) >= INT32_MIN && Number(s) <= INT32_MAX
        ? { t: 'int', v: Number(s) }
        : null;
    case '$numberLong':
      return s !== null ? { t: 'ext', v: { type: 'int64', text: s } } : null;
    case '$numberDouble':
      if (s === null) return null;
      return { t: 'float', v: s === 'NaN' || s.endsWith('Infinity') ? s : Number(s) };
    case '$numberDecimal':
      return s !== null ? { t: 'decimal', v: s } : null;
    case '$date': {
      if (s !== null) return { t: 'timestamp', v: s };
      if (node.k === 'num') return { t: 'ext', v: { type: 'dateMillis', text: node.raw } };
      const ms = asStr(inner?.get('$numberLong'));
      return inner && inner.size === 1 && ms !== null
        ? { t: 'ext', v: { type: 'dateMillis', text: ms } }
        : null;
    }
    case '$binary': {
      const b64 = asStr(inner?.get('base64'));
      const sub = asStr(inner?.get('subType'));
      if (!inner || inner.size !== 2 || b64 === null || sub === null) return null;
      const subHex = sub.padStart(2, '0').toLowerCase();
      if (subHex === '00') return { t: 'bytes', v: b64 };
      if (subHex === '04') {
        const u = uuidFromBase64(b64);
        if (u) return { t: 'uuid', v: u };
      }
      return { t: 'ext', v: { type: 'binary', text: `${subHex}:${b64}` } };
    }
    case '$uuid':
      return s !== null ? { t: 'uuid', v: s } : null;
    case '$timestamp': {
      const t = inner?.get('t');
      const i = inner?.get('i');
      return inner && inner.size === 2 && t?.k === 'num' && i?.k === 'num'
        ? { t: 'ext', v: { type: 'bsonTimestamp', text: `${t.raw},${i.raw}` } }
        : null;
    }
    case '$regularExpression': {
      const p = asStr(inner?.get('pattern'));
      const o = asStr(inner?.get('options'));
      return inner && inner.size === 2 && p !== null && o !== null
        ? { t: 'ext', v: { type: 'regex', text: `/${p}/${o}` } }
        : null;
    }
    case '$code':
      return s !== null ? { t: 'ext', v: { type: 'javascript', text: s } } : null;
    case '$symbol':
      return s !== null ? { t: 'ext', v: { type: 'symbol', text: s } } : null;
    case '$minKey':
      return { t: 'ext', v: { type: 'minKey', text: 'MinKey' } };
    case '$maxKey':
      return { t: 'ext', v: { type: 'maxKey', text: 'MaxKey' } };
    case '$undefined':
      return { t: 'ext', v: { type: 'undefined', text: 'undefined' } };
    default:
      return null;
  }
}

function toValue(n: Node): Value {
  switch (n.k) {
    case 'null':
      return { t: 'null' };
    case 'bool':
      return { t: 'bool', v: n.v };
    case 'num':
      return numberValue(n.raw);
    case 'str':
      return { t: 'text', v: n.v };
    case 'arr':
      return { t: 'array', v: n.v.map(toValue) };
    case 'obj':
      return extended(n.v) ?? { t: 'document', v: n.v.map(([k, x]) => [k, toValue(x)]) };
  }
}

/** Parse EJSON text into a value; throws `Error` (message with line/col) on a syntax error. */
export function fromEjson(text: string): Value {
  try {
    return toValue(parseText(text));
  } catch (e) {
    if (e instanceof SyntaxError2) {
      const { line, col } = locate(text, e.offset);
      throw new Error(`${e.message} (line ${line}, column ${col})`, { cause: e });
    }
    throw e;
  }
}
