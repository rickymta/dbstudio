import { describe, expect, it } from 'vitest';
import { generateModel, sampleModel } from '@/lib/mock/erd';
import { autoLayout } from './erdLayout';
import { PALETTES, buildSvg, bytesToBase64, renderExport } from './exportImage';
import { DEFAULT_OPTIONS, buildView, tableKey } from './model';

async function input(theme: 'light' | 'dark', n?: number) {
  const model = n ? generateModel(n) : sampleModel(true);
  const view = buildView(model, { ...DEFAULT_OPTIONS, showViews: true });
  const positions = await autoLayout({
    tables: view.tables,
    relations: view.relations,
    options: DEFAULT_OPTIONS,
  });
  return { view, positions, theme };
}

const parse = (svg: string) => new DOMParser().parseFromString(svg, 'image/svg+xml');

describe('SVG export', () => {
  it('is well-formed XML with one group per table and relation, and no foreignObject', async () => {
    const { view, positions, theme } = await input('light');
    const { svg, width, height } = buildSvg({
      tables: view.tables,
      relations: view.relations,
      positions,
      options: DEFAULT_OPTIONS,
      theme,
    });
    const doc = parse(svg);
    expect(doc.getElementsByTagName('parsererror')).toHaveLength(0);
    const root = doc.documentElement;
    expect(root.tagName).toBe('svg');
    expect(root.getAttribute('xmlns')).toBe('http://www.w3.org/2000/svg');
    expect(Number(root.getAttribute('width'))).toBe(width);
    expect(Number(root.getAttribute('height'))).toBe(height);
    expect(root.getAttribute('viewBox')).toBe(`0 0 ${width} ${height}`);
    expect(width).toBeGreaterThan(100);
    expect(doc.querySelectorAll('g[data-table]')).toHaveLength(view.tables.length);
    expect(doc.querySelectorAll('g[data-relation]')).toHaveLength(view.relations.length);
    expect(svg).not.toContain('foreignObject');
    // Only hex colors: no CSS variables and no oklch() (svg2pdf and editors cannot resolve them).
    expect(svg).not.toMatch(/var\(|oklch|rgb\(|hsl\(/);
    // Every column name is drawn as text.
    const texts = [...doc.querySelectorAll('text')].map((t) => t.textContent);
    expect(texts).toContain('film_id');
    expect(texts).toContain('public.film');
    expect(texts).toContain('PK');
    expect(texts).toContain('FK');
  });

  it('uses the dark palette for dark exports and the light one otherwise', async () => {
    const a = await input('light');
    const common = {
      tables: a.view.tables,
      relations: a.view.relations,
      positions: a.positions,
      options: DEFAULT_OPTIONS,
    };
    const light = buildSvg({ ...common, theme: 'light' }).svg;
    const dark = buildSvg({ ...common, theme: 'dark' }).svg;
    expect(light).toContain(PALETTES.light.background);
    expect(light).not.toContain(PALETTES.dark.background);
    expect(dark).toContain(PALETTES.dark.background);
    expect(dark).not.toContain(PALETTES.light.background);
    expect(parse(dark).getElementsByTagName('parsererror')).toHaveLength(0);
  });

  it('draws notation symbols, hides columns of collapsed tables and escapes text', async () => {
    const { view, positions } = await input('light');
    const film = view.tables.find((t) => t.table.name === 'film')!;
    const evil = {
      ...film,
      table: { ...film.table, name: 'a<b>&"c' },
    };
    const tables = view.tables.map((t) => (t.key === film.key ? evil : t));
    const collapsed = new Set([film.key]);
    const { svg } = buildSvg({
      tables,
      relations: view.relations,
      positions,
      collapsed,
      colors: new Map([[film.key, '#336699']]),
      options: DEFAULT_OPTIONS,
      theme: 'light',
    });
    const doc = parse(svg);
    expect(doc.getElementsByTagName('parsererror')).toHaveLength(0);
    // Zero-or-one / zero-or-many symbols come with circles.
    expect(doc.querySelectorAll('circle').length).toBeGreaterThan(0);
    const g = [...doc.querySelectorAll('g[data-table]')].find(
      (n) => n.getAttribute('data-table') === film.key,
    )!;
    expect(g).toBeTruthy();
    expect([...g.querySelectorAll('text')].map((t) => t.textContent)).toEqual(['public.a<b>&"c']);
    expect(svg).toContain('#336699');
    // An invalid custom color is ignored.
    const bad = buildSvg({
      tables: view.tables,
      relations: [],
      positions,
      colors: new Map([[film.key, 'url(javascript:1)']]),
      options: DEFAULT_OPTIONS,
      theme: 'light',
    }).svg;
    expect(bad).not.toContain('javascript');
  });

  it('exports 100 tables and an empty diagram', async () => {
    const big = await input('light', 100);
    const { svg } = buildSvg({
      tables: big.view.tables,
      relations: big.view.relations,
      positions: big.positions,
      options: DEFAULT_OPTIONS,
      theme: 'light',
    });
    const doc = parse(svg);
    expect(doc.getElementsByTagName('parsererror')).toHaveLength(0);
    expect(doc.querySelectorAll('g[data-table]')).toHaveLength(100);
    const empty = buildSvg({
      tables: [],
      relations: [],
      positions: new Map(),
      options: DEFAULT_OPTIONS,
      theme: 'dark',
    });
    expect(parse(empty.svg).getElementsByTagName('parsererror')).toHaveLength(0);
    expect(empty.width).toBeGreaterThan(0);
  });

  it('renderExport(svg) returns the UTF-8 bytes of the document', async () => {
    const a = await input('dark');
    const bytes = await renderExport(
      {
        tables: a.view.tables,
        relations: a.view.relations,
        positions: a.positions,
        options: DEFAULT_OPTIONS,
        theme: 'dark',
      },
      'svg',
    );
    const text = new TextDecoder().decode(bytes);
    expect(text.startsWith('<svg ')).toBe(true);
    expect(bytesToBase64(new TextEncoder().encode('hé'))).toBe(btoa('hÃ©'));
    expect(tableKey(a.view.tables[0]!.table.path)).toBeTruthy();
  });
});
