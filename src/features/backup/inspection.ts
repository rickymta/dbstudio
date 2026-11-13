// Tolerant readers for `BackupInspection.header` / `preamble` (typed `unknown` in the contract).

import type { BackupInspection } from '@/lib/types';

function isRecord(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

export interface InspectionView {
  /** Scalar header fields, in order, for display. */
  headerFields: { key: string; value: string }[];
  /** Source database name when the header carries one. */
  sourceDatabase: string | null;
  /** Names of the objects listed in the preamble. */
  objects: string[];
  /** Remaining preamble data (supported scope etc.) pretty-printed, or `null`. */
  extra: string | null;
  /** Content is readable (not an encrypted file still locked). */
  readable: boolean;
}

function objectName(o: unknown): string | null {
  if (typeof o === 'string') return o;
  if (isRecord(o)) {
    for (const k of ['name', 'path', 'qualifiedName']) {
      const v = o[k];
      if (typeof v === 'string') return v;
    }
  }
  return null;
}

function scalar(v: unknown): string | null {
  if (typeof v === 'string' || typeof v === 'number' || typeof v === 'boolean') return String(v);
  return null;
}

export function readInspection(info: BackupInspection): InspectionView {
  const header = isRecord(info.header) ? info.header : null;
  const preamble = isRecord(info.preamble) ? info.preamble : null;
  const headerFields = header
    ? Object.entries(header).flatMap(([key, v]) => {
        const s = scalar(v);
        return s === null ? [] : [{ key, value: s }];
      })
    : [];
  const rawObjects = preamble?.objects;
  const objects = Array.isArray(rawObjects)
    ? rawObjects.map(objectName).filter((n): n is string => n !== null)
    : [];
  const rest = preamble
    ? Object.fromEntries(Object.entries(preamble).filter(([k]) => k !== 'objects'))
    : {};
  const db = header?.database ?? header?.sourceDatabase;
  return {
    headerFields,
    sourceDatabase: typeof db === 'string' ? db : null,
    objects,
    extra: Object.keys(rest).length ? JSON.stringify(rest, null, 2) : null,
    readable: header !== null || preamble !== null,
  };
}
