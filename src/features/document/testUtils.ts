// Helpers for the document feature tests (not imported by app code).
import { ipc } from '@/lib/ipc';
import { resetMockConnections } from '@/lib/mock';
import { resetMockMongo } from '@/lib/mock/catalogMongo';
import type { ObjectPath } from '@/lib/types';

export function mongoPath(collection: string): ObjectPath {
  return {
    segments: [
      { kind: 'database', name: 'app' },
      { kind: 'folder', name: 'collections' },
      { kind: 'collection', name: collection },
    ],
  };
}

/** Saves a MongoDB profile into the connection mock and opens a session on it. */
export async function openMongoSession(
  opts: { readOnly?: boolean } = {},
): Promise<{ sessionId: string; connectionId: string }> {
  const snapshot = await ipc.connectionsList();
  const template = snapshot.profiles[0]!;
  const saved = await ipc.connectionSave(
    {
      ...template,
      id: crypto.randomUUID(),
      name: 'Mongo test',
      kind: 'mongodb',
      port: 27017,
      readOnly: opts.readOnly ?? false,
    },
    { action: 'keep' },
  );
  const info = await ipc.sessionOpen(saved.id);
  return { sessionId: info.sessionId, connectionId: saved.id };
}

export function resetMongoMocks(): void {
  resetMockMongo();
  resetMockConnections();
}
