// Export of the plan diagram as SVG and PNG. The SVG is written directly (real <text>
// elements, no foreignObject, so it renders the same in every viewer and when rasterised);
// the PNG is that SVG drawn on a canvas. Colors come from the active theme.

import { ipc } from '@/lib/ipc';
import type { PlanNode, PlanTree, Severity } from '@/lib/types';
import { NODE_HEIGHT, NODE_WIDTH, type PlanLayout } from './elkLayout';
import {
  formatCount,
  formatDuration,
  formatPercent,
  maxSeverity,
  metricOf,
  nodeShare,
  objectLabel,
  warningsByNode,
} from './planModel';

export interface ExportColors {
  background: string;
  surface: string;
  surfaceMuted: string;
  foreground: string;
  muted: string;
  border: string;
  accent: string;
  warning: string;
  warningSoft: string;
  danger: string;
}

/** Theme colors as they are right now (light or dark). */
export function themeColors(): ExportColors {
  const css = getComputedStyle(document.documentElement);
  const v = (name: string, fallback: string) => css.getPropertyValue(name).trim() || fallback;
  return {
    background: v('--background', '#ffffff'),
    surface: v('--surface', '#ffffff'),
    surfaceMuted: v('--surface-muted', '#f3f4f6'),
    foreground: v('--foreground', '#111827'),
    muted: v('--muted-foreground', '#6b7280'),
    border: v('--border', '#d1d5db'),
    accent: v('--accent', '#4f46e5'),
    warning: v('--warning', '#b45309'),
    warningSoft: v('--warning-soft', '#fef3c7'),
    danger: v('--danger', '#dc2626'),
  };
}

const MARGIN = 24;
const FONT = "system-ui, -apple-system, 'Segoe UI', Roboto, sans-serif";

function esc(s: string): string {
  return s
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}

/** Cut `s` to about `max` characters (the card has a fixed width). */
function clip(s: string, max: number): string {
  return s.length > max ? `${s.slice(0, max - 1)}…` : s;
}

function strokeOf(sev: Severity | null, c: ExportColors): string {
  return sev === 'critical' ? c.danger : sev === 'warning' ? c.warning : c.border;
}

/** The whole plan (every node of the tree, whatever is collapsed on screen) as an SVG document. */
export function planToSvg(tree: PlanTree, layout: PlanLayout, colors: ExportColors): string {
  const width = layout.width + MARGIN * 2;
  const height = layout.height + MARGIN * 2;
  const hot = new Set(tree.hotPath);
  const warnings = warningsByNode(tree);
  const metric = metricOf(tree);
  const parts: string[] = [];
  parts.push(
    `<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${height}" viewBox="0 0 ${width} ${height}" font-family="${esc(FONT)}">`,
    `<rect width="${width}" height="${height}" fill="${esc(colors.background)}"/>`,
    `<g transform="translate(${MARGIN} ${MARGIN})">`,
  );
  const edges: string[] = [];
  const cards: string[] = [];
  const walk = (n: PlanNode) => {
    const p = layout.positions.get(n.id);
    if (!p) return;
    for (const c of n.children) {
      const q = layout.positions.get(c.id);
      if (!q) continue;
      const x1 = p.x + NODE_WIDTH;
      const y1 = p.y + NODE_HEIGHT / 2;
      const x2 = q.x;
      const y2 = q.y + NODE_HEIGHT / 2;
      const mid = (x1 + x2) / 2;
      const onHot = hot.has(n.id) && hot.has(c.id);
      edges.push(
        `<path d="M${x1} ${y1} C${mid} ${y1} ${mid} ${y2} ${x2} ${y2}" fill="none" stroke="${esc(onHot ? colors.accent : colors.border)}" stroke-width="${onHot ? 3 : 1.5}"/>`,
      );
      walk(c);
    }
    const sev = maxSeverity(warnings.get(n.id));
    const isHot = hot.has(n.id);
    const share = nodeShare(tree, n);
    const rows = n.actualRows ?? n.estRows;
    const right = [
      metric === 'time' ? formatDuration(n.selfTimeMs) : '',
      share != null ? formatPercent(share) : '',
    ]
      .filter((x) => x && x !== '-')
      .join(' · ');
    const tx = p.x + 10;
    const barW = NODE_WIDTH - 20;
    cards.push(
      `<g data-node="${n.id}">`,
      `<rect x="${p.x}" y="${p.y}" width="${NODE_WIDTH}" height="${NODE_HEIGHT}" rx="6" fill="${esc(colors.surface)}" stroke="${esc(isHot ? colors.accent : strokeOf(sev, colors))}" stroke-width="${isHot ? 2.5 : 1.25}"/>`,
      `<text x="${tx}" y="${p.y + 20}" font-size="12.5" font-weight="${isHot ? 700 : 600}" fill="${esc(colors.foreground)}">${esc(clip(n.op, 30))}</text>`,
      `<text x="${tx}" y="${p.y + 37}" font-size="11" fill="${esc(colors.muted)}">${esc(clip(objectLabel(n), 38))}</text>`,
      `<text x="${tx}" y="${p.y + 54}" font-size="11" fill="${esc(colors.foreground)}">${esc(formatCount(rows))}</text>`,
      `<text x="${p.x + NODE_WIDTH - 10}" y="${p.y + 54}" font-size="11" text-anchor="end" fill="${esc(colors.muted)}">${esc(right)}</text>`,
    );
    if (share != null) {
      cards.push(
        `<rect x="${tx}" y="${p.y + 61}" width="${barW}" height="4" rx="2" fill="${esc(colors.surfaceMuted)}"/>`,
        `<rect x="${tx}" y="${p.y + 61}" width="${Math.max(2, share * barW).toFixed(1)}" height="4" rx="2" fill="${esc(colors.accent)}"/>`,
      );
    }
    cards.push('</g>');
  };
  walk(tree.root);
  parts.push(...edges, ...cards, '</g></svg>');
  return parts.join('\n');
}

export function utf8ToBase64(text: string): string {
  const bytes = new TextEncoder().encode(text);
  let bin = '';
  for (let i = 0; i < bytes.length; i += 0x8000) {
    bin += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  }
  return btoa(bin);
}

/** Rasterise an SVG document to PNG (base64) at `scale`x. */
export async function svgToPngBase64(
  svg: string,
  width: number,
  height: number,
  scale = 2,
): Promise<string> {
  const url = URL.createObjectURL(new Blob([svg], { type: 'image/svg+xml;charset=utf-8' }));
  try {
    const img = new Image();
    await new Promise<void>((resolve, reject) => {
      img.onload = () => resolve();
      img.onerror = () => reject(new Error('could not render the diagram'));
      img.src = url;
    });
    const canvas = document.createElement('canvas');
    // Browsers limit canvas size; stay well below.
    const k = Math.min(scale, 16000 / Math.max(width, height, 1));
    canvas.width = Math.max(1, Math.round(width * k));
    canvas.height = Math.max(1, Math.round(height * k));
    const ctx = canvas.getContext('2d');
    if (!ctx) throw new Error('no canvas');
    ctx.drawImage(img, 0, 0, canvas.width, canvas.height);
    const blob = await new Promise<Blob | null>((r) => canvas.toBlob(r, 'image/png'));
    if (!blob) throw new Error('PNG encoding failed');
    const bytes = new Uint8Array(await blob.arrayBuffer());
    let bin = '';
    for (let i = 0; i < bytes.length; i += 0x8000) {
      bin += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
    }
    return btoa(bin);
  } finally {
    URL.revokeObjectURL(url);
  }
}

export type ExportFormat = 'svg' | 'png';

/**
 * Save the diagram through the native Save dialog (`file_save_bytes`, the same command the
 * ERD uses). Resolves to the path, `null` when the dialog was cancelled.
 */
export async function exportPlan(
  tree: PlanTree,
  layout: PlanLayout,
  format: ExportFormat,
  baseName = 'plan',
): Promise<string | null> {
  const svg = planToSvg(tree, layout, themeColors());
  const width = layout.width + MARGIN * 2;
  const height = layout.height + MARGIN * 2;
  if (format === 'svg') {
    return ipc.fileSaveBytes(`${baseName}.svg`, utf8ToBase64(svg), [
      { name: 'SVG', extensions: ['svg'] },
    ]);
  }
  const png = await svgToPngBase64(svg, width, height);
  return ipc.fileSaveBytes(`${baseName}.png`, png, [{ name: 'PNG', extensions: ['png'] }]);
}
