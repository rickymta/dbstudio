// Tiny fake Redis tree for the browser mock (names only; no table data).

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

/** Children of `path`: databases -> virtual key-prefix folders and keys. */
export function redisChildren(path: ObjectPath): ObjectNode[] {
  const last = lastSegment(path);
  if (!last) return ['0', '1'].map((d) => node(ROOT_PATH, 'database', d, true));
  switch (last.kind) {
    case 'database':
      return [node(path, 'keyPrefix', 'user:', true), node(path, 'key', 'config', false, 'string')];
    case 'keyPrefix':
      return [node(path, 'key', `${last.name}1`, false, 'hash ttl:3600')];
    default:
      return [];
  }
}

export function redisDetails(path: ObjectPath): TableDetails {
  void path;
  throw ipcError('unsupported', 'table details are not available in the browser mock');
}

export function redisDdl(path: ObjectPath): string {
  return `-- DDL is not available for ${lastSegment(path)?.name ?? ''}`;
}
