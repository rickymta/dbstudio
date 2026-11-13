// Readers for `BackupInspection` (display fields of the header and the preamble).

import type { BackupInspection } from '@/lib/types';

export interface InspectionView {
  /** Scalar header fields, in order, for display. */
  headerFields: { key: string; value: string }[];
  /** Source database name. */
  sourceDatabase: string | null;
  /** Names of the objects listed in the preamble. */
  objects: string[];
  /** The backup is one integrated SQL script: it can only be restored as a whole. */
  integrated: boolean;
  /** Remaining preamble data (supported scope etc.) pretty-printed, or `null`. */
  extra: string | null;
  /** Content is readable (not an encrypted file still locked). */
  readable: boolean;
}

function scalar(v: unknown): string | null {
  if (typeof v === 'string' || typeof v === 'number' || typeof v === 'boolean') return String(v);
  return null;
}

export function readInspection(info: BackupInspection): InspectionView {
  const header = info.header;
  const preamble = info.preamble;
  const fields: [string, string | null | undefined][] = [
    ['database', preamble?.database],
    ['source', preamble ? `${preamble.source.kind} ${preamble.source.host}`.trim() : null],
    ['serverVersion', preamble?.source.serverVersion],
    ['createdAt', preamble?.createdAt ?? header.createdAt],
    ['engine', preamble?.engine],
    ['compression', header.compression],
    ['encryption', header.encryption ? header.encryption.alg : null],
  ];
  const headerFields = fields.flatMap(([key, v]) => {
    const value = scalar(v);
    return value ? [{ key, value }] : [];
  });
  const objects = (preamble?.objects ?? []).map((o) => o.name);
  const rest = preamble
    ? Object.fromEntries(
        Object.entries(preamble).filter(([k]) => !['objects', 'database', 'source'].includes(k)),
      )
    : {};
  return {
    headerFields,
    sourceDatabase: preamble?.database ?? null,
    objects,
    integrated: preamble?.engine === 'logical',
    extra: Object.keys(rest).length ? JSON.stringify(rest, null, 2) : null,
    readable: preamble !== null,
  };
}
