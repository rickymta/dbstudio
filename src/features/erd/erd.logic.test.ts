import { describe, expect, it } from 'vitest';
import { generateModel, sampleModel } from '@/lib/mock/erd';
import { diffLayout, makeSnapshot } from './diff';
import { autoLayout, gridLayout } from './erdLayout';
import { boundsOf, edgeGeometry, nodeSize, pickSides } from './geometry';
import {
  DEFAULT_OPTIONS,
  buildView,
  parseLayoutTables,
  parseOptions,
  parseSnapshot,
  parseViewport,
  tableKey,
} from './model';
import { endSymbol, relationEnds } from './notation';
import { matchTables, neighborhood } from './selection';

const key = (model: ReturnType<typeof sampleModel>, name: string) =>
  tableKey(model.tables.find((t) => t.name === name)!.path);

describe('buildView', () => {
  it('derives pk/fk/unique flags and honours the keys-only option', () => {
    const model = sampleModel();
    const all = buildView(model, { ...DEFAULT_OPTIONS, showViews: true });
    const film = all.byKey.get(key(model, 'film'))!;
    expect(film.columns).toHaveLength(7);
    expect(film.columns.find((c) => c.detail.name === 'film_id')).toMatchObject({ pk: true });
    expect(film.columns.find((c) => c.detail.name === 'language_id')).toMatchObject({ fk: true });
    expect(all.tables.some((t) => t.table.kind === 'view')).toBe(true);

    const keys = buildView(model, { ...DEFAULT_OPTIONS, columns: 'keys', showViews: false });
    const kfilm = keys.byKey.get(key(model, 'film'))!;
    expect(kfilm.columns.map((c) => c.detail.name)).toEqual([
      'film_id',
      'language_id',
      'original_language_id',
    ]);
    expect(keys.tables.some((t) => t.table.kind === 'view')).toBe(false);
  });

  it('drops relations whose end is hidden', () => {
    const model = sampleModel();
    const view = buildView(model, DEFAULT_OPTIONS);
    expect(view.relations.length).toBe(model.relations.length);
    const hidden = { ...model, tables: model.tables.filter((t) => t.name !== 'language') };
    const v2 = buildView(hidden, DEFAULT_OPTIONS);
    expect(v2.relations.every((r) => r.toTable.segments.at(-1)!.name !== 'language')).toBe(true);
  });
});

describe('persisted layout parsing', () => {
  it('is lenient with bad data', () => {
    const path = sampleModel().tables[0]!.path;
    expect(parseLayoutTables(null)).toEqual([]);
    expect(
      parseLayoutTables([{ path, x: 1, y: 2, collapsed: true, color: '#fff' }, { x: 1 }, 7]),
    ).toEqual([{ path, x: 1, y: 2, collapsed: true, color: '#fff' }]);
    expect(parseLayoutTables([{ path, x: 'a' }])[0]).toMatchObject({
      x: 0,
      y: 0,
      collapsed: false,
    });
    expect(parseOptions(undefined)).toEqual(DEFAULT_OPTIONS);
    expect(parseOptions({ columns: 'keys', showTypes: false }).columns).toBe('keys');
    expect(parseViewport({ x: 1, y: 2, zoom: 0.5 })).toEqual({ x: 1, y: 2, zoom: 0.5 });
    expect(parseViewport({ x: 1, y: 2, zoom: 0 })).toBeNull();
    expect(parseViewport(null)).toBeNull();
    expect(parseSnapshot({ a: [['x', 'int'], 3], b: 'no' })).toEqual({ a: [['x', 'int']] });
  });
});

describe('selection', () => {
  it('"table X + 1 level" and deeper levels', () => {
    const model = sampleModel();
    const film = key(model, 'film');
    const one = neighborhood(model.relations, film, 1);
    const names = (s: Set<string>) =>
      model.tables
        .filter((t) => s.has(tableKey(t.path)))
        .map((t) => t.name)
        .sort();
    expect(names(one)).toEqual(
      ['film', 'film_actor', 'film_category', 'film_text', 'inventory', 'language'].sort(),
    );
    const two = neighborhood(model.relations, film, 2);
    expect(two.size).toBeGreaterThan(one.size);
    expect(names(two)).toContain('actor');
    expect(neighborhood(model.relations, film, 0)).toEqual(new Set([film]));
    // A table without relations stays alone; unknown keys give themselves.
    expect(neighborhood(model.relations, key(model, 'film_list'), 3).size).toBe(1);
    expect(neighborhood([], 'x', 5)).toEqual(new Set(['x']));
  });

  it('matches tables by substring', () => {
    const tables = [
      { key: '1', label: 'public.film' },
      { key: '2', label: 'public.film_actor' },
      { key: '3', label: 'public.Customer' },
    ];
    expect(matchTables(tables, 'FILM').map((t) => t.key)).toEqual(['1', '2']);
    expect(matchTables(tables, 'customer')).toHaveLength(1);
    expect(matchTables(tables, '  ')).toEqual([]);
  });
});

describe('diff', () => {
  it('marks missing, added and changed tables', () => {
    const model = sampleModel();
    const view = buildView(model, DEFAULT_OPTIONS);
    const snapshot = makeSnapshot(model.tables);
    const layoutTables = view.tables.map((t, i) => ({
      path: t.table.path,
      x: i,
      y: i,
      collapsed: false,
    }));
    expect(diffLayout(layoutTables, snapshot, view.tables)).toEqual({
      missing: [],
      added: [],
      changed: [],
    });

    // `country` dropped, `film.length` retyped, a new table appeared.
    const ghost = { path: { segments: [{ kind: 'table' as const, name: 'gone' }] } };
    const current = model.tables
      .filter((t) => t.name !== 'country')
      .map((t) =>
        t.name === 'film'
          ? {
              ...t,
              columns: t.columns.map((c) =>
                c.name === 'length' ? { ...c, typeName: 'integer' } : c,
              ),
            }
          : t,
      );
    const extra = {
      ...model.tables[0]!,
      name: 'brand_new',
      path: { segments: [{ kind: 'table' as const, name: 'brand_new' }] },
    };
    const v2 = buildView({ ...model, tables: [...current, extra] }, DEFAULT_OPTIONS);
    const d = diffLayout(
      [...layoutTables, { ...ghost, x: 0, y: 0, collapsed: false }],
      snapshot,
      v2.tables,
    );
    expect(d.missing.map((m) => m.path.segments.at(-1)!.name).sort()).toEqual(['country', 'gone']);
    expect(d.added).toEqual([tableKey(extra.path)]);
    expect(d.changed).toEqual([key(model, 'film')]);
  });
});

describe('notation', () => {
  it('maps optionality and 1-1 to the right symbols', () => {
    expect(relationEnds({ fromOptional: false, oneToOne: false })).toEqual({
      parent: 'one',
      child: 'zeroMany',
    });
    expect(relationEnds({ fromOptional: true, oneToOne: false })).toEqual({
      parent: 'zeroOne',
      child: 'zeroMany',
    });
    expect(relationEnds({ fromOptional: false, oneToOne: true })).toEqual({
      parent: 'one',
      child: 'zeroOne',
    });
  });

  it('draws bars, prongs and circles away from the table', () => {
    const one = endSymbol('one', 100, 50, 1, 0);
    expect(one.circle).toBeNull();
    expect(one.d).toBe('M108 56L108 44M113 56L113 44');
    const zo = endSymbol('zeroOne', 100, 50, -1, 0);
    expect(zo.circle).toEqual({ cx: 84, cy: 50, r: 4 });
    const zm = endSymbol('zeroMany', 0, 0, 1, 0);
    expect(zm.d).toBe('M12 0L0 6M12 0L0 -6M12 0L0 0');
    expect(zm.circle).toEqual({ cx: 18, cy: 0, r: 4 });
  });
});

describe('geometry', () => {
  it('picks facing sides and builds a connected path', () => {
    const a = { x: 0, y: 0, width: 100, height: 60 };
    const b = { x: 300, y: 100, width: 100, height: 60 };
    expect(pickSides(a, b)).toEqual(['r', 'l']);
    expect(pickSides(b, a)).toEqual(['l', 'r']);
    expect(pickSides(a, { ...a, y: 200 })).toEqual(['r', 'r']);
    const g = edgeGeometry(a, b, 30, 120);
    expect(g.start).toEqual({ x: 100, y: 30 });
    expect(g.end).toEqual({ x: 300, y: 120 });
    expect(g.startDir).toEqual({ x: 1, y: 0 });
    expect(g.endDir).toEqual({ x: -1, y: 0 });
    expect(g.path.startsWith('M100 30C')).toBe(true);
    expect(g.path.endsWith('300 120')).toBe(true);
    expect(g.mid.x).toBeCloseTo(200, 5);
    expect(boundsOf([a, b], 10)).toEqual({ x: -10, y: -10, width: 420, height: 180 });
    expect(boundsOf([])).toBeNull();
  });

  it('sizes nodes from their content', () => {
    const view = buildView(sampleModel(), DEFAULT_OPTIONS);
    const film = view.tables.find((t) => t.table.name === 'film')!;
    const full = nodeSize(film, false, { showTypes: true });
    const collapsed = nodeSize(film, true, { showTypes: true });
    expect(full.height).toBeGreaterThan(collapsed.height);
    expect(full.width).toBeGreaterThanOrEqual(170);
    expect(nodeSize(film, false, { showTypes: false }).width).toBeLessThanOrEqual(full.width);
  });
});

describe('auto layout', () => {
  it('lays out 100 tables in under 3 seconds without overlaps', async () => {
    const model = generateModel(100);
    const view = buildView(model, DEFAULT_OPTIONS);
    const started = performance.now();
    const pos = await autoLayout({
      tables: view.tables,
      relations: view.relations,
      options: DEFAULT_OPTIONS,
    });
    const elapsed = performance.now() - started;
    expect(pos.size).toBe(100);
    expect(elapsed).toBeLessThan(3000);
    // No two nodes overlap.
    const rects = view.tables.map((t) => ({
      ...pos.get(t.key)!,
      ...nodeSize(t, false, DEFAULT_OPTIONS),
    }));
    for (let i = 0; i < rects.length; i++) {
      for (let j = i + 1; j < rects.length; j++) {
        const a = rects[i]!;
        const b = rects[j]!;
        const overlap =
          a.x < b.x + b.width &&
          b.x < a.x + a.width &&
          a.y < b.y + b.height &&
          b.y < a.y + a.height;
        expect(overlap).toBe(false);
      }
    }
  }, 20_000);

  it('lays out the sample schema left to right (parents before children)', async () => {
    const model = sampleModel(false);
    const view = buildView(model, DEFAULT_OPTIONS);
    const pos = await autoLayout({
      tables: view.tables,
      relations: view.relations,
      options: DEFAULT_OPTIONS,
    });
    expect(pos.size).toBe(view.tables.length);
    const x = (n: string) => pos.get(key(model, n))!.x;
    // `film` references `language`: with edges pointing from child to parent, ELK puts the
    // referencing table on the left layer.
    expect(x('film')).not.toBe(x('language'));
    expect((await autoLayout({ tables: [], relations: [], options: DEFAULT_OPTIONS })).size).toBe(
      0,
    );
  });

  it('has a grid fallback', () => {
    const view = buildView(generateModel(10), DEFAULT_OPTIONS);
    const grid = gridLayout(view.tables, DEFAULT_OPTIONS);
    expect(grid.size).toBe(10);
    expect(new Set([...grid.values()].map((p) => `${p.x},${p.y}`)).size).toBe(10);
  });
});
