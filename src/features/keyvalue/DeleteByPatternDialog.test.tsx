import { act, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { ConfirmDestructiveHost } from '@/features/safety/ConfirmDestructiveHost';
import { ipc } from '@/lib/ipc';
import { childPath } from '@/lib/objectPath';
import { redisDb } from '@/lib/mock/catalogRedis';
import { mockEmit, resetMockEvents } from '@/lib/mock/events';
import { setMockKeyDeleteConfirmation } from '@/lib/mock/keyvalue';
import type { JobInfo, ObjectNode } from '@/lib/types';
import { DeleteByPatternDialog } from './DeleteByPatternDialog';
import { openRedisSession, renderWithProviders, resetRedisMocks } from './testUtils';

const WAIT = { timeout: 4000 };
const db: ObjectNode = {
  path: { segments: [{ kind: 'database', name: '0' }] },
  kind: 'database',
  name: '0',
  hasChildren: true,
};
const prefixNode = (name: string): ObjectNode => ({
  path: childPath(db.path, 'keyPrefix', name),
  kind: 'keyPrefix',
  name,
  hasChildren: true,
});

let sessionId: string;
beforeEach(async () => {
  resetRedisMocks();
  sessionId = (await openRedisSession()).sessionId;
});
afterEach(() => {
  setMockKeyDeleteConfirmation(false);
  resetMockEvents();
  vi.restoreAllMocks();
});

function mount(node: ObjectNode, onDone = vi.fn()) {
  renderWithProviders(
    <>
      <DeleteByPatternDialog
        open
        sessionId={sessionId}
        node={node}
        onOpenChange={() => undefined}
        onDone={onDone}
      />
      <ConfirmDestructiveHost />
    </>,
  );
  return onDone;
}

describe('DeleteByPatternDialog', () => {
  it('suggests <prefix>* and deletes with progress', async () => {
    const user = userEvent.setup();
    const before = redisDb('0').size;
    const onDone = mount(prefixNode('session:'));
    const input = screen.getByLabelText('Pattern');
    expect(input).toHaveValue('session:*');
    await user.click(screen.getByRole('button', { name: 'Delete matching keys' }));
    expect(
      await screen.findByText('Done. Keys deleted: 900.', undefined, WAIT),
    ).toBeInTheDocument();
    expect(redisDb('0').size).toBe(before - 900);
    expect(onDone).toHaveBeenCalledTimes(1);
  });

  it('suggests * on a database and blocks an empty pattern', async () => {
    const user = userEvent.setup();
    mount(db);
    const input = screen.getByLabelText('Pattern');
    expect(input).toHaveValue('*');
    await user.clear(input);
    expect(screen.getByRole('button', { name: 'Delete matching keys' })).toBeDisabled();
  });

  it('asks for the database name before deleting every key, even without protection', async () => {
    const user = userEvent.setup();
    mount(db);
    const spy = vi.spyOn(ipc, 'keysDeletePattern');
    await user.click(screen.getByRole('button', { name: 'Delete matching keys' }));
    const confirm = await screen.findByRole('button', { name: 'Run anyway' }, WAIT);
    expect(spy).not.toHaveBeenCalled();
    expect(confirm).toBeDisabled();
    await user.type(screen.getByRole('textbox', { name: /^Type .* to confirm$/ }), 'db0');
    await user.click(confirm);
    expect(await screen.findByText(/Done\. Keys deleted/, undefined, WAIT)).toBeInTheDocument();
    expect(spy).toHaveBeenCalledTimes(1);
  });

  it('declining the whole-database confirmation deletes nothing', async () => {
    const user = userEvent.setup();
    mount(db);
    const spy = vi.spyOn(ipc, 'keysDeletePattern');
    await user.click(screen.getByRole('button', { name: 'Delete matching keys' }));
    await screen.findByRole('button', { name: 'Run anyway' }, WAIT);
    await user.click(screen.getByRole('button', { name: 'Cancel' }));
    await waitFor(() => expect(screen.queryByRole('button', { name: 'Run anyway' })).toBeNull());
    expect(spy).not.toHaveBeenCalled();
    expect(screen.getByRole('button', { name: 'Delete matching keys' })).toBeEnabled();
  });

  it('walks through the typed confirmation required on protected connections', async () => {
    const user = userEvent.setup();
    setMockKeyDeleteConfirmation(true);
    mount(prefixNode('jobs:'));
    const spy = vi.spyOn(ipc, 'keysDeletePattern');
    await user.click(screen.getByRole('button', { name: 'Delete matching keys' }));
    const confirm = await screen.findByRole('button', { name: 'Run anyway' }, WAIT);
    expect(confirm).toBeDisabled();
    expect(screen.getByText('Type DEL jobs:* to confirm')).toBeInTheDocument();
    await user.type(screen.getByRole('textbox', { name: /^Type .* to confirm$/ }), 'DEL jobs');
    expect(confirm).toBeDisabled();
    await user.type(screen.getByRole('textbox', { name: /^Type .* to confirm$/ }), ':*');
    await user.click(confirm);
    expect(await screen.findByText(/Done\. Keys deleted/, undefined, WAIT)).toBeInTheDocument();
    expect(spy).toHaveBeenCalledTimes(2);
    expect(spy.mock.calls[0]![3]).toBeNull();
    expect(spy.mock.calls[1]![3]).toMatchObject({ typed: 'DEL jobs:*' });
    expect([...redisDb('0').keys()].some((k) => k.startsWith('jobs:'))).toBe(false);
  });

  it('shows job progress and cancels the job', async () => {
    const user = userEvent.setup();
    const job = (status: JobInfo['status'], done: number): JobInfo => ({
      id: 'job-1',
      kind: 'keyDelete',
      title: 'DEL x',
      connectionId: null,
      status,
      progress: {
        phase: 'data',
        object: null,
        objectsDone: 0,
        objectsTotal: null,
        rowsDone: done,
        rowsTotal: 500,
        bytesDone: 0,
        bytesTotal: null,
        message: null,
      },
      createdAt: '',
      startedAt: null,
      finishedAt: null,
      error: null,
      result: null,
      cancellable: true,
    });
    vi.spyOn(ipc, 'keysDeletePattern').mockResolvedValue('job-1');
    const cancel = vi.spyOn(ipc, 'jobCancel').mockResolvedValue();
    mount(prefixNode('x:'));
    // Let the dialog subscribe to job events first.
    await new Promise((r) => setTimeout(r, 20));
    await user.click(screen.getByRole('button', { name: 'Delete matching keys' }));
    act(() => mockEmit('job://progress', { type: 'started', job: job('running', 0) }));
    act(() =>
      mockEmit('job://progress', {
        type: 'progress',
        jobId: 'job-1',
        progress: { ...job('running', 120).progress, message: 'x:120' },
      }),
    );
    expect(await screen.findByText('Deleted 120 of 500 keys...')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Cancel job' }));
    expect(cancel).toHaveBeenCalledWith('job-1');
    act(() => mockEmit('job://progress', { type: 'finished', job: job('cancelled', 120) }));
    await waitFor(() =>
      expect(screen.getByText('Cancelled after deleting 120 keys.')).toBeInTheDocument(),
    );
  });

  it('shows an error returned when starting the job', async () => {
    const user = userEvent.setup();
    vi.spyOn(ipc, 'keysDeletePattern').mockRejectedValue({
      kind: 'readOnly',
      message: 'read-only connection',
    });
    mount(prefixNode('jobs:'));
    await user.click(screen.getByRole('button', { name: 'Delete matching keys' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('read-only connection');
  });
});
