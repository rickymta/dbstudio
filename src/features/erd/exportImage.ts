// Export of the diagram as SVG, PNG and PDF. The SVG is generated straight from the data (never
// serialized from the DOM): no foreignObject, no CSS variables and only hex colors, so it opens
// the same in browsers, vector editors, WebKit webviews and svg2pdf. PNG and PDF are derived from
// that SVG, and the palette is chosen per theme so a dark diagram exports dark.

import {
  anchorOffset,
  boundsOf,
  edgeGeometry,
  HEADER_HEIGHT,
  nodeSize,
  ROW_HEIGHT,
} from './geometry';
import type { Rect } from './geometry';
import {
  type LayoutOptions,
  type Point,
  type ViewTable,
  relationKey,
  tableKey,
  tableLabel,
} from './model';
import { endSymbol, relationEnds } from './notation';
import type { jsPDF as JsPdfClass } from 'jspdf';
import type { svg2pdf as Svg2PdfFn } from 'svg2pdf.js';
import type { Relation } from '@/lib/types';

export type ExportTheme = 'light' | 'dark';
export type ExportFormat = 'svg' | 'png' | 'pdf';

export interface ErdPalette {
  background: string;
  nodeFill: string;
  nodeStroke: string;
  headerFill: string;
  headerText: string;
  text: string;
  muted: string;
  accent: string;
  edge: string;
  badge: string;
}

export const PALETTES: Record<ExportTheme, ErdPalette> = {
  light: {
    background: '#ffffff',
    nodeFill: '#ffffff',
    nodeStroke: '#c5ccd6',
    headerFill: '#e8edf5',
    headerText: '#1b2430',
    text: '#1b2430',
    muted: '#6b7685',
    accent: '#3b5bdb',
    edge: '#6b7685',
    badge: '#b45309',
  },
  dark: {
    background: '#14181f',
    nodeFill: '#1d232d',
    nodeStroke: '#3a4352',
    headerFill: '#2a3342',
    headerText: '#eef2f8',
    text: '#e6ebf2',
    muted: '#9aa6b8',
    accent: '#8da2fb',
    edge: '#9aa6b8',
    badge: '#f2b462',
  },
};

export interface SvgInput {
  tables: readonly ViewTable[];
  relations: readonly Relation[];
  positions: ReadonlyMap<string, Point>;
  collapsed?: ReadonlySet<string>;
  /** Custom header colors (hex) per table key. */
  colors?: ReadonlyMap<string, string>;
  options: Pick<LayoutOptions, 'showTypes'>;
  theme: ExportTheme;
}

export interface SvgResult {
  svg: string;
  width: number;
  height: number;
}

const PAD = 32;
const FONT = "system-ui, -apple-system, 'Segoe UI', Roboto, Helvetica, Arial, sans-serif";
const MONO = 'ui-monospace, SFMono-Regular, Menlo, Consolas, monospace';

const esc = (s: string) =>
  s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');

const num = (n: number) => String(Math.round(n * 100) / 100);

const truncate = (s: string, max: number) =>
  max < 2 ? '' : s.length <= max ? s : `${s.slice(0, Math.max(1, max - 1))}…`;

const HEX = /^#(?:[0-9a-f]{3}|[0-9a-f]{6})$/i;

export function buildSvg(input: SvgInput): SvgResult {
  const palette = PALETTES[input.theme];
  const collapsed = input.collapsed ?? new Set<string>();
  const rects = new Map<string, Rect>();
  for (const t of input.tables) {
    const pos = input.positions.get(t.key) ?? { x: 0, y: 0 };
    rects.set(t.key, { ...pos, ...nodeSize(t, collapsed.has(t.key), input.options) });
  }
  const edgeGeoms = input.relations.flatMap((r) => {
    const from = input.tables.find((t) => t.key === tableKey(r.fromTable));
    const to = input.tables.find((t) => t.key === tableKey(r.toTable));
    const fr = from && rects.get(from.key);
    const tr = to && rects.get(to.key);
    if (!from || !to || !fr || !tr) return [];
    const fy = fr.y + anchorOffset(from, r.fromColumns, collapsed.has(from.key), fr.height);
    const ty = tr.y + anchorOffset(to, r.toColumns, collapsed.has(to.key), tr.height);
    return [{ r, geom: edgeGeometry(fr, tr, fy, ty) }];
  });
  // Edges may leave the node rectangles (loops): include them in the bounds.
  const extra: Rect[] = edgeGeoms.flatMap(({ geom }) => [
    { x: geom.start.x + geom.startDir.x * 70 - 1, y: geom.start.y - 1, width: 2, height: 2 },
    { x: geom.end.x + geom.endDir.x * 70 - 1, y: geom.end.y - 1, width: 2, height: 2 },
  ]);
  const bounds = boundsOf([...rects.values(), ...extra], PAD) ?? {
    x: 0,
    y: 0,
    width: 2 * PAD,
    height: 2 * PAD,
  };
  const width = Math.ceil(bounds.width);
  const height = Math.ceil(bounds.height);
  const out: string[] = [];
  out.push(
    `<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${height}" viewBox="0 0 ${width} ${height}" font-family="${esc(FONT)}">`,
  );
  out.push(`<rect x="0" y="0" width="${width}" height="${height}" fill="${palette.background}"/>`);
  out.push(`<g transform="translate(${num(-bounds.x)} ${num(-bounds.y)})">`);

  out.push('<g class="edges" fill="none" stroke-linecap="round">');
  for (const { r, geom } of edgeGeoms) {
    const ends = relationEnds(r);
    out.push(
      `<g data-relation="${esc(relationKey(r))}"><path d="${geom.path}" stroke="${palette.edge}" stroke-width="1.4"/>`,
    );
    for (const [kind, p, dir] of [
      [ends.child, geom.start, geom.startDir],
      [ends.parent, geom.end, geom.endDir],
    ] as const) {
      const sym = endSymbol(kind, p.x, p.y, dir.x, dir.y);
      out.push(`<path d="${sym.d}" stroke="${palette.edge}" stroke-width="1.4"/>`);
      if (sym.circle) {
        out.push(
          `<circle cx="${num(sym.circle.cx)}" cy="${num(sym.circle.cy)}" r="${sym.circle.r}" fill="${palette.background}" stroke="${palette.edge}" stroke-width="1.4"/>`,
        );
      }
    }
    out.push('</g>');
  }
  out.push('</g>');

  out.push('<g class="tables">');
  for (const t of input.tables) {
    const rect = rects.get(t.key)!;
    const isCollapsed = collapsed.has(t.key);
    const custom = input.colors?.get(t.key);
    const header = custom && HEX.test(custom) ? custom : palette.headerFill;
    const headerText = custom && HEX.test(custom) ? '#ffffff' : palette.headerText;
    const chars = Math.floor((rect.width - 20) / 7);
    out.push(`<g data-table="${esc(t.key)}" transform="translate(${num(rect.x)} ${num(rect.y)})">`);
    out.push(
      `<rect width="${num(rect.width)}" height="${num(rect.height)}" rx="6" fill="${palette.nodeFill}" stroke="${palette.nodeStroke}"/>`,
    );
    out.push(
      `<path d="M0 ${HEADER_HEIGHT}V6a6 6 0 0 1 6-6h${num(rect.width - 12)}a6 6 0 0 1 6 6V${HEADER_HEIGHT}z" fill="${header}"/>`,
    );
    const title = truncate(tableLabel(t.table), chars - (t.table.kind === 'table' ? 0 : 5));
    out.push(
      `<text x="10" y="${HEADER_HEIGHT / 2 + 4.5}" font-size="13" font-weight="600" fill="${headerText}">${esc(title)}</text>`,
    );
    if (t.table.kind !== 'table') {
      out.push(
        `<text x="${num(rect.width - 8)}" y="${HEADER_HEIGHT / 2 + 4}" font-size="10" text-anchor="end" fill="${headerText}" opacity="0.7">view</text>`,
      );
    }
    if (!isCollapsed) {
      t.columns.forEach((c, i) => {
        const y = HEADER_HEIGHT + i * ROW_HEIGHT;
        const base = y + ROW_HEIGHT / 2 + 4;
        if (i > 0) {
          out.push(
            `<line x1="0" y1="${y}" x2="${num(rect.width)}" y2="${y}" stroke="${palette.nodeStroke}" stroke-width="0.5" opacity="0.6"/>`,
          );
        }
        const badge = c.pk ? 'PK' : c.fk ? 'FK' : '';
        if (badge) {
          out.push(
            `<text x="8" y="${base}" font-size="9" font-weight="700" font-family="${esc(MONO)}" fill="${c.pk ? palette.badge : palette.accent}">${badge}</text>`,
          );
        }
        const typeText = input.options.showTypes ? c.detail.typeName : '';
        const nameChars = Math.max(
          4,
          chars - 3 - (typeText ? Math.min(typeText.length, 18) + 2 : 0),
        );
        out.push(
          `<text x="30" y="${base}" font-size="12" fill="${c.detail.nullable ? palette.muted : palette.text}"${c.pk ? ' font-weight="600"' : ''}>${esc(truncate(c.detail.name, nameChars))}</text>`,
        );
        if (typeText) {
          out.push(
            `<text x="${num(rect.width - 8)}" y="${base}" font-size="10.5" text-anchor="end" fill="${palette.muted}">${esc(truncate(typeText, 18))}</text>`,
          );
        }
      });
    }
    out.push('</g>');
  }
  out.push('</g></g></svg>');
  return { svg: out.join(''), width, height };
}

// ---------------------------------------------------------------------------------------------
// Rasterizing and files
// ---------------------------------------------------------------------------------------------

/** Longest side of a PNG, in pixels (keeps huge schemas inside the canvas limits). */
const MAX_PNG_SIDE = 16000;

export async function svgToPngBytes(
  svg: string,
  width: number,
  height: number,
  scale = 2,
): Promise<Uint8Array> {
  const factor = Math.max(0.1, Math.min(scale, MAX_PNG_SIDE / Math.max(width, height)));
  const w = Math.max(1, Math.round(width * factor));
  const h = Math.max(1, Math.round(height * factor));
  const img = new Image();
  img.decoding = 'async';
  const loaded = new Promise<void>((resolve, reject) => {
    img.onload = () => resolve();
    img.onerror = () => reject(new Error('could not render the diagram'));
  });
  // A data URL (allowed by the app CSP) instead of a blob URL: no revoke bookkeeping.
  img.src = `data:image/svg+xml;charset=utf-8,${encodeURIComponent(svg)}`;
  await loaded;
  const canvas = document.createElement('canvas');
  canvas.width = w;
  canvas.height = h;
  const ctx = canvas.getContext('2d');
  if (!ctx) throw new Error('canvas is not available');
  ctx.drawImage(img, 0, 0, w, h);
  const blob = await new Promise<Blob | null>((resolve) => canvas.toBlob(resolve, 'image/png'));
  if (!blob) throw new Error('could not encode the PNG');
  return new Uint8Array(await blob.arrayBuffer());
}

export async function svgToPdfBytes(
  svg: string,
  width: number,
  height: number,
): Promise<Uint8Array> {
  const [jspdfModule, svg2pdfModule] = await Promise.all([import('jspdf'), import('svg2pdf.js')]);
  // The CommonJS builds expose the exports under `default`.
  const pick = <T>(mod: unknown, name: string): T => {
    const m = mod as Record<string, unknown> & { default?: Record<string, unknown> };
    return (m[name] ?? m.default?.[name]) as T;
  };
  const jsPDF = pick<typeof JsPdfClass>(jspdfModule, 'jsPDF');
  const svg2pdf = pick<typeof Svg2PdfFn>(svg2pdfModule, 'svg2pdf');
  const doc = new jsPDF({
    orientation: width >= height ? 'landscape' : 'portrait',
    unit: 'pt',
    format: [width, height],
    compress: true,
  });
  const root = new DOMParser().parseFromString(svg, 'image/svg+xml').documentElement;
  await svg2pdf(root, doc, { x: 0, y: 0, width, height });
  return new Uint8Array(doc.output('arraybuffer'));
}

export function bytesToBase64(bytes: Uint8Array): string {
  let binary = '';
  const CHUNK = 0x8000;
  for (let i = 0; i < bytes.length; i += CHUNK) {
    binary += String.fromCharCode(...bytes.subarray(i, i + CHUNK));
  }
  return btoa(binary);
}

export const FORMAT_FILTERS: Record<ExportFormat, { name: string; extensions: string[] }> = {
  svg: { name: 'SVG', extensions: ['svg'] },
  png: { name: 'PNG', extensions: ['png'] },
  pdf: { name: 'PDF', extensions: ['pdf'] },
};

/** Render `format` of the diagram and return the file bytes. */
export async function renderExport(input: SvgInput, format: ExportFormat): Promise<Uint8Array> {
  const { svg, width, height } = buildSvg(input);
  switch (format) {
    case 'svg':
      return new TextEncoder().encode(svg);
    case 'png':
      return svgToPngBytes(svg, width, height);
    case 'pdf':
      return svgToPdfBytes(svg, width, height);
  }
}
