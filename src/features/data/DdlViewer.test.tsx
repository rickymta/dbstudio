import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { ipc } from '@/lib/ipc';
import { DdlViewer } from './DdlViewer';
import {
  PG_CONNECTION,
  openMockSession,
  pgPath,
  polyfillLayoutApis,
  renderWithProviders,
} from './testUtils';

let sessionId: string;
beforeAll(async () => {
  polyfillLayoutApis();
  sessionId = await openMockSession();
});
afterEach(() => vi.restoreAllMocks());

describe('DdlViewer (mock IPC)', () => {
  it('shows the DDL in a read-only editor', async () => {
    renderWithProviders(
      <DdlViewer sessionId={sessionId} connectionId={PG_CONNECTION} path={pgPath('events')} />,
    );
    const editor = await screen.findByTestId('code-view', undefined, { timeout: 5000 });
    await waitFor(() => expect(editor.textContent).toContain('CREATE TABLE'));
    expect(editor.textContent).toContain('"public"."events"');
    const content = editor.querySelector('.cm-content')!;
    expect(content).toHaveAttribute('contenteditable', 'false');
    expect(editor.querySelector('.cm-lineNumbers')).not.toBeNull();
  });

  it('shows a friendly message when DDL is unsupported', async () => {
    vi.spyOn(ipc, 'objectDdl').mockRejectedValue({
      kind: 'unsupported',
      message: 'ddl not supported',
    });
    renderWithProviders(
      <DdlViewer sessionId={sessionId} connectionId={PG_CONNECTION} path={pgPath('events')} />,
    );
    expect(
      await screen.findByText('DDL is not available for this object on this database.'),
    ).toBeInTheDocument();
    expect(screen.queryByText('ddl not supported')).toBeNull();
  });

  it('shows other errors with a retry that refetches', async () => {
    const user = userEvent.setup();
    const spy = vi
      .spyOn(ipc, 'objectDdl')
      .mockRejectedValueOnce({ kind: 'connection', message: 'connection lost' })
      .mockResolvedValue('CREATE TABLE t (id int);');
    renderWithProviders(
      <DdlViewer sessionId={sessionId} connectionId={PG_CONNECTION} path={pgPath('events')} />,
    );
    expect(await screen.findByText('connection lost')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Retry' }));
    const editor = await screen.findByTestId('code-view', undefined, { timeout: 5000 });
    await waitFor(() => expect(editor.textContent).toContain('CREATE TABLE t'));
    expect(spy).toHaveBeenCalledTimes(2);
  });
});
