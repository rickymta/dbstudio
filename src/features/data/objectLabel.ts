import type { ObjectPath } from '@/lib/types';

/** Path segments worth showing (folders like "Tables" are navigation only). */
export function displaySegments(path: ObjectPath): string[] {
  return path.segments.filter((s) => s.kind !== 'folder').map((s) => s.name);
}
