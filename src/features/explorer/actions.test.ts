import { describe, expect, it } from 'vitest';
import { mockDrivers } from '@/lib/mock';
import { childPath, ROOT_PATH } from '@/lib/objectPath';
import type { DbKind, ObjectKind, ObjectNode } from '@/lib/types';
import { explorerActions, scriptTemplates } from './actions';

const node = (kind: ObjectKind): ObjectNode => ({
  path: childPath(ROOT_PATH, kind, 'x'),
  kind,
  name: 'x',
  hasChildren: false,
});
const driver = (kind: DbKind) => mockDrivers.find((d) => d.kind === kind)!;
const ids = (n: ObjectNode, kind: DbKind) => explorerActions(n, driver(kind)).map((a) => a.id);

describe('explorerActions', () => {
  it('offers editor, diagram, export and backup on a PostgreSQL database', () => {
    expect(ids(node('database'), 'postgres')).toEqual([
      'openEditor',
      'showErd',
      'exportData',
      'backup',
    ]);
  });

  it('limits actions by node kind and capabilities', () => {
    expect(ids(node('table'), 'postgres')).toEqual(['openEditor', 'exportData']);
    expect(ids(node('column'), 'postgres')).toEqual([]);
    expect(ids(node('key'), 'redis')).toEqual([]);
    expect(ids(node('database'), 'cassandra')).toEqual([]);
  });

  it('labels actions with translation keys of the explorer namespace', () => {
    for (const a of explorerActions(node('database'), driver('postgres'))) {
      expect(a.labelKey).toBe(`explorer:action.${a.id}`);
    }
  });
});

describe('scriptTemplates', () => {
  it('offers the four templates on tables and collections, SELECT on views', () => {
    expect(scriptTemplates(node('table'), driver('postgres'))).toEqual([
      'select',
      'insert',
      'update',
      'delete',
    ]);
    expect(scriptTemplates(node('collection'), driver('mongodb'))).toHaveLength(4);
    expect(scriptTemplates(node('view'), driver('postgres'))).toEqual(['select']);
    expect(scriptTemplates(node('materializedView'), driver('postgres'))).toEqual(['select']);
  });

  it('offers none for key-value stores and non-table nodes', () => {
    expect(scriptTemplates(node('key'), driver('redis'))).toEqual([]);
    expect(scriptTemplates(node('table'), driver('redis'))).toEqual([]);
    expect(scriptTemplates(node('database'), driver('postgres'))).toEqual([]);
  });
});
