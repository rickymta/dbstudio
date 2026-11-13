// Shared helpers for the key-value feature tests (not imported by app code).
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { render } from '@testing-library/react';
import type { ReactElement } from 'react';
import { I18nextProvider } from 'react-i18next';
import i18n from '@/lib/i18n';
import { ipc } from '@/lib/ipc';
import { resetMockConnections } from '@/lib/mock';
import { resetMockRedis } from '@/lib/mock/catalogRedis';
import type { ConnectionProfile, ObjectPath, SessionInfo } from '@/lib/types';

let counter = 0;

/** Save a Redis profile in the mock store and open a session on it. */
export async function openRedisSession(
  overrides: Partial<ConnectionProfile> = {},
): Promise<SessionInfo> {
  const id = `00000000-0000-4000-8000-00000000f${String(++counter).padStart(3, '0')}`;
  await ipc.connectionSave(
    {
      id,
      name: 'Redis test',
      kind: 'redis',
      host: 'localhost',
      port: 6379,
      database: null,
      username: null,
      tls: { mode: 'disable' },
      connectTimeoutMs: null,
      readOnly: false,
      environment: null,
      options: {},
      groupId: null,
      color: null,
      savePassword: true,
      hasPassword: false,
      sortOrder: 0,
      createdAt: '2026-01-01T00:00:00Z',
      updatedAt: '2026-01-01T00:00:00Z',
      ...overrides,
    },
    { action: 'keep' },
  );
  return ipc.sessionOpen(id);
}

export function resetRedisMocks(): void {
  resetMockConnections();
  resetMockRedis();
}

/** `[Database "0", Key name]`. */
export const keyPath = (name: string, db = '0'): ObjectPath => ({
  segments: [
    { kind: 'database', name: db },
    { kind: 'key', name },
  ],
});

export function renderWithProviders(ui: ReactElement) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  void i18n.changeLanguage('en');
  return render(
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={client}>{ui}</QueryClientProvider>
    </I18nextProvider>,
  );
}
