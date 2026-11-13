import type { ObjectKind, ObjectPath, PathSegment } from './types';

export const ROOT_PATH: ObjectPath = { segments: [] };

/** Path of a child of `parent`. */
export function childPath(parent: ObjectPath, kind: ObjectKind, name: string): ObjectPath {
  return { segments: [...parent.segments, { kind, name }] };
}

/** Parent path; the root is its own parent. */
export function parentPath(path: ObjectPath): ObjectPath {
  return { segments: path.segments.slice(0, -1) };
}

export function lastSegment(path: ObjectPath): PathSegment | undefined {
  return path.segments[path.segments.length - 1];
}

export function pathsEqual(a: ObjectPath, b: ObjectPath): boolean {
  return (
    a.segments.length === b.segments.length &&
    a.segments.every((s, i) => s.kind === b.segments[i]?.kind && s.name === b.segments[i]?.name)
  );
}

/** True when `ancestor` is a strict prefix of `path`. */
export function isAncestor(ancestor: ObjectPath, path: ObjectPath): boolean {
  return (
    ancestor.segments.length < path.segments.length &&
    pathsEqual(ancestor, { segments: path.segments.slice(0, ancestor.segments.length) })
  );
}

/**
 * Stable, unambiguous string key (for React keys, maps and tab ids). Equal paths give equal keys
 * and different paths give different keys, whatever characters the names contain.
 */
export function pathKey(path: ObjectPath): string {
  return JSON.stringify(path.segments.map((s) => [s.kind, s.name]));
}
