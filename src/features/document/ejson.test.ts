import { describe, expect, it } from 'vitest';
import type { Value } from '@/lib/types';
import { fromEjson, toEjson, toEjsonCompact, validateEjson } from './ejson';

const ext = (type: string, text: string): Value => ({ t: 'ext', v: { type, text } });
const field = (k: string, v: Value): [string, Value] => [k, v];

const samples: [string, Value][] = [
  ['objectId', ext('objectId', '64b7f0c2a1b2c3d4e5f60718')],
  ['int64', ext('int64', '9007199254740993')],
  ['decimal', { t: 'decimal', v: '12.50' }],
  ['timestamp', { t: 'timestamp', v: '2024-01-02T03:04:05.678Z' }],
  ['dateMillis', ext('dateMillis', '-62135596800001')],
  ['bytes', { t: 'bytes', v: 'AAEC' }],
  ['uuid', { t: 'uuid', v: '00010203-0405-0607-0809-0a0b0c0d0e0f' }],
  ['binary', ext('binary', '80:AAEC')],
  ['bsonTimestamp', ext('bsonTimestamp', '1700000000,3')],
  ['regex', ext('regex', '/^a.*b$/i')],
  ['javascript', ext('javascript', '{"$code":"function(){}"}')],
  [
    'javascriptWithScope',
    ext('javascriptWithScope', '{"$code":"x+y","$scope":{"y":{"$numberInt":"2"}}}'),
  ],
  ['symbol', ext('symbol', '{"$symbol":"sym"}')],
  ['minKey', ext('minKey', 'MinKey')],
  ['maxKey', ext('maxKey', 'MaxKey')],
  ['float', { t: 'float', v: 1 }],
  ['float frac', { t: 'float', v: 1.5 }],
  ['int', { t: 'int', v: 42 }],
  ['text', { t: 'text', v: 'he said "hi"\n' }],
  ['bool', { t: 'bool', v: true }],
  ['null', { t: 'null' }],
];

describe('toEjson / fromEjson round trip', () => {
  it.each(samples)('%s survives a round trip', (_name, value) => {
    const doc: Value = { t: 'document', v: [field('v', value)] };
    expect(fromEjson(toEjson(doc))).toEqual(doc);
  });

  it('keeps key order, nesting and arrays', () => {
    const doc: Value = {
      t: 'document',
      v: [
        field('z', { t: 'int', v: 1 }),
        field('a', {
          t: 'document',
          v: [
            field('list', { t: 'array', v: [{ t: 'int', v: 1 }, ext('objectId', 'a'.repeat(24))] }),
          ],
        }),
        field('empty', { t: 'document', v: [] }),
        field('arr', { t: 'array', v: [] }),
      ],
    };
    expect(fromEjson(toEjson(doc))).toEqual(doc);
    expect(toEjson(doc).indexOf('"z"')).toBeLessThan(toEjson(doc).indexOf('"a"'));
  });

  it('prints canonical wrappers', () => {
    const doc: Value = {
      t: 'document',
      v: [
        field('_id', ext('objectId', '64b7f0c2a1b2c3d4e5f60718')),
        field('n', ext('int64', '5')),
        field('d', { t: 'decimal', v: '1.5' }),
        field('t', { t: 'timestamp', v: '2024-01-01T00:00:00.000Z' }),
        field('b', { t: 'bytes', v: 'AAEC' }),
        field('ts', ext('bsonTimestamp', '1,2')),
        field('r', ext('regex', '/a/i')),
        field('f', { t: 'float', v: 2 }),
      ],
    };
    const text = toEjson(doc);
    expect(text).toContain('"_id": {"$oid": "64b7f0c2a1b2c3d4e5f60718"}');
    expect(text).toContain('{"$numberLong": "5"}');
    expect(text).toContain('{"$numberDecimal": "1.5"}');
    expect(text).toContain('{"$date": "2024-01-01T00:00:00.000Z"}');
    expect(text).toContain('{"$binary": {"base64": "AAEC", "subType": "00"}}');
    expect(text).toContain('{"$timestamp": {"t": 1, "i": 2}}');
    expect(text).toContain('{"$regularExpression": {"pattern": "a", "options": "i"}}');
    expect(text).toContain('"f": 2.0');
  });

  it('keeps Int32, negative Int32 and Int64 distinct', () => {
    const doc: Value = {
      t: 'document',
      v: [
        field('a', { t: 'int', v: 5 }),
        field('b', { t: 'int', v: -5 }),
        field('c', ext('int64', '-5')),
      ],
    };
    const text = toEjson(doc);
    expect(text).toContain('"a": 5');
    expect(text).toContain('"b": {"$numberInt": "-5"}');
    expect(text).toContain('"c": {"$numberLong": "-5"}');
    expect(fromEjson(text)).toEqual(doc);
  });

  it('embeds the canonical text of code, scope and symbol values', () => {
    const doc = fromEjson(
      toEjson({
        t: 'document',
        v: [field('s', ext('javascriptWithScope', '{"$code":"a","$scope":{"k":1}}'))],
      }),
    );
    expect(doc).toEqual({
      t: 'document',
      v: [['s', ext('javascriptWithScope', '{"$code":"a","$scope":{"k":1}}')]],
    });
    expect(
      toEjson({ t: 'document', v: [field('j', ext('javascript', '{"$code":"f()"}'))] }),
    ).toMatch(/"j": \{\s+"\$code": "f\(\)"\s+\}/);
    expect(toEjson({ t: 'document', v: [field('y', ext('symbol', '{"$symbol":"q"}'))] })).toMatch(
      /"y": \{\s+"\$symbol": "q"\s+\}/,
    );
  });

  it('shows dbPointer text but refuses to parse it back', () => {
    const text = toEjson({
      t: 'document',
      v: [field('p', ext('dbPointer', '{"$dbPointer":{"$ref":"c","$id":{"$oid":"a"}}}'))],
    });
    expect(text).toContain('"$dbPointer"');
    expect(() => fromEjson(text)).toThrow(/dbPointer/);
  });

  it('keeps 64-bit integers carried as strings as $numberLong', () => {
    const text = toEjson({ t: 'document', v: [field('big', { t: 'int', v: '9007199254740993' })] });
    expect(text).toContain('{"$numberLong": "9007199254740993"}');
  });

  it('parses plain big integers without losing digits', () => {
    expect(fromEjson('{"n": 9007199254740993}')).toEqual({
      t: 'document',
      v: [['n', { t: 'int', v: '9007199254740993' }]],
    });
  });

  it('keeps a document with several $ keys as a plain document', () => {
    const v = fromEjson('{"$oid": "a", "x": 1}');
    expect(v.t).toBe('document');
  });

  it('compact form is one line', () => {
    expect(toEjsonCompact({ t: 'document', v: [field('a', { t: 'int', v: 1 })] })).toBe('{"a":1}');
  });
});

describe('validateEjson', () => {
  it('accepts objects', () => {
    expect(validateEjson('{"a": [1, 2, {"b": null}], "c": "x"}')).toEqual({ ok: true });
    expect(validateEjson('  {}  ')).toEqual({ ok: true });
  });

  it('reports line and column of a syntax error', () => {
    const r = validateEjson('{\n  "a": 1,\n  "b": ,\n}');
    expect(r.ok).toBe(false);
    if (!r.ok) {
      expect(r.line).toBe(3);
      expect(r.col).toBe(8);
    }
  });

  it('rejects trailing commas, single quotes and unterminated strings', () => {
    expect(validateEjson('{"a": 1,}').ok).toBe(false);
    expect(validateEjson("{'a': 1}").ok).toBe(false);
    expect(validateEjson('{"a": "x').ok).toBe(false);
    expect(validateEjson('{"a": 1} x').ok).toBe(false);
  });

  it('rejects non-objects and empty text', () => {
    expect(validateEjson('[1]').ok).toBe(false);
    expect(validateEjson('5').ok).toBe(false);
    expect(validateEjson('   ').ok).toBe(false);
  });

  it('fromEjson throws with the position', () => {
    expect(() => fromEjson('{"a": }')).toThrow(/line 1, column 7/);
  });
});
