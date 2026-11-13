import { afterEach, describe, expect, it } from 'vitest';
import { ipc } from '../ipc';
import type { ErdLayout, ObjectPath } from '../types';
import { generateModel, modelFromSpecs, resetMockErd, sampleModel } from './erd';

const scope: ObjectPath = { segments: [{ kind: 'database', name: 'app_dev' }] };
const layout = (name: string, connectionId = 'c1'): ErdLayout => ({
  id: crypto.randomUUID(),
  connectionId,
  name,
  scope,
  tables: [],
  options: {},
  viewport: null,
  snapshot: {},
  createdAt: '',
  updatedAt: '',
});

afterEach(() => resetMockErd());

describe('mock ERD', () => {
  it('keeps layouts in memory with unique names per connection', async () => {
    const a = await ipc.erdLayoutSave(layout('Main'));
    expect(a.createdAt).not.toBe('');
    await expect(ipc.erdLayoutSave(layout('Main'))).rejects.toMatchObject({ kind: 'other' });
    await ipc.erdLayoutSave(layout('Main', 'c2'));
    // Updating keeps the creation time.
    const again = await ipc.erdLayoutSave({ ...a, name: 'Renamed' });
    expect(again.createdAt).toBe(a.createdAt);
    expect((await ipc.erdLayoutsList('c1')).map((l) => l.name)).toEqual(['Renamed']);
    expect((await ipc.erdLayoutGet(a.id)).name).toBe('Renamed');
    await ipc.erdLayoutDelete(a.id);
    await expect(ipc.erdLayoutGet(a.id)).rejects.toMatchObject({ kind: 'notFound' });
    await expect(ipc.erdLayoutDelete(a.id)).rejects.toMatchObject({ kind: 'notFound' });
  });

  it('builds relations with the optional / 1-1 flags', () => {
    const m = sampleModel();
    const rel = (from: string, col: string) =>
      m.relations.find(
        (r) => r.fromTable.segments.at(-1)!.name === from && r.fromColumns[0] === col,
      )!;
    expect(rel('film', 'original_language_id')).toMatchObject({
      fromOptional: true,
      oneToOne: false,
    });
    expect(rel('film', 'language_id')).toMatchObject({ fromOptional: false, oneToOne: false });
    expect(rel('film_text', 'film_id')).toMatchObject({ oneToOne: true });
    expect(rel('store', 'manager_staff_id')).toMatchObject({ oneToOne: true });
    expect(rel('film_actor', 'film_id').oneToOne).toBe(false);
    expect(sampleModel(false).tables.some((t) => t.kind === 'view')).toBe(false);
  });

  it('generates large schemas', () => {
    const m = generateModel(100);
    expect(m.tables).toHaveLength(100);
    expect(m.relations.length).toBeGreaterThanOrEqual(99);
    expect(
      modelFromSpecs([], { kind: 'mysql', database: 'd', schema: null }, scope, {}).tables,
    ).toEqual([]);
  });
});
