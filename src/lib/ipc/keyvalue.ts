import { mock } from '../mock';
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
import { call } from './core';

/** Paged object tree and key-value (Redis) reads. */
export const keyvalueApi = {
  objectChildrenPage: (sessionId: Uuid, request: ChildrenPageRequest): Promise<ChildrenPage> =>
    call('object_children_page', () => mock.objectChildrenPage(sessionId, request), {
      sessionId,
      request,
    }),
  keyRead: (sessionId: Uuid, request: KeyReadRequest): Promise<KeyValuePage> =>
    call('key_read', () => mock.keyRead(sessionId, request), { sessionId, request }),
  /** Starts a `keyDelete` job; production needs `confirm` (see `confirmationRequired`). */
  keysDeletePattern: (
    sessionId: Uuid,
    database: ObjectPath,
    pattern: string,
    confirm?: ConfirmationGrant | null,
  ): Promise<JobId> =>
    call(
      'keys_delete_pattern',
      () => mock.keysDeletePattern(sessionId, database, pattern, confirm ?? null),
      { sessionId, database, pattern, confirm: confirm ?? null },
    ),
};
