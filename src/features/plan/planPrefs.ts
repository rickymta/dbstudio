// Per-user layout preferences of the plan tab (kept in localStorage; every access is guarded
// because storage can be unavailable).

import { useCallback, useState } from 'react';

export type GraphDirection = 'RIGHT' | 'DOWN';

const KEY_DIRECTION = 'dbstudio.plan.direction';
const KEY_INSPECTOR = 'dbstudio.plan.inspector';
const KEY_INSPECTOR_WIDTH = 'dbstudio.plan.inspectorWidth';

export const INSPECTOR_MIN = 240;
export const INSPECTOR_MAX = 480;
export const INSPECTOR_DEFAULT = 300;

function read(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function write(key: string, value: string): void {
  try {
    localStorage.setItem(key, value);
  } catch {
    /* storage unavailable */
  }
}

export function clampInspectorWidth(w: number): number {
  return Math.max(INSPECTOR_MIN, Math.min(INSPECTOR_MAX, Math.round(w)));
}

export function readDirection(): GraphDirection {
  return read(KEY_DIRECTION) === 'DOWN' ? 'DOWN' : 'RIGHT';
}

export function useGraphDirection(): [GraphDirection, (d: GraphDirection) => void] {
  const [direction, set] = useState<GraphDirection>(readDirection);
  const update = useCallback((d: GraphDirection) => {
    write(KEY_DIRECTION, d);
    set(d);
  }, []);
  return [direction, update];
}

/** The user's explicit choice for the inspector; `null` lets the available width decide. */
export type InspectorChoice = 'open' | 'collapsed' | null;

export function useInspectorPrefs() {
  const [choice, setChoice] = useState<InspectorChoice>(() => {
    const v = read(KEY_INSPECTOR);
    return v === 'open' || v === 'collapsed' ? v : null;
  });
  const [width, setWidth] = useState<number>(() => {
    const n = Number(read(KEY_INSPECTOR_WIDTH));
    return Number.isFinite(n) && n > 0 ? clampInspectorWidth(n) : INSPECTOR_DEFAULT;
  });
  const choose = useCallback((c: 'open' | 'collapsed') => {
    write(KEY_INSPECTOR, c);
    setChoice(c);
  }, []);
  const resize = useCallback((w: number, persist = true) => {
    const next = clampInspectorWidth(w);
    if (persist) write(KEY_INSPECTOR_WIDTH, String(next));
    setWidth(next);
  }, []);
  return { choice, choose, width, resize };
}
