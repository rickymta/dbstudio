// Mock key-value commands: only the paged tree works (it wraps the plain children list).

import type {
  ChildrenPage,
  ChildrenPageRequest,
  ConfirmationGrant,
  JobId,
  KeyReadRequest,
  KeyValuePage,
  ObjectPath,
  Uuid,
} from '../types';
import { mockChildren } from './catalog';
import { mockSessionKind } from './sessions';
import { unsupported } from './util';

function glob(pattern: string): RegExp {
  const escaped = pattern
    .replace(/[.+^${}()|\\]/g, '\\$&')
    .replace(/\*/g, '.*')
    .replace(/\?/g, '.');
  return new RegExp(`^${escaped}$`, 'i');
}

export const keyvalueMock = {
  async objectChildrenPage(sessionId: Uuid, request: ChildrenPageRequest): Promise<ChildrenPage> {
    const kind = mockSessionKind(sessionId);
    const all = mockChildren(kind, request.parent);
    const re = request.pattern ? glob(request.pattern) : null;
    const filtered = re ? all.filter((n) => re.test(n.name)) : all;
    const offset = Number(request.cursor ?? 0) || 0;
    const nodes = filtered.slice(offset, offset + request.limit);
    const next = offset + nodes.length;
    return {
      nodes,
      nextCursor: next < filtered.length ? String(next) : null,
      approximateTotal: filtered.length,
    };
  },
  async keyRead(sessionId: Uuid, request: KeyReadRequest): Promise<KeyValuePage> {
    mockSessionKind(sessionId);
    void request;
    throw unsupported('key_value');
  },
  async keysDeletePattern(
    sessionId: Uuid,
    database: ObjectPath,
    pattern: string,
    confirm: ConfirmationGrant | null,
  ): Promise<JobId> {
    mockSessionKind(sessionId);
    void [database, pattern, confirm];
    throw unsupported('key_value');
  },
};
