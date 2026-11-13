import { describe, expect, it } from 'vitest';
import type { ImportPreview, ObjectPath } from '@/lib/types';
import {
  autoMapping,
  buildImportSpec,
  columnName,
  defaultImportForm,
  importFormat,
  kindFromFile,
  layoutFromFile,
  newColumnsFromPreview,
  typeForKind,
  validateImport,
} from './importForm';

const ID = '00000000-0000-4000-8000-000000000001';
const parent: ObjectPath = { segments: [{ kind: 'database', name: 'app' }] };
const table: ObjectPath = { segments: [...parent.segments, { kind: 'table', name: 't' }] };

const preview: ImportPreview = {
  columns: ['Unit price', 'Name', 'name', '3rd'],
  rows: [],
  sheets: [],
  detectedDelimiter: ';',
  suggestedKinds: ['float', 'text', 'int', 'date'],
  estimatedRows: 10,
  statementsPreview: [],
};

describe('file kinds', () => {
  it('follows the extension', () => {
    expect(kindFromFile('/a/b.XLSX')).toBe('xlsx');
    expect(kindFromFile('/a/b.ndjson')).toBe('json');
    expect(kindFromFile('/a/b.sql')).toBe('sql');
    expect(kindFromFile('/a/b.tsv')).toBe('csv');
    expect(kindFromFile('/a/noext')).toBe('csv');
    expect(layoutFromFile('/a/b.jsonl')).toBe('ndjson');
    expect(layoutFromFile('/a/b.json')).toBe('array');
  });
});

describe('new table proposal', () => {
  it('makes safe unique column names and types from the sampled kinds', () => {
    expect(columnName(' Unit price ', 0)).toBe('unit_price');
    expect(columnName('3rd', 3)).toBe('col_4');
    expect(columnName('???', 1)).toBe('col_2');
    const cols = newColumnsFromPreview(preview, { name: 'postgres' });
    expect(cols.map((c) => c.name)).toEqual(['unit_price', 'name', 'name_3', 'col_4']);
    expect(cols.map((c) => c.typeName)).toEqual(['double precision', 'text', 'bigint', 'date']);
    expect(typeForKind('dateTime', { name: 'mySql', mariadb: false, backslashEscapes: true })).toBe(
      'datetime(6)',
    );
    expect(typeForKind('bool', { name: 'msSql' })).toBe('bit');
    expect(typeForKind('text', { name: 'oracle' })).toBe('clob');
    expect(typeForKind('text', { name: 'opaque' })).toBe('text');
  });
});

describe('autoMapping', () => {
  it('maps equal names once, case-insensitively', () => {
    expect(autoMapping(['ID', 'Name', 'name', 'zzz'], ['id', 'name'])).toEqual([
      { source: 'ID', target: 'id', transform: 'none' },
      { source: 'Name', target: 'name', transform: 'none' },
      { source: 'name', target: null, transform: 'none' },
      { source: 'zzz', target: null, transform: 'none' },
    ]);
  });
});

describe('buildImportSpec', () => {
  it('existing table: mapped columns only, conflict and batch mode', () => {
    const form = { ...defaultImportForm(table), file: '/in.csv' };
    form.mapping = autoMapping(['id', 'x'], ['id']);
    form.onConflict = 'skip';
    form.batchRows = 100;
    form.maxErrors = 5;
    const spec = buildImportSpec(ID, form, ['id', 'x'], parent);
    expect(spec.target).toEqual({ kind: 'existing', table });
    expect(spec.mapping).toEqual([{ source: 'id', target: 'id', transform: 'none' }]);
    expect(spec.onConflict).toBe('skip');
    expect(spec.mode).toEqual({ kind: 'batches', rows: 100 });
    expect(spec.maxErrors).toBe(5);
    expect(spec.format).toMatchObject({ kind: 'csv', delimiter: ',', header: true });
  });

  it('new table: columns come from the editor, mapping by position', () => {
    const form = { ...defaultImportForm(undefined), file: '/in.csv', newName: ' t2 ' };
    form.newColumns = newColumnsFromPreview(preview, { name: 'postgres' });
    form.mode = 'single';
    const spec = buildImportSpec(ID, form, preview.columns, parent);
    expect(spec.target).toMatchObject({ kind: 'new', parent, name: 't2' });
    expect(spec.mapping?.[0]).toEqual({ source: 'Unit price', target: 'unit_price' });
    expect(spec.onConflict).toBeNull();
    expect(spec.mode).toEqual({ kind: 'singleTransaction' });
  });

  it('SQL script: script target, no mapping', () => {
    const form = { ...defaultImportForm(undefined), file: '/d.sql', kind: 'sql' as const };
    form.sql = { continueOnError: true };
    const spec = buildImportSpec(ID, form, [], null);
    expect(spec.target).toEqual({ kind: 'script', database: null });
    expect(spec.format).toEqual({ kind: 'sql', continueOnError: true });
    expect(spec.mapping).toBeUndefined();
    expect(importFormat(form)).toEqual({ kind: 'sql', continueOnError: true });
  });
});

describe('validateImport', () => {
  it('lists what blocks the import', () => {
    const base = defaultImportForm(table);
    expect(validateImport(base, parent)).toEqual(['noFile', 'noMapping']);
    const mapped = { ...base, file: '/x', mapping: autoMapping(['a', 'b'], ['a', 'b']) };
    expect(validateImport(mapped, parent)).toEqual([]);
    const dup = {
      ...mapped,
      mapping: mapped.mapping.map((m) => ({ ...m, target: 'a' })),
    };
    expect(validateImport(dup, parent)).toEqual(['duplicateTarget']);
    const fresh = { ...defaultImportForm(undefined), file: '/x' };
    expect(validateImport(fresh, null)).toEqual(['noTable', 'noName', 'noColumns']);
    expect(validateImport({ ...fresh, kind: 'sql' }, null)).toEqual([]);
    expect(validateImport({ ...fresh, kind: 'sql', file: '' }, null)).toEqual(['noFile']);
  });
});
