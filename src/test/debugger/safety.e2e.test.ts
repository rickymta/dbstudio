import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { ipc } from '@/lib/ipc';
import { setMockDebugTimings } from '@/lib/mock/debug';
import { mockBreakLease } from '@/lib/mock/leases';
import {
  addProductionProfile,
  bp,
  openLease,
  pauseReason,
  PG_PROD_RO,
  resetDebugWorld,
  startDebug,
  stepWithGrant,
} from './helpers';

beforeEach(resetDebugWorld);
afterEach(() => vi.useRealTimers());

describe('read-only connection', () => {
  it('blocks a write statement; Jump To skips it', async () => {
    const leaseId = await openLease(PG_PROD_RO);
    const text = "select 1;\nupdate users set name = 'x' where id = 1;\nselect 3;";
    const s = await startDebug(leaseId, text, { breakpoints: [bp(1)] });
    expect(pauseReason(s)).toBe('breakpoint');
    const blocked = await ipc.debugResume(s.debugId);
    expect(blocked.status).toMatchObject({
      reason: { kind: 'blocked', error: { kind: 'readOnly' } },
    });
    expect(blocked.position?.line).toBe(2);
    // the blocked statement does not run: still blocked on retry
    const again = await ipc.debugStep(s.debugId, { kind: 'stepOver' });
    expect(pauseReason(again)).toBe('blocked');
    const jumped = await ipc.debugStep(s.debugId, { kind: 'jumpTo', line: 3 });
    expect(jumped.position).toMatchObject({ line: 3, step: 2 });
    const end = await ipc.debugStep(s.debugId, { kind: 'resume' });
    expect(pauseReason(end)).toBe('awaitingEnd');
    expect((await ipc.debugStop(s.debugId, 'rollback')).stepsRun).toBe(2);
  });
});

describe('production confirmation', () => {
  it('a destructive step pauses for a one-time token tied to the statement', async () => {
    const profile = await addProductionProfile();
    const leaseId = await openLease(profile.id);
    const s = await startDebug(leaseId, 'select 1;\ndelete from users;\nselect 3;', {
      options: { stopOnEntry: false } as never,
    });
    expect(s.status).toMatchObject({ reason: { kind: 'confirmation' } });
    const pause = s.status.kind === 'paused' ? s.status.reason : null;
    if (pause?.kind !== 'confirmation') throw new Error('expected a confirmation pause');
    const first = pause.request;
    expect(first.statements[0]).toMatchObject({ reason: 'deleteWithoutWhere', index: 1 });
    expect(first.confirmText).toBe('app_prod');

    // plain retry without a grant: a fresh token is issued
    const retry = await ipc.debugStep(s.debugId, { kind: 'stepOver' });
    const second =
      retry.status.kind === 'paused' && retry.status.reason.kind === 'confirmation'
        ? retry.status.reason.request
        : null;
    expect(second).not.toBeNull();
    expect(second!.token).not.toBe(first.token);

    // wrong text typed -> rejected, a new token is issued
    await expect(
      stepWithGrant(s.debugId, { token: second!.token, typed: 'wrong' }),
    ).rejects.toMatchObject({ kind: 'invalid' });
    // the old tokens cannot be reused
    await expect(
      stepWithGrant(s.debugId, { token: first.token, typed: first.confirmText }),
    ).rejects.toMatchObject({ kind: 'invalid' });

    const snap = await ipc.debugSnapshot(s.debugId);
    const fresh =
      snap.status.kind === 'paused' && snap.status.reason.kind === 'confirmation'
        ? snap.status.reason.request
        : null;
    expect(fresh).not.toBeNull();
    const ok = await stepWithGrant(s.debugId, { token: fresh!.token, typed: fresh!.confirmText });
    expect(pauseReason(ok)).toBe('step');
    expect(ok.position?.step).toBe(2);
    await ipc.debugStop(s.debugId, 'rollback');
  });
});

describe('transaction lifetime', () => {
  it('warns after the threshold and the paused TTL rolls the owned transaction back', async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    setMockDebugTimings({ pausedTtlSecs: 120, txWarnSecs: 30 });
    const leaseId = await openLease();
    const s = await startDebug(leaseId, 'select 1;\nselect 2;', {
      options: { stopOnEntry: true } as never,
    });
    const stepped = await ipc.debugStep(s.debugId, { kind: 'stepOver' });
    expect(stepped.transaction).toMatchObject({ owned: true, warn: false, ttlLeftSecs: 120 });

    await vi.advanceTimersByTimeAsync(31_000);
    const warned = await ipc.debugSnapshot(s.debugId);
    expect(warned.transaction).toMatchObject({ warn: true, ttlLeftSecs: 90 });

    // any command re-arms the timers
    await ipc.debugEvaluate(s.debugId, '1 + 1');
    await vi.advanceTimersByTimeAsync(100_000);
    expect(pauseReason(await ipc.debugSnapshot(s.debugId))).toBe('step');

    await vi.advanceTimersByTimeAsync(30_000);
    const ended = await ipc.debugSnapshot(s.debugId);
    expect(ended.status).toMatchObject({
      kind: 'terminated',
      reason: 'ttl',
      report: { rolledBack: true, committed: false },
    });
  });

  it('a broken lease ends the session with leaseLost', async () => {
    const leaseId = await openLease();
    const s = await startDebug(leaseId, 'select 1;\nselect 2;', {
      options: { stopOnEntry: true } as never,
    });
    mockBreakLease(leaseId);
    const state = await ipc.debugStep(s.debugId, { kind: 'stepOver' });
    expect(state.status).toMatchObject({ kind: 'terminated', reason: 'leaseLost' });
  });

  it('keep leaves the transaction open for the tab; lease mode never owns one', async () => {
    const leaseId = await openLease();
    const s = await startDebug(leaseId, 'select 1;', { options: { stopOnEntry: true } as never });
    await ipc.debugStep(s.debugId, { kind: 'stepOver' });
    const keep = await ipc.debugStop(s.debugId, 'keep');
    expect(keep).toMatchObject({ committed: false, rolledBack: false });

    const lease2 = await openLease();
    const l = await startDebug(lease2, 'select 1;\nselect 2;', {
      options: { txMode: 'lease', stopOnEntry: true } as never,
    });
    const after = await ipc.debugStep(l.debugId, { kind: 'stepOver' });
    expect(after.transaction.owned).toBe(false);
    const end = await ipc.debugStep(l.debugId, { kind: 'resume' });
    expect(end.status).toMatchObject({ kind: 'terminated', reason: 'finished' });
  });
});
