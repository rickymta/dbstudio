import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { ExecuteResult, ExecuteRequest, QueryEvent } from './types';

const hoisted = vi.hoisted(() => {
  class FakeChannel {
    onmessage: (m: unknown) => void = () => undefined;
  }
  return { FakeChannel, invoke: vi.fn(), listen: vi.fn() };
});
vi.mock('@tauri-apps/api/core', () => ({ Channel: hoisted.FakeChannel, invoke: hoisted.invoke }));
vi.mock('@tauri-apps/api/event', () => ({ listen: hoisted.listen }));

import { ipc } from './ipc';

const endEvent: QueryEvent = {
  type: 'statementEnd',
  end: { outcome: { kind: 'done' }, elapsedMs: 1 },
};
const request: ExecuteRequest = { text: 'select 1; select 2', queryId: 'q1' };
const result = (statements: number): ExecuteResult => ({
  summary: { statements, elapsedMs: 3 },
  state: null,
  historyId: null,
});
const flush = () => new Promise((r) => setTimeout(r, 0));

beforeEach(() => {
  hoisted.invoke.mockReset();
  hoisted.listen.mockReset();
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
});
afterEach(() => {
  delete (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__;
});

describe('streaming execute commands on the Tauri path', () => {
  it('lease_execute waits for one statementEnd per statement', async () => {
    hoisted.invoke.mockResolvedValue(result(2));
    let settled = false;
    const p = ipc
      .leaseExecute('l1', request, () => undefined)
      .then((r) => {
        settled = true;
        return r;
      });
    await flush();
    const [command, args] = hoisted.invoke.mock.calls[0]!;
    expect(command).toBe('lease_execute');
    expect(args.leaseId).toBe('l1');
    expect(args.request).toEqual(request);
    const ch = args.onEvent as InstanceType<typeof hoisted.FakeChannel>;
    ch.onmessage(endEvent);
    await flush();
    expect(settled).toBe(false);
    ch.onmessage(endEvent);
    await expect(p).resolves.toEqual(result(2));
  });

  it('query_execute and lease_fetch_more send the documented parameters', async () => {
    hoisted.invoke.mockImplementation(async (_c, args) => {
      (args.onEvent as { onmessage: (e: QueryEvent) => void }).onmessage(endEvent);
      return result(1);
    });
    await ipc.queryExecute('s1', request, () => undefined);
    expect(hoisted.invoke.mock.calls[0]![0]).toBe('query_execute');
    expect(Object.keys(hoisted.invoke.mock.calls[0]![1]).sort()).toEqual([
      'onEvent',
      'request',
      'sessionId',
    ]);
    await ipc.leaseFetchMore('l1', 'q1', 500, null, () => undefined);
    const [command, args] = hoisted.invoke.mock.calls[1]!;
    expect(command).toBe('lease_fetch_more');
    expect({ ...args, onEvent: undefined }).toEqual({
      leaseId: 'l1',
      queryId: 'q1',
      maxRows: 500,
      batchSize: null,
      onEvent: undefined,
    });
  });

  it('resolves at once for a script without statements', async () => {
    hoisted.invoke.mockResolvedValue(result(0));
    await expect(ipc.queryExecute('s1', request, () => undefined)).resolves.toEqual(result(0));
  });
});

describe('plain commands use the documented names and parameters', () => {
  it('maps wrappers to snake_case commands', async () => {
    hoisted.invoke.mockResolvedValue(undefined);
    await ipc.leaseCommit('l1');
    await ipc.leaseSetAutoCommit('l1', false);
    await ipc.historyClear();
    await ipc.keyRead('s1', { key: { segments: [] }, count: 10 });
    await ipc.connectionSave(
      { id: 'c' } as never,
      { action: 'keep' },
      { action: 'set', value: 'pw' },
    );
    expect(hoisted.invoke.mock.calls.map((c) => c[0])).toEqual([
      'lease_commit',
      'lease_set_auto_commit',
      'history_clear',
      'key_read',
      'connection_save',
    ]);
    expect(hoisted.invoke.mock.calls[1]![1]).toEqual({ leaseId: 'l1', autoCommit: false });
    expect(hoisted.invoke.mock.calls[2]![1]).toEqual({ connectionId: null });
    expect(hoisted.invoke.mock.calls[4]![1]).toMatchObject({ sshSecret: { action: 'set' } });
  });

  it('keeps extra error fields on the IpcError', async () => {
    hoisted.invoke.mockRejectedValue({
      kind: 'sshHostKey',
      message: 'unknown host',
      host: 'h',
      port: 22,
      keyType: 'ssh-ed25519',
      fingerprint: 'SHA256:x',
      changed: true,
    });
    await expect(ipc.sshTrustHost('h', 22, 'k', 'f')).rejects.toEqual({
      kind: 'sshHostKey',
      message: 'unknown host',
      host: 'h',
      port: 22,
      keyType: 'ssh-ed25519',
      fingerprint: 'SHA256:x',
      changed: true,
    });
  });
});

describe('ipc.onEvent on the Tauri path', () => {
  it('listens through Tauri and delivers payloads', async () => {
    const unlisten = vi.fn();
    let callback: (e: { payload: unknown }) => void = () => undefined;
    hoisted.listen.mockImplementation(async (_name, cb) => {
      callback = cb;
      return unlisten;
    });
    const seen: unknown[] = [];
    const off = await ipc.onEvent('job://progress', (p) => seen.push(p));
    expect(hoisted.listen.mock.calls[0]![0]).toBe('job://progress');
    callback({ payload: { type: 'started' } });
    expect(seen).toEqual([{ type: 'started' }]);
    off();
    expect(unlisten).toHaveBeenCalled();
  });
});
