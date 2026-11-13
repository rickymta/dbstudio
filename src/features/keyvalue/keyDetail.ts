import type { ObjectNode, ObjectPath } from '@/lib/types';

/** Prefix of a key-name segment that holds base64 bytes (see `encode_key_name` in the core). */
export const BINARY_KEY_PREFIX = '\u0000b64:';

export interface KeyDetail {
  /** `string|hash|list|set|zset|stream|ReJSON-RL|...` */
  type: string;
  /** Remaining time to live in seconds; `null` = persistent. */
  ttlSeconds: number | null;
}

/** Parse `ObjectNode.detail` of a key node: `"<type>"` or `"<type> ttl:<seconds>"`. */
export function parseKeyDetail(detail: string | null | undefined): KeyDetail | null {
  const text = (detail ?? '').trim();
  if (!text) return null;
  const [type = '', ...rest] = text.split(/\s+/);
  let ttlSeconds: number | null = null;
  for (const part of rest) {
    if (!part.startsWith('ttl:')) continue;
    const n = Number(part.slice(4));
    if (Number.isFinite(n) && n >= 0) ttlSeconds = n;
  }
  return { type, ttlSeconds };
}

export function isBinaryKeyName(name: string): boolean {
  return name.startsWith(BINARY_KEY_PREFIX);
}

/** Display form of a key name: binary names become `<binary: xxxx>` (shortened base64). */
export function displayKeyName(name: string): string {
  if (!isBinaryKeyName(name)) return name;
  const b64 = name.slice(BINARY_KEY_PREFIX.length);
  return b64.length > 12 ? `<binary: ${b64.slice(0, 12)}...>` : `<binary: ${b64}>`;
}

/** The `Database` segment path a key path lives in. */
export function databaseOfKeyPath(path: ObjectPath): ObjectPath {
  const i = path.segments.findIndex((s) => s.kind === 'database');
  return { segments: i >= 0 ? path.segments.slice(0, i + 1) : [] };
}

/** Name of the `Key` segment, used verbatim as `key` in `KeyChange`. */
export function keyNameOfPath(path: ObjectPath): string {
  const last = path.segments[path.segments.length - 1];
  return last?.name ?? '';
}

/** Compact human duration for a TTL: `3d 4h`, `1h 2m`, `45s`. */
export function formatDuration(totalSeconds: number): string {
  const s = Math.max(0, Math.floor(totalSeconds));
  const d = Math.floor(s / 86400);
  const h = Math.floor((s % 86400) / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  if (d > 0) return h > 0 ? `${d}d ${h}h` : `${d}d`;
  if (h > 0) return m > 0 ? `${h}h ${m}m` : `${h}h`;
  if (m > 0) return sec > 0 ? `${m}m ${sec}s` : `${m}m`;
  return `${sec}s`;
}

/** Pattern suggested for a node: everything below a prefix, or every key of a database. */
export function defaultDeletePattern(node: Pick<ObjectNode, 'kind' | 'name'>): string {
  return node.kind === 'keyPrefix' ? `${node.name}*` : '*';
}
