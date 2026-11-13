import { describe, expect, it } from 'vitest';
import type { Value } from '@/lib/types';
import {
  base64ByteLength,
  bytesPreview,
  clampColumnWidth,
  decodeBase64,
  defaultColumnWidth,
  detectImageMime,
  formatBytes,
  formatCell,
  hexDump,
  valueToPlain,
  valueToText,
} from './format';

const b64 = (bytes: number[]) => btoa(String.fromCharCode(...bytes));
const PNG = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

describe('formatCell', () => {
  it('shows NULL distinctly from the empty string', () => {
    const nul = formatCell({ t: 'null' });
    const empty = formatCell({ t: 'text', v: '' });
    expect(nul).toMatchObject({ text: 'NULL', kind: 'null' });
    expect(empty).toMatchObject({ text: '', kind: 'text' });
    expect(nul.kind).not.toBe(empty.kind);
    expect(valueToText({ t: 'null' })).toBe('');
  });

  it('keeps integers beyond 2^53 verbatim (decimal strings) and right-aligns numbers', () => {
    const big = '9007199254740993';
    expect(formatCell({ t: 'int', v: big })).toEqual({ text: big, kind: 'number', align: 'right' });
    expect(formatCell({ t: 'uInt', v: '18446744073709551615' }).text).toBe('18446744073709551615');
    expect(formatCell({ t: 'int', v: -42 }).text).toBe('-42');
    expect(valueToText({ t: 'int', v: big })).toBe(big);
  });

  it('keeps decimals verbatim, including trailing zeros', () => {
    const v: Value = { t: 'decimal', v: '12345678901234567890.1234500' };
    expect(formatCell(v)).toMatchObject({ text: '12345678901234567890.1234500', align: 'right' });
  });

  it('formats floats including NaN, Infinity and negative zero', () => {
    expect(formatCell({ t: 'float', v: 1.5 }).text).toBe('1.5');
    expect(formatCell({ t: 'float', v: 'NaN' }).text).toBe('NaN');
    expect(formatCell({ t: 'float', v: 'Infinity' }).text).toBe('Infinity');
    expect(formatCell({ t: 'float', v: '-Infinity' }).text).toBe('-Infinity');
    expect(formatCell({ t: 'float', v: -0 }).text).toBe('-0');
  });

  it('formats booleans, temporal values and uuids as given', () => {
    expect(formatCell({ t: 'bool', v: true }).text).toBe('true');
    expect(formatCell({ t: 'bool', v: false }).text).toBe('false');
    for (const t of ['date', 'time', 'dateTime', 'timestamp'] as const) {
      expect(formatCell({ t, v: '2024-01-02T03:04:05.123456Z' }).text).toBe(
        '2024-01-02T03:04:05.123456Z',
      );
    }
    const id = '123e4567-e89b-12d3-a456-426614174000';
    expect(formatCell({ t: 'uuid', v: id }).text).toBe(id);
  });

  it('shows json on one line and arrays/documents as compact JSON', () => {
    expect(formatCell({ t: 'json', v: { a: 1, b: [true, null] } }).text).toBe(
      '{"a":1,"b":[true,null]}',
    );
    const arr: Value = { t: 'array', v: [{ t: 'int', v: '9007199254740993' }, { t: 'null' }] };
    expect(formatCell(arr).text).toBe('["9007199254740993",null]');
    const doc: Value = { t: 'document', v: { name: { t: 'text', v: 'x' } } };
    expect(formatCell(doc).text).toBe('{"name":"x"}');
  });

  it('previews bytes as hex plus size', () => {
    expect(formatCell({ t: 'bytes', v: b64(PNG) }).text).toBe('0x89504e470d0a1a0a (8 B)');
    const long = bytesPreview(b64(Array.from({ length: 2048 }, (_, i) => i % 256)));
    expect(long).toBe('0x0001020304050607… (2.0 KB)');
  });

  it('marks newlines and tabs, and truncates long text', () => {
    expect(formatCell({ t: 'text', v: 'a\nb\r\nc\td' }).text).toBe('a↵b↵c⇥d');
    const text = formatCell({ t: 'text', v: 'x'.repeat(5000) }).text;
    expect(text.length).toBe(201);
    expect(text.endsWith('…')).toBe(true);
  });
});

describe('valueToText / valueToPlain', () => {
  it('returns full, untruncated text', () => {
    const long = 'y'.repeat(5000);
    expect(valueToText({ t: 'text', v: long })).toBe(long);
    expect(valueToText({ t: 'bool', v: false })).toBe('false');
    expect(valueToText({ t: 'bytes', v: b64([1, 255]) })).toBe('0x01ff');
    expect(valueToText({ t: 'json', v: { a: [1] } })).toBe('{"a":[1]}');
  });

  it('flattens value trees', () => {
    const v: Value = {
      t: 'document',
      v: { n: { t: 'int', v: 1 }, xs: { t: 'array', v: [{ t: 'null' }, { t: 'text', v: 's' }] } },
    };
    expect(valueToPlain(v)).toEqual({ n: 1, xs: [null, 's'] });
  });
});

describe('bytes helpers', () => {
  it('computes byte length without decoding', () => {
    expect(base64ByteLength('')).toBe(0);
    expect(base64ByteLength(b64([1]))).toBe(1);
    expect(base64ByteLength(b64([1, 2]))).toBe(2);
    expect(base64ByteLength(b64([1, 2, 3]))).toBe(3);
    expect(decodeBase64('not base64!!')).toHaveLength(0);
  });

  it('formats a hex dump with offset, hex and ascii', () => {
    const bytes = new TextEncoder().encode('Hello, hex dump!\n');
    const lines = hexDump(bytes).split('\n');
    expect(lines).toHaveLength(2);
    expect(lines[0]).toBe(
      '00000000  48 65 6c 6c 6f 2c 20 68  65 78 20 64 75 6d 70 21  |Hello, hex dump!|',
    );
    expect(lines[1]!.startsWith('00000010  0a')).toBe(true);
    expect(lines[1]!.endsWith('|.|')).toBe(true);
  });

  it('detects image types from magic bytes', () => {
    expect(detectImageMime(Uint8Array.from(PNG))).toBe('image/png');
    expect(detectImageMime(Uint8Array.from([0xff, 0xd8, 0xff, 0xe0]))).toBe('image/jpeg');
    expect(detectImageMime(new TextEncoder().encode('GIF89a'))).toBe('image/gif');
    expect(detectImageMime(new TextEncoder().encode('RIFF\0\0\0\0WEBPVP8 '))).toBe('image/webp');
    expect(detectImageMime(new TextEncoder().encode('plain text'))).toBeNull();
    expect(detectImageMime(new Uint8Array(0))).toBeNull();
  });
});

describe('formatBytes and column widths', () => {
  it('formats sizes', () => {
    expect(formatBytes(0)).toBe('0 B');
    expect(formatBytes(1023)).toBe('1023 B');
    expect(formatBytes(1536)).toBe('1.5 KB');
    expect(formatBytes(5 * 1024 * 1024)).toBe('5.0 MB');
    expect(formatBytes(3 * 1024 ** 3)).toBe('3.0 GB');
  });

  it('derives default widths from kind and name within bounds', () => {
    expect(defaultColumnWidth({ name: 'id', valueKind: 'int' })).toBeLessThan(
      defaultColumnWidth({ name: 'trace_id', valueKind: 'uuid' }),
    );
    expect(
      defaultColumnWidth({ name: 'a_really_long_column_name_that_needs_room', valueKind: 'bool' }),
    ).toBeGreaterThan(200);
    expect(clampColumnWidth(5)).toBe(56);
    expect(clampColumnWidth(99999)).toBe(600);
  });
});
