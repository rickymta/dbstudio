import { describe, expect, it } from 'vitest';
import type { ObjectPath } from '@/lib/types';
import {
  buildSpec,
  defaultForm,
  extensionFor,
  formFromSpec,
  formatsFor,
  needsZip,
  saveFilters,
  sourceFromPath,
  suggestedName,
  validate,
} from './specForm';

const table = (name: string): ObjectPath => ({
  segments: [
    { kind: 'database', name: 'app' },
    { kind: 'folder', name: 'tables' },
    { kind: 'table', name },
  ],
});
const db: ObjectPath = { segments: [{ kind: 'database', name: 'app' }] };
const ID = '00000000-0000-4000-8000-000000000001';

describe('sourceFromPath', () => {
  it('follows the node the wizard opens on', () => {
    expect(sourceFromPath(db)).toEqual({ kind: 'database', path: db });
    expect(sourceFromPath(table('a'))).toEqual({ kind: 'tables', paths: [table('a')] });
    expect(sourceFromPath(undefined, 'SELECT 1')).toEqual({
      kind: 'query',
      text: 'SELECT 1',
      database: null,
    });
    const schema: ObjectPath = { segments: [...db.segments, { kind: 'schema', name: 'public' }] };
    expect(sourceFromPath(schema).kind).toBe('schema');
  });
});

describe('buildSpec', () => {
  it('sends the chosen format with its options and always overwrites (the dialog asked)', () => {
    const form = { ...defaultForm(sourceFromPath(table('orders'))), format: 'csv' as const };
    form.csv = { ...form.csv, delimiter: ';' };
    form.path = '/tmp/o.csv';
    const spec = buildSpec(ID, form);
    expect(spec.format).toMatchObject({ kind: 'csv', delimiter: ';', encoding: 'utf8Bom' });
    expect(spec.destination).toEqual({ path: '/tmp/o.csv', overwrite: true });
    expect(spec.content).toBe('both');
    expect(spec.values).toEqual({ binary: 'base64' });
  });

  it('a query is always exported as data', () => {
    const form = defaultForm(sourceFromPath(undefined, 'SELECT 1'));
    expect(form.content).toBe('data');
    form.content = 'both';
    expect(buildSpec(ID, form).content).toBe('data');
  });

  it('restores a preset into the form', () => {
    const base = defaultForm(sourceFromPath(table('orders')));
    const spec = buildSpec(ID, {
      ...base,
      format: 'json',
      json: { layout: 'ndjson', pretty: false },
      content: 'data',
      binary: 'hex',
    });
    const restored = formFromSpec(spec, base);
    expect(restored).toMatchObject({
      format: 'json',
      content: 'data',
      binary: 'hex',
      json: { layout: 'ndjson' },
    });
    expect(formFromSpec('nope', base)).toBeNull();
    expect(formFromSpec({}, base)).toBeNull();
  });
});

describe('output file', () => {
  it('uses a zip when several files are produced', () => {
    const one = defaultForm(sourceFromPath(table('orders')));
    expect(needsZip({ ...one, format: 'csv', content: 'data' })).toBe(false);
    expect(needsZip({ ...one, format: 'csv', content: 'both' })).toBe(true);
    expect(needsZip({ ...one, format: 'csv', content: 'schema' })).toBe(false);
    expect(needsZip({ ...one, format: 'xlsx', content: 'both' })).toBe(false);
    expect(needsZip({ ...one, format: 'sql', content: 'both' })).toBe(false);
    const whole = {
      ...defaultForm(sourceFromPath(db)),
      format: 'json' as const,
      content: 'data' as const,
    };
    expect(needsZip(whole)).toBe(true);
    const query = { ...defaultForm(sourceFromPath(undefined, 'x')), format: 'csv' as const };
    expect(needsZip(query)).toBe(false);
  });

  it('names the file after the format', () => {
    const f = {
      ...defaultForm(sourceFromPath(table('orders'))),
      format: 'csv' as const,
      content: 'data' as const,
    };
    expect(suggestedName(f, 'orders')).toBe('orders.csv');
    expect(suggestedName({ ...f, content: 'both' }, 'orders')).toBe('orders.zip');
    expect(suggestedName({ ...f, format: 'json', json: { layout: 'ndjson' } }, 'a/b')).toBe(
      'a_b.ndjson',
    );
    expect(suggestedName({ ...f, format: 'sql', content: 'schema' }, 'x')).toBe('x-schema.sql');
    expect(suggestedName(f, '')).toBe('export.csv');
    expect(extensionFor({ ...f, format: 'xlsx' })).toBe('xlsx');
    const names = { zip: 'ZIP', csv: 'CSV' };
    expect(saveFilters(f, names)).toEqual([{ name: 'CSV', extensions: ['csv'] }]);
    expect(saveFilters({ ...f, content: 'both' }, names)).toEqual([
      { name: 'ZIP', extensions: ['zip'] },
    ]);
  });

  it('offers SQL only for SQL databases', () => {
    expect(formatsFor({ name: 'postgres' })).toContain('sql');
    expect(formatsFor({ name: 'mongoShell' })).not.toContain('sql');
    expect(formatsFor({ name: 'redisCli' })).toEqual(['csv', 'xlsx', 'json']);
  });
});

describe('validate', () => {
  it('lists what blocks the export', () => {
    const f = defaultForm({ kind: 'tables', paths: [] });
    expect(validate(f)).toEqual(['noTables', 'noPath']);
    const q = defaultForm(sourceFromPath(undefined, '  '));
    expect(validate({ ...q, path: '/x' })).toEqual(['noQuery']);
    const ok = { ...defaultForm(sourceFromPath(table('a'))), path: '/x', format: 'csv' as const };
    expect(validate(ok)).toEqual([]);
    expect(validate({ ...ok, csv: { ...ok.csv, delimiter: ';;' } })).toEqual(['delimiterLength']);
    expect(validate({ ...ok, csv: { ...ok.csv, delimiter: '"' } })).toEqual([
      'delimiterEqualsQuote',
    ]);
  });
});
