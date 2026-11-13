// Tiny fake MongoDB tree for the browser mock (names only; no table data).

import { childPath, lastSegment, ROOT_PATH } from '../objectPath';
import type { ObjectKind, ObjectNode, ObjectPath, TableDetails } from '../types';
import { ipcError } from './util';

const node = (
  parent: ObjectPath,
  kind: ObjectKind,
  name: string,
  hasChildren: boolean,
  detail?: string,
): ObjectNode => ({
  path: childPath(parent, kind, name),
  kind,
  name,
  hasChildren,
  detail: detail ?? null,
});

/** Children of `path`: databases -> a collections folder -> collections. */
export function mongoChildren(path: ObjectPath): ObjectNode[] {
  const last = lastSegment(path);
  if (!last) return ['admin', 'app'].map((d) => node(ROOT_PATH, 'database', d, true));
  switch (last.kind) {
    case 'database':
      return [node(path, 'folder', 'collections', true)];
    case 'folder':
      return last.name === 'collections'
        ? ['users', 'orders'].map((c) => node(path, 'collection', c, false, '~12 docs'))
        : [];
    default:
      return [];
  }
}

export function mongoDetails(path: ObjectPath): TableDetails {
  void path;
  throw ipcError('unsupported', 'table details are not available in the browser mock');
}

export function mongoDdl(path: ObjectPath): string {
  return `-- DDL is not available for ${lastSegment(path)?.name ?? ''}`;
}
