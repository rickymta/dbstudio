// Shared helpers for the data feature tests (not imported by app code).
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { render } from '@testing-library/react';
import type { ReactElement } from 'react';
import { I18nextProvider } from 'react-i18next';
import i18n from '@/lib/i18n';
import { ipc } from '@/lib/ipc';
import type { ObjectKind, ObjectPath } from '@/lib/types';

export const PG_CONNECTION = '00000000-0000-4000-8000-0000000000b1';

/** `app_dev > public > <name>` inside the mock PostgreSQL server. */
export function pgPath(name: string, kind: ObjectKind = 'table'): ObjectPath {
  const folder = kind === 'view' ? 'views' : 'tables';
  return {
    segments: [
      { kind: 'database', name: 'app_dev' },
      { kind: 'folder', name: 'schemas' },
      { kind: 'schema', name: 'public' },
      { kind: 'folder', name: folder },
      { kind, name },
    ],
  };
}

export async function openMockSession(): Promise<string> {
  const info = await ipc.sessionOpen(PG_CONNECTION);
  return info.sessionId;
}

export function renderWithProviders(ui: ReactElement) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  void i18n.changeLanguage('en');
  const wrap = (node: ReactElement) => (
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={client}>{node}</QueryClientProvider>
    </I18nextProvider>
  );
  const result = render(wrap(ui));
  return { ...result, rerenderWithProviders: (node: ReactElement) => result.rerender(wrap(node)) };
}

/** CodeMirror measures text through Range APIs that jsdom lacks. */
export function polyfillLayoutApis(): void {
  const rect = { x: 0, y: 0, top: 0, left: 0, right: 0, bottom: 0, width: 0, height: 0 } as DOMRect;
  const list = Object.assign([], { item: () => null }) as unknown as DOMRectList;
  if (!Range.prototype.getBoundingClientRect) Range.prototype.getBoundingClientRect = () => rect;
  if (!Range.prototype.getClientRects) Range.prototype.getClientRects = () => list;
  if (!document.elementFromPoint) document.elementFromPoint = () => null;
}
