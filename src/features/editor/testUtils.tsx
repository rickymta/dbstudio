// Shared helpers for the editor tests (not imported by app code).
import { ipc } from '@/lib/ipc';
import { resetMockConnections, resetMockPhase2 } from '@/lib/mock';
import type { ConnectionProfile, SessionInfo } from '@/lib/types';
import { useSessionsStore } from '@/stores/sessions';
import { WELCOME_TAB, useTabsStore } from '@/stores/tabs';
import { useConfirmStore } from '@/features/safety/confirmStore';
import { resetEditorRunner } from './editorRunner';
import { resetEditorStore } from './editorStore';

export const PG_DEV = '00000000-0000-4000-8000-0000000000b1';
export const MYSQL_STAGING = '00000000-0000-4000-8000-0000000000b2';
export const PG_PROD_RO = '00000000-0000-4000-8000-0000000000b3';

/** Back to a clean app: mocks, stores, sessions, tabs. */
export function resetEditorWorld(): void {
  resetEditorRunner();
  resetMockConnections();
  resetMockPhase2();
  useSessionsStore.setState({ sessionsByConnection: {}, opening: {} });
  useTabsStore.setState({ tabs: [WELCOME_TAB], activeId: WELCOME_TAB.id });
  useConfirmStore.setState({ queue: [] });
  resetEditorStore();
}

export function openSession(connectionId = PG_DEV): Promise<SessionInfo> {
  return useSessionsStore.getState().open(connectionId, 'pw');
}

/** A writable production copy of the dev PostgreSQL profile. */
export async function addProductionProfile(): Promise<ConnectionProfile> {
  const base = (await ipc.connectionsList()).profiles.find((p) => p.id === PG_DEV)!;
  return ipc.connectionSave(
    {
      ...base,
      id: '00000000-0000-4000-8000-0000000000c9',
      name: 'PG prod writable',
      environment: 'production',
      readOnly: false,
      database: 'app_prod',
    },
    { action: 'keep' },
  );
}
