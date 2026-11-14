import { beforeEach, describe, expect, it } from 'vitest';
import { ipc } from '@/lib/ipc';
import type { DebugOutputEvent } from '@/lib/types';
import { bp, openLease, pauseReason, recordEvents, resetDebugWorld, startDebug } from './helpers';

const FIVE = [
  'select 1;',
  'select * from users;',
  "update users set name = 'x' where id = 1;",
  'select 4;',
  'select 5;',
].join('\n');

beforeEach(resetDebugWorld);

describe('script debugging (5 statements, breakpoint on statement 3)', () => {
  it('start (entry) -> resume stops at the breakpoint -> step -> variables -> stop asks, rollback', async () => {
    const leaseId = await openLease();
    const b3 = bp(3);
    const first = await startDebug(leaseId, FIVE, {
      breakpoints: [b3],
      options: { stopOnEntry: true } as never,
    });
    expect(pauseReason(first)).toBe('entry');
    expect(first.position).toMatchObject({ line: 1, step: 0, stepCount: 5 });
    expect(first.breakpoints).toEqual([{ id: b3.id, verified: true, line: 3 }]);

    const atBp = await ipc.debugResume(first.debugId);
    expect(pauseReason(atBp)).toBe('breakpoint');
    expect(atBp.status).toMatchObject({ reason: { kind: 'breakpoint', ids: [b3.id] } });
    expect(atBp.position).toMatchObject({ line: 3, step: 2 });
    expect(atBp.transaction).toMatchObject({ owned: true, status: 'active' });

    const stepped = await ipc.debugStep(first.debugId, { kind: 'stepOver' });
    expect(pauseReason(stepped)).toBe('step');
    expect(stepped.position).toMatchObject({ line: 4, step: 3 });
    expect(stepped.inlineValues.some((i) => i.line === 3 && i.text === '1 row affected')).toBe(
      true,
    );

    const groups = stepped.variables.map((v) => v.name);
    expect(groups).toEqual(expect.arrayContaining(['Session', 'Transaction', 'Last result']));
    const tx = stepped.variables.find((v) => v.name === 'Transaction')!;
    const kids = await ipc.debugVariableChildren(first.debugId, tx.reference);
    expect(kids.find((k) => k.name === 'owned by debugger')?.value).toEqual({
      t: 'bool',
      v: true,
    });

    // stop with the default `ask`: the session waits for the commit / rollback choice
    const ask = await ipc.debugStop(first.debugId, 'ask');
    expect(ask).toMatchObject({ committed: false, rolledBack: false });
    expect(pauseReason(await ipc.debugSnapshot(first.debugId))).toBe('awaitingEnd');
    await expect(ipc.debugStop(first.debugId, 'ask')).rejects.toMatchObject({ kind: 'invalid' });
    const report = await ipc.debugStop(first.debugId, 'rollback');
    expect(report).toMatchObject({ rolledBack: true, committed: false, stepsRun: 3 });
    const done = await ipc.debugSnapshot(first.debugId);
    expect(done.status).toMatchObject({ kind: 'terminated', reason: 'stopped' });
    // idempotent
    expect(await ipc.debugStop(first.debugId, 'commit')).toEqual(report);
  });

  it('a breakpoint with no stop-on-entry runs straight to it, and resuming past it ends the script', async () => {
    const leaseId = await openLease();
    const s = await startDebug(leaseId, FIVE, { breakpoints: [bp(3)] });
    expect(pauseReason(s)).toBe('breakpoint');
    expect(s.position?.step).toBe(2);
    const end = await ipc.debugResume(s.debugId);
    expect(pauseReason(end)).toBe('awaitingEnd');
    const report = await ipc.debugStop(s.debugId, 'commit');
    expect(report).toMatchObject({ committed: true, stepsRun: 5 });
  });

  it('blank/comment lines snap to the next statement; lines past the end are unverified', async () => {
    const leaseId = await openLease();
    const text = 'select 1;\n\n-- note\nselect 2;';
    const s = await startDebug(leaseId, text);
    const statuses = await ipc.debugSetBreakpoints(s.debugId, { kind: 'script' }, [bp(2), bp(9)]);
    expect(statuses[0]).toMatchObject({ verified: true, line: 4 });
    expect(statuses[1]).toMatchObject({ verified: false });
  });

  it('condition, hit count, log message and mute', async () => {
    const leaseId = await openLease();
    const text = 'select 1;\nselect 2;\nselect 3;';
    const events = await recordEvents();
    const s = await startDebug(leaseId, text, {
      params: [{ name: 'x', value: { kind: 'number', v: '5' } }],
      options: { stopOnEntry: true } as never,
    });
    const falseBp = bp(2, { condition: 'x = 4' });
    const logBp = bp(2, { suspend: false, logMessage: 'x+1={x + 1}' });
    const trueBp = bp(3, { condition: 'x = 5 and not false' });
    await ipc.debugSetBreakpoints(s.debugId, { kind: 'script' }, [falseBp, logBp, trueBp]);
    const stop = await ipc.debugResume(s.debugId);
    expect(stop.status).toMatchObject({ reason: { kind: 'breakpoint', ids: [trueBp.id] } });
    const logs = events.outputs.filter(
      (e): e is DebugOutputEvent & { output: { kind: 'message'; text: string } } =>
        e.output.kind === 'message' && e.output.source === 'log',
    );
    expect(logs.map((l) => l.output.text)).toEqual(['x+1=6']);
    events.stop();

    // restart keeps breakpoints; muted ignores them
    await ipc.debugSetMuted(s.debugId, true);
    const re = await ipc.debugRestart(s.debugId, text, 'rollback');
    expect(re.debugId).not.toBe(s.debugId);
    expect(re.muted).toBe(true);
    expect(pauseReason(re)).toBe('entry');
    const ran = await ipc.debugResume(re.debugId);
    expect(pauseReason(ran)).toBe('awaitingEnd'); // muted: no breakpoint stops it
  });

  it('run to cursor stops at the cursor statement but earlier breakpoints win unless forced', async () => {
    const leaseId = await openLease();
    const s = await startDebug(leaseId, FIVE, {
      breakpoints: [bp(2)],
      options: { stopOnEntry: true } as never,
    });
    const viaBp = await ipc.debugRunToCursor(s.debugId, { kind: 'script' }, 4, false);
    expect(pauseReason(viaBp)).toBe('breakpoint');
    expect(viaBp.position?.line).toBe(2);
    const reached = await ipc.debugRunToCursor(s.debugId, { kind: 'script' }, 4, false);
    expect(pauseReason(reached)).toBe('runToCursor');
    expect(reached.position?.line).toBe(4);

    const fresh = await ipc.debugRestart(s.debugId, FIVE, 'rollback');
    const forced = await ipc.debugRunToCursor(fresh.debugId, { kind: 'script' }, 4, true);
    expect(pauseReason(forced)).toBe('runToCursor');
    expect(forced.position?.line).toBe(4);
  });

  it('events: seq is strictly increasing across both streams, the last state is terminated', async () => {
    const leaseId = await openLease();
    const events = await recordEvents();
    const s = await startDebug(leaseId, 'select 1;\nselect 2;', {
      options: { endAction: 'rollback' } as never,
    });
    await ipc.debugResume(s.debugId);
    events.stop();
    expect(events.seqs.length).toBeGreaterThan(5);
    for (let i = 1; i < events.seqs.length; i++) {
      expect(events.seqs[i]!).toBeGreaterThan(events.seqs[i - 1]!);
    }
    expect(events.states.at(-1)?.status.kind).toBe('terminated');
    const kinds = events.outputs.map((o) => o.output.kind);
    expect(kinds.filter((k) => k === 'stepStart')).toHaveLength(2);
    expect(kinds.filter((k) => k === 'stepEnd')).toHaveLength(2);
    expect(kinds.indexOf('stepStart')).toBeLessThan(kinds.indexOf('stepEnd'));
  });

  it('a failing statement pauses with the error; stepping skips it (break on error)', async () => {
    const leaseId = await openLease();
    const s = await startDebug(leaseId, 'select 1;\nselect * from nowhere;\nselect 3;');
    const err = await ipc.debugResume(s.debugId);
    expect(pauseReason(err)).toBe('error');
    expect(err.transaction.status).toBe('failed');
    expect(err.inlineValues.find((i) => i.kind === 'error')).toBeTruthy();
    const next = await ipc.debugStep(s.debugId, { kind: 'stepOver' });
    // the aborted transaction rejects the following statement too
    expect(pauseReason(next)).toBe('error');
    await ipc.debugStop(s.debugId, 'rollback');
  });

  it('commands on unknown or ended sessions', async () => {
    await expect(ipc.debugResume('nope')).rejects.toMatchObject({ kind: 'notFound' });
    const leaseId = await openLease();
    const s = await startDebug(leaseId, 'select 1;');
    await expect(startDebug(leaseId, 'select 2;')).rejects.toMatchObject({ kind: 'busy' });
    await ipc.debugStop(s.debugId, 'rollback');
    await expect(ipc.debugResume(s.debugId)).rejects.toMatchObject({ kind: 'invalid' });
  });
});
