import { afterEach, describe, expect, it, vi } from 'vitest';
import { generateModel, sampleModel } from '@/lib/mock/erd';
import { autoLayout } from './erdLayout';
import { PALETTES, buildSvg, bytesToBase64, renderExport, svgToPngBytes } from './exportImage';
import { DEFAULT_OPTIONS, buildView, tableKey } from './model';

// jsdom resolves the CommonJS build of svg2pdf.js; the app (Vite) uses the ES build.
vi.mock('svg2pdf.js', async () => {
  // @ts-expect-error the ES build ships without its own declaration file
  return await import('../../../node_modules/svg2pdf.js/dist/svg2pdf.es.min.js');
});

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

describe('PDF and PNG export', () => {
  afterEach(() => vi.restoreAllMocks());

  it('writes a valid PDF for both themes', async () => {
    // svg2pdf measures shapes with getBBox, which jsdom does not implement.
    (SVGElement.prototype as unknown as { getBBox: () => unknown }).getBBox = () => ({
      x: 0,
      y: 0,
      width: 100,
      height: 20,
    });
    const a = await input('light');
    for (const theme of ['light', 'dark'] as const) {
      const bytes = await renderExport(
        {
          tables: a.view.tables,
          relations: a.view.relations,
          positions: a.positions,
          options: DEFAULT_OPTIONS,
          theme,
        },
        'pdf',
      );
      const text = new TextDecoder('latin1').decode(bytes);
      expect(text.startsWith('%PDF-')).toBe(true);
      expect(text.trimEnd().endsWith('%%EOF')).toBe(true);
      expect(bytes.length).toBeGreaterThan(2000);
    }
  });

  it('rasterizes the SVG to PNG bytes within the canvas limits', async () => {
    const drawn: number[] = [];
    class FakeImage {
      onload: (() => void) | null = null;
      onerror: (() => void) | null = null;
      decoding = '';
      set src(value: string) {
        expect(value.startsWith('data:image/svg+xml;charset=utf-8,')).toBe(true);
        queueMicrotask(() => this.onload?.());
      }
    }
    vi.stubGlobal('Image', FakeImage);
    vi.spyOn(HTMLCanvasElement.prototype, 'getContext').mockImplementation(
      () =>
        ({
          drawImage: (_i: unknown, _x: number, _y: number, w: number, h: number) =>
            drawn.push(w, h),
        }) as never,
    );
    vi.spyOn(HTMLCanvasElement.prototype, 'toBlob').mockImplementation((cb) =>
      cb(new Blob([new Uint8Array([0x89, 0x50, 0x4e, 0x47])], { type: 'image/png' })),
    );
    const bytes = await svgToPngBytes('<svg xmlns="http://www.w3.org/2000/svg"/>', 400, 300, 2);
    expect(Array.from(bytes)).toEqual([0x89, 0x50, 0x4e, 0x47]);
    expect(drawn).toEqual([800, 600]);
    // A huge diagram is scaled down to fit.
    drawn.length = 0;
    await svgToPngBytes('<svg xmlns="http://www.w3.org/2000/svg"/>', 40000, 1000, 2);
    expect(drawn[0]).toBeLessThanOrEqual(16000);
    vi.unstubAllGlobals();
  });
});
